//! VPS-режим: у сервера белый IP, пробивать ничего не нужно.
//!
//! Клиент заранее знает `ip:порт знакомства` сервера. Никаких STUN, MQTT и окон пробива: клиент
//! спрашивает, сервер отвечает «твой слот k — мой порт P, сессия S», клиент идёт на этот порт.
//!
//! **Порт знакомства** — без состояния: на каждый подписанный запрос клиента по слоту k (его
//! запись `Rendezvous` с сессией) сервер отвечает текущей записью своего слота k. Запись у слота
//! есть всегда — с момента, как он занял порт, — и не снимается никогда, только заменяется новой.
//! Поэтому сервер не может «замолчать».
//!
//! Стейт-машина **слота сервера** (инициатор всего — клиент):
//!
//! ```text
//!   Listening: новый случайный порт P из диапазона и сессия S; запись (P, S) — в таблицу порта
//!   знакомства ──────────────────────────────────────────────────────────────────────────┐
//!        │ пришла сессия клиента C (не та, с которой была прошлая дыра)                   │
//!        ▼                                                                                │
//!   Accepting(C): ждём на P первый подписанный пакет клиента; пришла сессия новее — ждём   │
//!   уже её (без тайм-аута: клиента, который пропал, ждать не вредно)                      │
//!        │                                                                                │
//!   Connected: дыра в реестре, пока не потеряна, не помечена плохой (`move_slot`) или      │
//!   клиент не пришёл с новой сессией (перезапустился) → новая регистрация ────────────────┘
//! ```
//!
//! Стейт-машина **слота клиента**:
//!
//! ```text
//!   Asking: новый локальный сокет и сессия C; раз в секунду шлём запрос на порт знакомства ─┐
//!        │ ответ сервера (P, S)                                                            │
//!        ▼                                                                                 │
//!   Connecting(P, S): стучимся на P (запросы продолжаются); ответ сменился — идём на новый   │
//!        │                                                                                 │
//!   Connected: дыра в реестре, пока не потеряна или не помечена плохой → новая сессия ───────┘
//! ```
//!
//! Все слоты знакомятся через порт знакомства напрямую (виртуал-брокер P2P здесь не нужен).
//! Обмен на нём — обычный `PeerMessage::Lite` с `Rendezvous` внутри (номер слота — в записи),
//! подписанный и замаскированный ключами знакомства из секрета пары
//! (`auth::PairSecret::bootstrap_keys`); сама запись тоже подписана. Без полного GUID обеих
//! сторон такой пакет не подделать.

use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use uuid::Uuid;

use crate::auth::{PairSecret, RecvKeys, SendKeys};
use crate::codec;
use crate::multilink::{AbortOnDrop, SlotBase, SlotPhase, TARGET_LINKS};
use crate::proto::{lite, peer_message, Lite, PeerMessage, Rendezvous};
use crate::punch::{self, PeerIdentity, PunchConfig};
use crate::rendezvous::{self, PeerSession};

/// Порт знакомства по умолчанию и диапазон портов слотов (10 000 портов вместе с ним).
pub const DEFAULT_BOOTSTRAP_PORT: u16 = 40000;
pub const DEFAULT_SLOT_PORTS: RangeInclusive<u16> = 40001..=49999;

/// Как часто клиент повторяет запрос, пока слот не залинкован.
const ASK_INTERVAL: Duration = Duration::from_secs(1);

/// Сколько раз пробуем занять случайный порт из диапазона, прежде чем сдаться.
const BIND_ATTEMPTS: usize = 64;

/// Адрес VPS-сервера: `ip:порт` или просто `ip` (тогда порт знакомства по умолчанию).
pub fn parse_server(value: &str) -> Option<SocketAddr> {
    let value = value.trim();
    value.parse().ok().or_else(|| Some(SocketAddr::new(value.parse().ok()?, DEFAULT_BOOTSTRAP_PORT)))
}

/// Общее для обеих сторон: кто мы, кто пир, секрет пары.
#[derive(Clone)]
pub(crate) struct Pair {
    pub my_peer_id: Uuid,
    pub peer_id: Uuid,
    pub secret: PairSecret,
}

impl Pair {
    fn identity(&self, slot: u8, my_session: Uuid, peer_session: Uuid) -> PeerIdentity {
        PeerIdentity {
            session_id: my_session,
            peer_session_id: peer_session,
            my_peer_id: self.my_peer_id,
            peer_id: self.peer_id,
            slot,
            pair: self.secret.clone(),
        }
    }

    /// Наша подписанная запись слота.
    fn record(&self, slot: u8, session: Uuid, endpoint: SocketAddr) -> Rendezvous {
        rendezvous::our_record(&self.secret, self.my_peer_id, slot, session, &[endpoint], 0)
    }
}

fn wrap(record: Rendezvous) -> PeerMessage {
    let slot = record.slot;
    PeerMessage { body: Some(peer_message::Body::Lite(Lite { slot, payload: Some(lite::Payload::Rendezvous(record)), pid: None })) }
}

/// Достаёт запись слота пира (подпись пакета и записи проверены, слот в пределах набора); всё
/// остальное — `None`.
fn unwrap(data: &[u8], keys: &mut RecvKeys, pair: &Pair) -> Option<PeerSession> {
    let msg = codec::decode(data.to_vec(), keys)?;
    let Some(peer_message::Body::Lite(Lite { payload: Some(lite::Payload::Rendezvous(r)), .. })) = msg.body else {
        return None;
    };
    let session = rendezvous::peer_session_from(&r, &pair.secret, pair.peer_id).ok()?;
    (session.slot < TARGET_LINKS).then_some(session)
}

/// Случайный свободный порт из диапазона (кроме `exclude`).
pub async fn bind_random_port(ports: &RangeInclusive<u16>, exclude: u16) -> Result<UdpSocket> {
    let (low, high) = (*ports.start(), *ports.end());
    anyhow::ensure!(low <= high, "пустой диапазон портов {low}..={high}");
    let span = u64::from(high - low) + 1;
    for _ in 0..BIND_ATTEMPTS {
        let random = u64::from_le_bytes(Uuid::new_v4().into_bytes()[..8].try_into().expect("8 байт"));
        let port = low + (random % span) as u16;
        if port == exclude {
            continue;
        }
        if let Ok(socket) = UdpSocket::bind(("0.0.0.0", port)).await {
            return Ok(socket);
        }
    }
    anyhow::bail!("не нашёл свободный порт в {low}..={high} за {BIND_ATTEMPTS} попыток")
}

// ---------------------------------------------------------------------------------------------
// Сервер
// ---------------------------------------------------------------------------------------------

/// Настройки сервера.
pub(crate) struct ServerConfig {
    pub public_ip: IpAddr,
    pub bootstrap_port: u16,
    pub ports: RangeInclusive<u16>,
    pub pair: Pair,
}

/// Занимает порт знакомства и запускает слоты сервера.
pub(crate) async fn start_server(label: &crate::label::Label, config: ServerConfig, bases: Vec<SlotBase>) -> Result<Vec<AbortOnDrop>> {
    let socket = UdpSocket::bind(("0.0.0.0", config.bootstrap_port))
        .await
        .with_context(|| format!("не удалось занять порт знакомства {}", config.bootstrap_port))?;
    log::info!("{label}VPS-сервер: порт знакомства {}", config.bootstrap_port);

    // Таблица порта знакомства: текущая запись каждого слота. Пишет только сам слот.
    let offers: Arc<Mutex<Vec<Option<Rendezvous>>>> = Arc::new(Mutex::new(vec![None; TARGET_LINKS as usize]));
    let mut clients = Vec::new();
    let mut tasks = Vec::new();
    for base in bases {
        // Последняя сессия клиента для этого слота (с порта знакомства).
        let (client_tx, client_rx) = watch::channel(None);
        clients.push(client_tx);
        let slot = ServerSlot {
            public_ip: config.public_ip,
            ports: config.ports.clone(),
            bootstrap_port: config.bootstrap_port,
            pair: config.pair.clone(),
            offers: offers.clone(),
            client_rx,
            base,
        };
        tasks.push(AbortOnDrop(tokio::spawn(slot.run())));
    }
    tasks.push(AbortOnDrop(tokio::spawn(serve_bootstrap(socket, config.pair, offers, clients))));
    Ok(tasks)
}

/// Порт знакомства: запрос клиента по слоту k → его сессию слоту k, в ответ — запись слота k.
async fn serve_bootstrap(
    socket: UdpSocket,
    pair: Pair,
    offers: Arc<Mutex<Vec<Option<Rendezvous>>>>,
    clients: Vec<watch::Sender<Option<Uuid>>>,
) {
    let (send, mut recv) = pair.secret.bootstrap_keys();
    let mut buf = [0u8; 1500];
    loop {
        let Ok((n, from)) = socket.recv_from(&mut buf).await else { continue };
        let Some(request) = unwrap(&buf[..n], &mut recv, &pair) else { continue };
        let slot = request.slot as usize;
        clients[slot].send_if_modified(|current| {
            let changed = *current != Some(request.session_id);
            if changed {
                log::debug!("знакомство: слот {slot}, клиент {from}, сессия {}", request.session_id);
                *current = Some(request.session_id);
            }
            changed
        });
        let offer = offers.lock().unwrap()[slot].clone();
        if let Some(offer) = offer {
            let _ = socket.send_to(&codec::encode(&wrap(offer), &send), from).await;
        }
    }
}

/// Рабочая задача одного слота сервера.
struct ServerSlot {
    public_ip: IpAddr,
    ports: RangeInclusive<u16>,
    bootstrap_port: u16,
    pair: Pair,
    offers: Arc<Mutex<Vec<Option<Rendezvous>>>>,
    client_rx: watch::Receiver<Option<Uuid>>,
    base: SlotBase,
}

impl ServerSlot {
    async fn run(mut self) {
        let (slot, label) = (self.base.slot, self.base.label.clone());
        // Сессия клиента, с которой была последняя дыра: её повторный запрос — не повод
        // принимать заново (клиент после потери дыры приходит с новой сессией).
        let mut last_linked: Option<Uuid> = None;
        loop {
            // Listening: новый порт и сессия, запись сразу в таблице порта знакомства.
            self.base.phase(SlotPhase::Rendezvous);
            let socket = match bind_random_port(&self.ports, self.bootstrap_port).await {
                Ok(socket) => Arc::new(socket),
                Err(e) => {
                    log::warn!("{label}слот {slot}: не удалось занять порт: {e:#}; повтор");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            let port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
            let my_session = Uuid::new_v4();
            self.offers.lock().unwrap()[slot as usize] = Some(self.pair.record(slot, my_session, SocketAddr::new(self.public_ip, port)));
            log::debug!("{label}слот {slot}: порт {port}");

            // Accepting: ждём пакет клиента на порту; сессия клиента сменилась — ждём уже её.
            let link = 'accept: loop {
                let client = tokio::select! {
                    client = next_client(&mut self.client_rx, last_linked) => client,
                    _ = self.base.redrop_rx.recv() => break 'accept None,
                };
                let Some(client) = client else { return };
                log::info!("{label}слот {slot}: ждём клиента на порту {port}");
                self.base.phase(SlotPhase::Punching);
                let attempt = punch::establish(
                    socket.clone(),
                    port,
                    Vec::new(), // пассивно: отвечаем туда, откуда придёт клиент
                    self.pair.identity(slot, my_session, client),
                    PunchConfig::default(),
                    self.base.events.clone(),
                );
                tokio::select! {
                    result = attempt => match result {
                        Ok(link) => break 'accept Some((link, client)),
                        Err(e) => {
                            log::warn!("{label}слот {slot}: приём не удался: {e}");
                            break 'accept None;
                        }
                    },
                    _ = client_changed(&mut self.client_rx, client) => continue 'accept,
                    _ = self.base.redrop_rx.recv() => break 'accept None,
                }
            };
            let Some((link, client)) = link else { continue };

            // Connected: держим, пока жива; клиент пришёл с новой сессией — значит, он эту дыру
            // уже бросил (перезапустился), переходим на новую регистрацию сразу.
            last_linked = Some(client);
            let mut client_rx = self.client_rx.clone();
            self.base.hold(link, async move { client_changed(&mut client_rx, client).await }).await;
        }
    }
}

/// Ждёт сессию клиента, отличную от `skip`; `None` — порт знакомства остановлен.
async fn next_client(rx: &mut watch::Receiver<Option<Uuid>>, skip: Option<Uuid>) -> Option<Uuid> {
    let value = rx.wait_for(|s| s.is_some() && *s != skip).await.ok()?;
    *value
}

/// Завершается, когда у клиента появилась сессия, отличная от `current`.
async fn client_changed(rx: &mut watch::Receiver<Option<Uuid>>, current: Uuid) {
    if rx.wait_for(|s| s.is_some_and(|s| s != current)).await.is_err() {
        std::future::pending::<()>().await;
    }
}

// ---------------------------------------------------------------------------------------------
// Клиент
// ---------------------------------------------------------------------------------------------

/// Настройки клиента.
pub(crate) struct ClientConfig {
    pub server: SocketAddr,
    pub pair: Pair,
    pub bind_ip: IpAddr,
    pub bind_ifindex: Option<u32>,
}

/// Сокет знакомства клиента: запросы серверу и разбор ответов по слотам.
struct ClientBootstrap {
    socket: UdpSocket,
    server: SocketAddr,
    send: SendKeys,
}

impl ClientBootstrap {
    /// Шлёт запрос раз в `ASK_INTERVAL`, пока хэндл жив.
    fn ask(self: &Arc<Self>, record: Rendezvous) -> AbortOnDrop {
        let this = self.clone();
        AbortOnDrop(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(ASK_INTERVAL);
            loop {
                ticker.tick().await;
                // Каждый раз заново: у каждого пакета свой счётчик в подписи.
                let packet = codec::encode(&wrap(record.clone()), &this.send);
                let _ = this.socket.send_to(&packet, this.server).await;
            }
        }))
    }
}

/// Принимает ответы сервера и раскладывает их по слотам (последний ответ слота — в его `watch`).
async fn receive_offers(boot: Arc<ClientBootstrap>, mut recv: RecvKeys, pair: Pair, offers: Vec<watch::Sender<Option<PeerSession>>>) {
    let mut buf = [0u8; 1500];
    loop {
        let Ok((n, _)) = boot.socket.recv_from(&mut buf).await else { continue };
        let Some(offer) = unwrap(&buf[..n], &mut recv, &pair) else { continue };
        offers[offer.slot as usize].send_if_modified(|current| {
            let changed = current.as_ref() != Some(&offer);
            if changed {
                log::debug!("знакомство: слот {}, сервер предлагает {} (сессия {})", offer.slot, offer.addr, offer.session_id);
                *current = Some(offer.clone());
            }
            changed
        });
    }
}

/// Сокет знакомства и слоты клиента.
pub(crate) async fn start_client(label: &crate::label::Label, config: ClientConfig, bases: Vec<SlotBase>) -> Result<Vec<AbortOnDrop>> {
    let socket = crate::bind::udp(config.bind_ip, 0, config.bind_ifindex).await.context("сокет знакомства")?;
    let (send, recv) = config.pair.secret.bootstrap_keys();
    let boot = Arc::new(ClientBootstrap { socket, server: config.server, send });
    log::info!("{label}VPS-клиент: сервер {}", config.server);

    let mut offers = Vec::new();
    let mut tasks = Vec::new();
    for base in bases {
        let (offer_tx, offer_rx) = watch::channel(None);
        offers.push(offer_tx);
        let slot = ClientSlot {
            boot: boot.clone(),
            pair: config.pair.clone(),
            bind_ip: config.bind_ip,
            bind_ifindex: config.bind_ifindex,
            offer_rx,
            base,
        };
        tasks.push(AbortOnDrop(tokio::spawn(slot.run())));
    }
    tasks.push(AbortOnDrop(tokio::spawn(receive_offers(boot, recv, config.pair, offers))));
    Ok(tasks)
}

/// Рабочая задача одного слота клиента.
struct ClientSlot {
    boot: Arc<ClientBootstrap>,
    pair: Pair,
    bind_ip: IpAddr,
    bind_ifindex: Option<u32>,
    offer_rx: watch::Receiver<Option<PeerSession>>,
    base: SlotBase,
}

impl ClientSlot {
    async fn run(mut self) {
        let (slot, label) = (self.base.slot, self.base.label.clone());
        loop {
            // Asking: новый сокет (новый локальный адрес — на случай, если плохой была
            // именно эта пара портов) и новая сессия; спрашиваем, пока не залинкуемся.
            self.base.phase(SlotPhase::Rendezvous);
            let socket = match crate::bind::udp(self.bind_ip, 0, self.bind_ifindex).await {
                Ok(socket) => Arc::new(socket),
                Err(e) => {
                    log::warn!("{label}слот {slot}: сокет слота: {e}; повтор");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            let port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
            let my_session = Uuid::new_v4();
            let asking = self.boot.ask(self.pair.record(slot, my_session, SocketAddr::from(([0, 0, 0, 0], port))));

            // Connecting: идём на порт из последнего ответа; ответ сменился — на новый. Ответ,
            // оставшийся от прошлой регистрации, тоже годится: сервер, получив нашу новую сессию,
            // либо примет её на том же порту, либо выдаст новый — и мы на него перейдём.
            let link = 'connect: loop {
                let offer = tokio::select! {
                    offer = self.offer_rx.wait_for(Option::is_some) => match offer {
                        Ok(offer) => offer.clone().expect("проверено"),
                        Err(_) => return,
                    },
                    _ = self.base.redrop_rx.recv() => break 'connect None,
                };
                self.offer_rx.mark_unchanged();
                log::info!("{label}слот {slot}: идём на порт сервера {}", offer.addr);
                self.base.phase(SlotPhase::Punching);
                // Сокет говорит только с этим портом сервера: ядру не искать маршрут на каждый
                // пакет, чужие адреса отсекаются в ядре.
                if let Err(e) = socket.connect(offer.addr).await {
                    log::warn!("{label}слот {slot}: connect к {}: {e}", offer.addr);
                }
                let attempt = punch::establish(
                    socket.clone(),
                    port,
                    vec![offer.addr],
                    self.pair.identity(slot, my_session, offer.session_id),
                    PunchConfig { margin: 0, ..PunchConfig::default() },
                    self.base.events.clone(),
                );
                tokio::select! {
                    result = attempt => match result {
                        Ok(link) => break 'connect Some(link),
                        Err(e) => {
                            log::warn!("{label}слот {slot}: подключение не удалось: {e}");
                            break 'connect None;
                        }
                    },
                    changed = self.offer_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        continue 'connect;
                    }
                    _ = self.base.redrop_rx.recv() => break 'connect None,
                }
            };
            drop(asking);
            let Some(link) = link else { continue };
            self.base.hold(link, std::future::pending()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_address_with_or_without_port() {
        assert_eq!(parse_server("203.0.113.10"), Some("203.0.113.10:40000".parse().unwrap()));
        assert_eq!(parse_server("203.0.113.10:41000"), Some("203.0.113.10:41000".parse().unwrap()));
        assert_eq!(parse_server("vps"), None);
    }

    fn pair(me: Uuid, peer: Uuid) -> Pair {
        Pair { my_peer_id: me, peer_id: peer, secret: PairSecret::new(me, peer) }
    }

    #[test]
    fn only_signed_records_of_the_expected_peer_within_the_set_are_accepted() {
        let (me, peer) = (Uuid::new_v4(), Uuid::new_v4());
        let mine = pair(me, peer);
        let theirs = pair(peer, me);
        let (send, mut recv) = theirs.secret.bootstrap_keys();
        let endpoint = SocketAddr::from(([203, 0, 113, 10], 41234));
        let packet = |p: &Pair, slot: u8| codec::encode(&wrap(p.record(slot, Uuid::new_v4(), endpoint)), &send);

        let ok = unwrap(&packet(&theirs, 3), &mut recv, &mine).expect("запись пира");
        assert_eq!((ok.slot, ok.addr), (3, endpoint));
        assert!(unwrap(&packet(&mine, 0), &mut recv, &mine).is_none(), "своя запись (эхо)");
        assert!(unwrap(&packet(&theirs, TARGET_LINKS), &mut recv, &mine).is_none(), "слот вне набора");

        let stranger = pair(peer, Uuid::new_v4());
        assert!(unwrap(&packet(&stranger, 0), &mut recv, &mine).is_none(), "запись подписана чужим");
        let (foreign_send, _) = stranger.secret.bootstrap_keys();
        let foreign = codec::encode(&wrap(theirs.record(0, Uuid::new_v4(), endpoint)), &foreign_send);
        assert!(unwrap(&foreign, &mut recv, &mine).is_none(), "пакет чужих ключей знакомства");
    }

    #[tokio::test]
    async fn random_ports_stay_in_range_and_skip_the_excluded_one() {
        let range = 45000..=45003;
        let mut held = Vec::new();
        for _ in 0..3 {
            let socket = bind_random_port(&range, 45001).await.unwrap();
            let port = socket.local_addr().unwrap().port();
            assert!(range.contains(&port) && port != 45001);
            held.push(socket);
        }
        assert!(bind_random_port(&range, 45001).await.is_err(), "свободных портов не осталось");
    }

    #[tokio::test]
    async fn client_session_waits_skip_the_linked_one_and_see_changes() {
        let (tx, mut rx) = watch::channel(None);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        tx.send(Some(a)).unwrap();
        assert_eq!(next_client(&mut rx, None).await, Some(a));
        let wait = tokio::time::timeout(Duration::from_millis(50), next_client(&mut rx, Some(a))).await;
        assert!(wait.is_err(), "сессию прошлой дыры не принимаем");
        tx.send(Some(b)).unwrap();
        assert_eq!(next_client(&mut rx, Some(a)).await, Some(b));
        tokio::time::timeout(Duration::from_millis(50), client_changed(&mut rx, a)).await.expect("сессия сменилась");
    }
}
