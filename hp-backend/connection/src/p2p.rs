//! P2P-режим: оба пира за NAT, знакомство через STUN + MQTT, дыры пробиваются.
//!
//! Стейт-машина слота k (у каждого слота свой сокет на всё время жизни набора):
//!
//! ```text
//!        ┌──────────────────────────────────────────────────────────────┐
//!        ▼                                                              │
//!   Starting: новая сессия, STUN → свои внешние адреса                  │
//!        │                                                              │
//!   анонс: слот 0 — запись в MQTT, слоты 1..9 — та же запись по живым   │
//!   дырам (виртуал-брокер); держится, пока слот не залинкован           │
//!        │                                                              │
//!   Rendezvous: ждём запись пира для слота k (не ту, с которой уже был  │
//!   линк)                                                               │
//!        │                                                              │
//!   Punching: пробив к его адресам, окно PUNCH_WINDOW; пришла запись    │
//!   новее — пробиваем к ней; окно вышло — всё заново ──────────────────┤
//!        │                                                              │
//!   Connected: дыра в реестре, пока не потеряна или не помечена плохой ─┘
//! ```
//!
//! Слот k пробивается только к слоту k пира — стороны сходятся без N×N. Записи пира приходят
//! из MQTT (слот 0) и по дырам (`Rendezvous` в `Lite`, остальные слоты), их раскладывает по
//! слотам `demux`.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::auth::PairSecret;
use crate::label::Label;
use crate::multilink::{AbortOnDrop, LinkRegistry, SlotBase, SlotPhase, TARGET_LINKS};
use crate::port_utils;
use crate::proto::Rendezvous;
use crate::punch::{self, PeerIdentity, PunchConfig};
use crate::rendezvous::{self, PeerSession, Registrar};
use crate::stun;

/// Слот, о котором договариваемся через MQTT; остальные — через виртуал-брокер.
const BOOTSTRAP_SLOT: crate::multilink::SlotId = 0;

/// Окно пробива одной попытки. Длиннее TTL регистрации (60 c) — с перекрытием.
const PUNCH_WINDOW: Duration = Duration::from_secs(80);

/// Как часто обновляем регистрацию bootstrap-слота в MQTT, пока не залинкован.
const REPUBLISH_INTERVAL: Duration = Duration::from_secs(30);

/// Как часто небутстрап-слот шлёт свой `Rendezvous` по живым дырам, пока не залинкован
/// (виртуал-брокер).
const HOLE_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(3);

/// Настройки P2P-набора.
pub(crate) struct Config {
    pub stun_addrs: Vec<SocketAddr>,
    pub mqtt_addr: SocketAddr,
    pub mqtt_ca_pem: Vec<u8>,
    pub my_peer_id: Uuid,
    pub peer_id: Uuid,
    pub pair: PairSecret,
    pub bind_ip: IpAddr,
    pub bind_ifindex: Option<u32>,
    /// Первый локальный порт слотов (`MultiLinkOptions::local_port_base`), 0 — от ОС.
    pub local_port_base: u16,
}

/// Запущенный P2P-режим: задачи слотов и приёмник записей пира, пришедших по дырам.
pub(crate) struct Started {
    pub tasks: Vec<AbortOnDrop>,
    /// Сюда `control_loop` отдаёт `Rendezvous` пира, пришедшие по дырам (виртуал-брокер).
    pub hole_records: mpsc::Sender<Rendezvous>,
    pub registrar: Arc<Registrar>,
}

/// Подключается к MQTT, создаёт сокеты слотов и запускает их рабочие задачи.
pub(crate) async fn start(label: &Label, config: Config, bases: Vec<SlotBase>) -> Result<Started> {
    let (registrar, mqtt_rx) =
        rendezvous::connect(label.clone(), config.mqtt_addr, config.mqtt_ca_pem.clone(), config.my_peer_id, config.peer_id)
            .await
            .context("не удалось подключиться к MQTT-брокеру")?;
    let registrar = Arc::new(registrar);

    let mut tasks = Vec::new();
    let mut slot_txs = Vec::new();
    for base in bases {
        let port = slot_port(config.local_port_base, base.slot);
        let socket = crate::bind::udp(config.bind_ip, port, config.bind_ifindex)
            .await
            .with_context(|| format!("не удалось создать сокет для слота {}", base.slot))?;
        let (slot_tx, peer_rx) = mpsc::channel::<PeerSession>(8);
        slot_txs.push(slot_tx);
        let announce = if base.slot == BOOTSTRAP_SLOT {
            Announce::Mqtt(registrar.clone())
        } else {
            Announce::VirtualBroker(base.registry.clone())
        };
        let slot = Slot {
            socket: Arc::new(socket),
            stun_addrs: config.stun_addrs.clone(),
            my_peer_id: config.my_peer_id,
            peer_id: config.peer_id,
            pair: config.pair.clone(),
            announce,
            peer_rx,
            base,
        };
        tasks.push(AbortOnDrop(tokio::spawn(slot.run())));
    }

    let (hole_tx, hole_rx) = mpsc::channel::<Rendezvous>(16);
    tasks.push(AbortOnDrop(tokio::spawn(demux(mqtt_rx, hole_rx, slot_txs, config.pair, config.peer_id))));
    Ok(Started { tasks, hole_records: hole_tx, registrar })
}

/// Локальный порт слота: `base + slot`, или 0 (выберет ОС), если `base` не задан.
fn slot_port(base: u16, slot: crate::multilink::SlotId) -> u16 {
    if base == 0 { 0 } else { base.saturating_add(u16::try_from(slot).unwrap_or(u16::MAX)) }
}

/// Раскладывает записи пира по слотам: из MQTT (слот 0) и пришедшие по дырам (остальные;
/// подпись и имя проверяются здесь).
async fn demux(
    mut mqtt: mpsc::Receiver<PeerSession>,
    mut holes: mpsc::Receiver<Rendezvous>,
    slot_txs: Vec<mpsc::Sender<PeerSession>>,
    pair: PairSecret,
    peer_id: Uuid,
) {
    loop {
        let session = tokio::select! {
            Some(session) = mqtt.recv() => session,
            Some(record) = holes.recv() => match rendezvous::peer_session_from(&record, &pair, peer_id) {
                Ok(session) => session,
                Err(e) => {
                    log::warn!("некорректный Rendezvous по дыре: {e:#}");
                    continue;
                }
            },
            else => return,
        };
        match slot_txs.get(session.slot as usize) {
            Some(tx) => {
                let _ = tx.send(session).await;
            }
            None => log::warn!("запись пира на неизвестный слот {}", session.slot),
        }
    }
}

/// Как слот анонсирует себя пиру.
enum Announce {
    /// Bootstrap-слот: публикуем `Rendezvous` в MQTT.
    Mqtt(Arc<Registrar>),
    /// Остальные: тот же `Rendezvous` шлём напрямую по живым дырам (виртуал-брокер).
    VirtualBroker(Arc<Mutex<LinkRegistry>>),
}

/// Рабочая задача одного P2P-слота.
struct Slot {
    socket: Arc<UdpSocket>,
    stun_addrs: Vec<SocketAddr>,
    my_peer_id: Uuid,
    peer_id: Uuid,
    pair: PairSecret,
    announce: Announce,
    peer_rx: mpsc::Receiver<PeerSession>,
    base: SlotBase,
}

impl Slot {
    async fn run(mut self) {
        let (slot, label) = (self.base.slot, self.base.label.clone());
        let mut last_linked_peer_session: Option<Uuid> = None;
        loop {
            // Каждая регистрация — новая сессия, а с ней новые вектор и ключи подписи.
            let my_session = Uuid::new_v4();
            self.base.phase(SlotPhase::Starting);

            let my_endpoints = match observe_endpoints(&self.socket, &self.stun_addrs).await {
                Ok(endpoints) => endpoints,
                Err(e) => {
                    log::warn!("{label}слот {slot}: STUN не удался: {e:#}; повтор");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            // Анонс держим, пока не залинкуемся (дроп хэндла его останавливает).
            let announce = match self.announce(my_session, &my_endpoints).await {
                Ok(guard) => guard,
                Err(e) => {
                    log::warn!("{label}слот {slot}: анонс не удался: {e:#}; повтор");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };

            self.base.phase(SlotPhase::Rendezvous);
            let Some(mut peer) = wait_fresh_peer(&mut self.peer_rx, last_linked_peer_session).await else { return };
            let my_port = my_endpoints[0].port();

            // Пробиваем к последней записи пира. Пришла новая (пир перезапустился, сменил сеть) —
            // бросаем текущий пробив и начинаем к ней: к старой сессии не пройдёт ни один пакет
            // (подпись другая). Свою сессию не меняем, чтобы стороны не гоняли друг друга новыми
            // записями. Окно одно на регистрацию: переключения его не продлевают.
            let window = tokio::time::sleep(PUNCH_WINDOW);
            tokio::pin!(window);
            let result = 'punch: loop {
                let candidates = peer.candidates();
                let (low, high) = port_utils::sweep_bounds(my_port, peer.addr.port(), PunchConfig::default().margin);
                log::info!(
                    "{label}слот {slot}: пробив {low}..={high} на {} (STUN-порт пира {}){}",
                    peer.addr.ip(),
                    peer.addr.port(),
                    if candidates.len() > 1 { format!(", ещё адреса пира: {:?}", &candidates[1..]) } else { String::new() }
                );
                self.base.phase(SlotPhase::Punching);
                let attempt = punch::establish(
                    self.socket.clone(),
                    my_port,
                    candidates,
                    self.identity(my_session, peer.session_id),
                    PunchConfig::default(),
                    self.base.events.clone(),
                );
                tokio::pin!(attempt);
                loop {
                    tokio::select! {
                        result = &mut attempt => break 'punch Some(result),
                        () = &mut window => break 'punch None,
                        newer = self.peer_rx.recv() => {
                            let Some(newer) = newer else { return };
                            if newer.session_id == peer.session_id || Some(newer.session_id) == last_linked_peer_session {
                                continue;
                            }
                            log::info!("{label}слот {slot}: у пира новая запись, пробиваем к ней");
                            peer = latest_peer(newer, &mut self.peer_rx, last_linked_peer_session);
                            continue 'punch;
                        }
                    }
                }
            };
            let link = match result {
                Some(Ok(link)) => link,
                Some(Err(e)) => {
                    log::warn!("{label}слот {slot}: пробив не удался: {e}; новая попытка");
                    continue;
                }
                None => {
                    log::warn!("{label}слот {slot}: пробив не уложился в {PUNCH_WINDOW:?}; новая попытка");
                    continue;
                }
            };

            // Залинковались — анонс больше не нужен.
            drop(announce);
            last_linked_peer_session = Some(peer.session_id);
            self.base.hold(link, std::future::pending()).await;
        }
    }

    fn identity(&self, my_session: Uuid, peer_session: Uuid) -> PeerIdentity {
        PeerIdentity {
            session_id: my_session,
            peer_session_id: peer_session,
            my_peer_id: self.my_peer_id,
            peer_id: self.peer_id,
            slot: self.base.slot,
            pair: self.pair.clone(),
        }
    }

    /// Анонс слота; возвращает хэндл фоновой задачи анонса (дроп её останавливает).
    async fn announce(&self, session: Uuid, endpoints: &[SocketAddr]) -> Result<AbortOnDrop> {
        let slot = self.base.slot;
        match &self.announce {
            Announce::Mqtt(registrar) => {
                registrar.publish_slot(slot, session, endpoints).await.context("публикация в MQTT")?;
                Ok(spawn_mqtt_republish(registrar.clone(), slot, session, endpoints.to_vec()))
            }
            Announce::VirtualBroker(registry) => {
                // `registered_at_unix_ms` над дырой не используется.
                let record = rendezvous::our_record(&self.pair, self.my_peer_id, slot, session, endpoints, 0);
                Ok(spawn_hole_announce(registry.clone(), record))
            }
        }
    }
}

/// Периодически обновляет MQTT-регистрацию bootstrap-слота, пока хэндл жив.
fn spawn_mqtt_republish(registrar: Arc<Registrar>, slot: crate::multilink::SlotId, session_id: Uuid, endpoints: Vec<SocketAddr>) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REPUBLISH_INTERVAL);
        ticker.tick().await; // первый тик сразу — публикацию уже сделали снаружи
        loop {
            ticker.tick().await;
            if registrar.publish_slot(slot, session_id, &endpoints).await.is_err() {
                return;
            }
        }
    }))
}

/// Виртуал-брокер: периодически шлёт наш `Rendezvous` по всем живым дырам, пока хэндл жив.
/// Пока живых дыр нет — просто ждёт следующего тика.
fn spawn_hole_announce(registry: Arc<Mutex<LinkRegistry>>, rendezvous: Rendezvous) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HOLE_ANNOUNCE_INTERVAL);
        loop {
            ticker.tick().await;
            let links = registry.lock().unwrap().links();
            for link in links {
                link.sender.send_rendezvous(rendezvous.clone()).await;
            }
        }
    }))
}

/// Ждёт сессию пира по слоту, пропуская ту, по которой уже был линк.
async fn wait_fresh_peer(peer_rx: &mut mpsc::Receiver<PeerSession>, already_linked: Option<Uuid>) -> Option<PeerSession> {
    loop {
        let peer = peer_rx.recv().await?;
        if Some(peer.session_id) == already_linked {
            continue;
        }
        return Some(latest_peer(peer, peer_rx, already_linked));
    }
}

/// Самая свежая запись пира: `first` или пришедшие за ней, если они уже в очереди (пир мог
/// перезапуститься несколько раз, пока мы пробивались к старой записи).
fn latest_peer(first: PeerSession, peer_rx: &mut mpsc::Receiver<PeerSession>, already_linked: Option<Uuid>) -> PeerSession {
    let mut latest = first;
    while let Ok(newer) = peer_rx.try_recv() {
        if Some(newer.session_id) != already_linked {
            latest = newer;
        }
    }
    latest
}

/// Опрашивает все STUN-серверы с сокета слота. Возвращает увиденные адреса без повторов
/// (первый — от первого ответившего сервера). Не ответил ни один — ошибка. Если серверы видят
/// сокет по-разному, это пишется в лог: разные адреса — разные маршруты, разные порты — NAT с
/// зависимостью от адресата.
async fn observe_endpoints(socket: &UdpSocket, stun_addrs: &[SocketAddr]) -> Result<Vec<SocketAddr>> {
    let mut observed: Vec<(SocketAddr, SocketAddr)> = Vec::new();
    for &server in stun_addrs {
        for _ in 0..2 {
            match tokio::time::timeout(Duration::from_secs(3), stun::query(socket, server)).await {
                Ok(Ok(seen)) => {
                    log::debug!("STUN {server}: наш публичный адрес {seen}");
                    observed.push((server, seen));
                    break;
                }
                Ok(Err(e)) => log::debug!("STUN {server}: ошибка {e}"),
                Err(_) => log::debug!("STUN {server}: таймаут"),
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    if observed.is_empty() {
        anyhow::bail!("ни один STUN-сервер не ответил ({} шт.)", stun_addrs.len());
    }
    if let Some(hint) = stun::describe_nat(&observed) {
        log::info!("STUN: {hint}");
    }
    let mut endpoints: Vec<SocketAddr> = Vec::new();
    for (_, seen) in &observed {
        if !endpoints.contains(seen) {
            endpoints.push(*seen);
        }
    }
    Ok(endpoints)
}

const _: () = assert!(BOOTSTRAP_SLOT < TARGET_LINKS);

#[cfg(test)]
mod tests {
    use super::*;

    fn session(n: u8) -> PeerSession {
        PeerSession { slot: 0, session_id: Uuid::from_bytes([n; 16]), addr: SocketAddr::from(([203, 0, 113, n], 1000)), extra: vec![], registered_at_unix_ms: 0 }
    }

    /// Пир перезапускался, пока мы пробивались: берём последнюю запись, а не первую в очереди.
    #[tokio::test]
    async fn the_freshest_peer_record_wins() {
        let (tx, mut rx) = mpsc::channel(8);
        for n in [1, 2, 3] {
            tx.send(session(n)).await.unwrap();
        }
        assert_eq!(wait_fresh_peer(&mut rx, None).await.unwrap(), session(3));
        tx.send(session(4)).await.unwrap();
        tx.send(session(5)).await.unwrap();
        assert_eq!(latest_peer(session(9), &mut rx, Some(session(5).session_id)), session(4), "уже залинкованную сессию пропускаем");
    }

    #[test]
    fn slot_ports_follow_the_base_or_stay_random() {
        assert_eq!(slot_port(0, 3), 0);
        assert_eq!(slot_port(51410, 0), 51410);
        assert_eq!(slot_port(51410, 9), 51419);
    }
}
