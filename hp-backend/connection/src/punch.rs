//! Конечный автомат UDP hole punching для одной дыры (слота).
//!
//! С одного сокета с фиксированным source-портом перебираем порты назначения
//! на публичном IP пира, отправляя `Punch`: симметричный NAT может открыть
//! для каждого адресата свой внешний порт, отличный от увиденного STUN'ом.
//! Диапазон и порядок перебора — в `port_utils`. Чей пакет пройдёт первым
//! (мы получили `Punch`/`PunchAck` от пира или наш `Punch` подтвердили), тот
//! и считается живой дырой.
//!
//! Пробив шлёт `PeerMessage::Init` с полным GUID-заголовком (`receive_loop`
//! принимает `Init` только с ожидаемым `from_peer_id` — заодно отсекает
//! hairpin-эхо). После установки дыры идут `PeerMessage::Lite` (только slot).
//! Адрес отправителя не проверяется: любой пакет с нашим слотом, прошедший XOR-вектор
//! и protobuf, делает свой адрес текущим эндпоинтом пира (как роуминг в WireGuard),
//! а `LinkSender` шлёт всегда на текущий эндпоинт. `PunchAck` уходит туда, откуда
//! пришёл `Punch`.
//!
//! Keep-alive здесь НЕ запускается: `establish` возвращает `PeerLink` с
//! `LinkSender`, а слать keep-alive по всем дырам — забота одной общей таски
//! в менеджере.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::auth::{peer_name, PairSecret, RecvKeys, SendKeys, AUTH_LEN};
use crate::codec;
use crate::pool::{Packet, PACKET_CAP};
use crate::wire::{self, Fast};
use crate::link_id::PeerLinkId;
use crate::port_utils::{sweep_bounds, zigzag_ports};
use crate::proto::{
    DeleteLink, InitMessage, KeepAlive, Lite, PeerMessage, Punch, PunchAck, Rendezvous, Stats,
};
use crate::proto::{init_message, lite, peer_message};

/// Счётчики пакетов по одной дыре (для синхронизации статистики).
#[derive(Default)]
pub struct LinkStats {
    // 32 бита: на 32-битных MIPS (роутеры) 64-битных атомиков нет.
    pub sent: AtomicU32,
    pub received: AtomicU32,
}

impl LinkStats {
    pub fn snapshot(&self) -> (u64, u64) {
        (
            u64::from(self.sent.load(Ordering::Relaxed)),
            u64::from(self.received.load(Ordering::Relaxed)),
        )
    }
}

/// Статистика пира по одной его дыре (из пришедшего `Stats`).
#[derive(Clone, Copy, Debug)]
pub struct PeerLinkStat {
    pub slot: u8,
    pub sent: u64,
    pub received: u64,
}

/// События с дыры, которые `receive_loop` отдаёт менеджеру.
#[derive(Debug)]
pub enum LinkEvent {
    /// Пришла статистика пира по всем его дырам.
    PeerStats(Vec<PeerLinkStat>),
    /// Пир просит удалить линк по этому слоту (он пробивает дыру заново).
    DeleteLink { slot: u8 },
    /// Виртуал-брокер: пир прислал по этой дыре запись `Rendezvous` о своём
    /// другом слоте (то, что иначе ушло бы в MQTT).
    PeerRendezvous(Rendezvous),
    /// Пир прислал полезную нагрузку (`Data`) по дыре `slot` (буфер из банка).
    PeerData { slot: u8, payload: Packet },
    /// Пир прислал обёрнутый пакет (`WrappedData`) по дыре `slot`; `flow` — корзина TCP-потока
    /// (TUN-режим, `seq` тогда — номер в ней).
    PeerWrapped { slot: u8, seq: u64, client_id: u32, flow: Option<u32>, payload: Packet },
    /// Пир прислал IP-пакет с номером в потоке (`Ordered`, TUN-режим).
    PeerOrdered { slot: u8, flow: u32, seq: u64, payload: Packet },
}

#[derive(Clone, Debug)]
pub struct PunchConfig {
    /// Сколько лишних портов пробуем за пределами `[min(a, b), max(a, b)]`.
    pub margin: u16,
    pub punch_interval: Duration,
    /// Связь считается потерянной, если столько времени не было ни одного
    /// валидного пакета от пира (см. `PeerLink::lost`).
    pub keepalive_timeout: Duration,
}

impl Default for PunchConfig {
    fn default() -> Self {
        Self {
            margin: 1000,
            punch_interval: Duration::from_millis(50),
            keepalive_timeout: Duration::from_secs(15),
        }
    }
}

/// Идентичность одной дыры (слота): GUID сессий (наша и пира), GUID пиров, номер слота и
/// секрет пары, из которого выводятся XOR-векторы и ключи подписи (`auth`). Наружу в `Init`
/// уходят только имена пиров (первая группа GUID).
#[derive(Clone, Debug)]
pub struct PeerIdentity {
    pub session_id: Uuid,
    pub peer_session_id: Uuid,
    pub my_peer_id: Uuid,
    pub peer_id: Uuid,
    pub slot: u8,
    pub pair: PairSecret,
}

impl PeerIdentity {
    /// Стабильный идентификатор дыры, одинаковый на обеих сторонах.
    pub fn link_id(&self) -> PeerLinkId {
        PeerLinkId::new(self.session_id, self.peer_session_id)
    }

    /// Ключи этой дыры: (отправка пиру, приём от пира).
    pub fn keys(&self) -> (SendKeys, RecvKeys) {
        self.pair.link_keys(self.session_id, self.peer_session_id)
    }

    /// Конверт фазы пробива: сессия и имена пиров (полные GUID не публикуются).
    fn init(&self, payload: init_message::Payload) -> PeerMessage {
        PeerMessage {
            body: Some(peer_message::Body::Init(InitMessage {
                session_id: self.session_id.to_string(),
                from_peer_id: peer_name(&self.my_peer_id),
                to_peer_id: peer_name(&self.peer_id),
                slot: self.slot as u32,
                payload: Some(payload),
            })),
        }
    }

    /// Конверт после установки: только слот.
    fn lite(&self, payload: lite::Payload) -> PeerMessage {
        PeerMessage {
            body: Some(peer_message::Body::Lite(Lite {
                slot: self.slot as u32,
                payload: Some(payload),
            })),
        }
    }
}

/// Абортит задачу при дропе.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// То, что нужно, чтобы слать по дыре: сокет, адрес пира и идентичность.
/// Общая keep-alive-таска держит по одному такому на каждую живую дыру.
pub struct LinkSender {
    socket: Arc<UdpSocket>,
    /// Адрес, к которому сокет подключён (`connect`, режим `vps-client`): туда шлём `send` без
    /// адреса — ядро берёт закешированный маршрут вместо поиска на каждый `sendto`.
    connected: Option<SocketAddr>,
    /// Текущий эндпоинт пира: его обновляет `receive_loop` по каждому валидному пакету
    /// (у пира может быть несколько провайдеров/маршрутов, адрес отправителя меняется).
    endpoint: watch::Receiver<Option<SocketAddr>>,
    identity: PeerIdentity,
    /// Вектор пира и подпись: общие с пробивом этой дыры (один счётчик на направление).
    keys: Arc<SendKeys>,
    seq: AtomicU32,
    stats: Arc<LinkStats>,
}

impl LinkSender {
    /// Слот этой дыры.
    pub fn slot(&self) -> u8 {
        self.identity.slot
    }

    /// Снимок счётчиков (отправлено, получено) по этой дыре.
    pub fn stats(&self) -> (u64, u64) {
        self.stats.snapshot()
    }

    /// Адрес пира, с которого недавно приходили валидные пакеты; шлём именно туда.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        *self.endpoint.borrow()
    }

    async fn send_lite(&self, payload: lite::Payload) {
        let Some(peer_addr) = self.peer_addr() else { return };
        let msg = self.identity.lite(payload);
        let packet = codec::encode(&msg, &self.keys);
        if self.send_to_peer(&packet, peer_addr).await.is_ok() {
            self.stats.sent.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn send_to_peer(&self, packet: &[u8], peer_addr: SocketAddr) -> std::io::Result<usize> {
        if self.connected == Some(peer_addr) {
            self.socket.send(packet).await
        } else {
            self.socket.send_to(packet, peer_addr).await
        }
    }

    pub async fn send_keepalive(&self) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        log::trace!(
            "слот {}: keep-alive отправлен (seq={seq})",
            self.identity.slot
        );
        self.send_lite(lite::Payload::KeepAlive(KeepAlive { seq: u64::from(seq) }))
            .await;
    }

    /// Отправить полезную нагрузку (в перспективе — WireGuard) по этой дыре.
    /// Отправить полезную нагрузку (WireGuard) по этой дыре. Пакет собирается в буфере на
    /// стеке (`wire`), без выделения памяти.
    pub async fn send_data(&self, payload: &[u8]) {
        log::trace!("слот {}: отправлено {} байт данных", self.identity.slot, payload.len());
        let mut buf = [0u8; PACKET_CAP];
        let Some(n) = wire::encode_data(u32::from(self.identity.slot), payload, &self.keys, &mut buf) else {
            log::debug!("слот {}: {} байт не влезают в пакет", self.identity.slot, payload.len());
            return;
        };
        self.send_raw(&buf[..n]).await;
    }

    /// Отправить обёрнутый пакет (роутер ↔ сервер) по этой дыре; `flow` — корзина TCP-потока.
    pub async fn send_wrapped(&self, seq: u64, client_id: u32, flow: Option<u32>, payload: &[u8]) {
        log::trace!("слот {}: отправлен WrappedData seq={seq} ({} байт)", self.identity.slot, payload.len());
        let mut buf = [0u8; PACKET_CAP];
        let slot = u32::from(self.identity.slot);
        let Some(n) = wire::encode_wrapped(slot, seq, payload, client_id, flow, &self.keys, &mut buf) else {
            log::debug!("слот {}: {} байт не влезают в пакет", self.identity.slot, payload.len());
            return;
        };
        self.send_raw(&buf[..n]).await;
    }

    /// Отправить IP-пакет с номером в потоке (`Ordered`, TUN-режим) по этой дыре.
    pub async fn send_ordered(&self, flow: u32, seq: u64, payload: &[u8]) {
        let mut buf = [0u8; PACKET_CAP];
        let slot = u32::from(self.identity.slot);
        let Some(n) = wire::encode_ordered(slot, flow, seq, payload, &self.keys, &mut buf) else {
            log::debug!("слот {}: {} байт не влезают в пакет", self.identity.slot, payload.len());
            return;
        };
        self.send_raw(&buf[..n]).await;
    }

    async fn send_raw(&self, packet: &[u8]) {
        let Some(peer_addr) = self.peer_addr() else { return };
        if self.send_to_peer(packet, peer_addr).await.is_ok() {
            self.stats.sent.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Отправить нашу статистику по всем дырам.
    pub async fn send_stats(&self, links: Vec<crate::proto::LinkStat>) {
        self.send_lite(lite::Payload::Stats(Stats { links })).await;
    }

    /// Попросить пира удалить линк по слоту `slot`.
    pub async fn send_delete_link(&self, slot: u8) {
        log::debug!(
            "слот {}: отправлен DeleteLink слота {slot}",
            self.identity.slot
        );
        self.send_lite(lite::Payload::DeleteLink(DeleteLink { slot: slot as u32 }))
            .await;
    }

    /// Виртуал-брокер: отдать пиру запись `Rendezvous` о нашем слоте по этой
    /// дыре — то же, что ушло бы в MQTT.
    pub async fn send_rendezvous(&self, rendezvous: Rendezvous) {
        log::debug!(
            "слот {}: отправлен Rendezvous слота {}",
            self.identity.slot,
            rendezvous.slot
        );
        self.send_lite(lite::Payload::Rendezvous(rendezvous)).await;
    }
}

/// Живая дыра: пробита, keep-alive гоняет менеджер снаружи. Дроп
/// останавливает фоновую задачу приёма.
pub struct PeerLink {
    pub slot: u8,
    pub link_id: PeerLinkId,
    /// Адрес пира, с которого пришёл пакет, установивший дыру. Дальше эндпоинт может
    /// сменяться (см. `LinkSender::peer_addr`).
    pub peer_addr: SocketAddr,
    /// Локальный адрес нашего сокета (IP — `0.0.0.0`, порт — от ОС).
    pub local_addr: SocketAddr,
    pub sender: Arc<LinkSender>,
    last_seen: Arc<Mutex<Instant>>,
    keepalive_timeout: Duration,
    _receiver: AbortOnDrop,
}

impl PeerLink {
    /// Завершается, когда от пира не приходило ни одного валидного пакета
    /// `keepalive_timeout`.
    pub async fn lost(&self) {
        loop {
            let silent_for = self.last_seen.lock().unwrap().elapsed();
            if silent_for >= self.keepalive_timeout {
                return;
            }
            tokio::time::sleep(self.keepalive_timeout - silent_for).await;
        }
    }
}

/// Порядок отправки `Punch` по нескольким адресам пира: для каждого свой список
/// портов (зигзаг от центра), между адресами чередуем, чтобы ни один не ждал
/// очереди за длинным перебором другого.
fn sweep_order(candidates: &[SocketAddr], my_port: u16, margin: u16) -> Vec<SocketAddr> {
    let lists: Vec<Vec<SocketAddr>> = candidates
        .iter()
        .map(|candidate| {
            let (low, high) = sweep_bounds(my_port, candidate.port(), margin);
            zigzag_ports(candidate.port(), low, high)
                .into_iter()
                .map(|port| SocketAddr::new(candidate.ip(), port))
                .collect()
        })
        .collect();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    let mut order = Vec::with_capacity(lists.iter().map(Vec::len).sum());
    for step in 0..longest {
        for list in &lists {
            if let Some(dest) = list.get(step) {
                order.push(*dest);
            }
        }
    }
    order
}

/// Пробивает одну дыру и возвращает `PeerLink`. Keep-alive НЕ запускает.
/// `peer_addrs` — внешние адреса пира (основной первым; несколько, если разные
/// STUN-серверы видели его по-разному): стучимся по каждому, дырой считается тот,
/// с которого пришёл ответ. Пустой `peer_addrs` — пассивный режим (сервер с белым
/// адресом): никуда не стучимся, ждём первый валидный пакет пира и отвечаем туда,
/// откуда он пришёл. `events` — канал в менеджер: сюда `receive_loop`
/// шлёт статистику пира и команды `DeleteLink`.
pub async fn establish(
    socket: Arc<UdpSocket>,
    my_port: u16,
    peer_addrs: Vec<SocketAddr>,
    identity: PeerIdentity,
    config: PunchConfig,
    events: mpsc::Sender<LinkEvent>,
) -> io::Result<PeerLink> {
    let local_addr = socket.local_addr()?;
    let (found_tx, mut found_rx) = watch::channel::<Option<SocketAddr>>(None);

    let last_seen = Arc::new(Mutex::new(Instant::now()));
    let stats = Arc::new(LinkStats::default());
    let (send_keys, recv_keys) = identity.keys();
    let send_keys = Arc::new(send_keys);
    // Обе фоновые задачи держим под охраной: если `establish` отменят по
    // таймауту окна пробива, они остановятся вместе с ним, а не будут стучаться
    // и читать сокет дальше.
    let receiver = AbortOnDrop(tokio::spawn(receive_loop(
        socket.clone(),
        identity.clone(),
        (send_keys.clone(), recv_keys),
        found_tx,
        last_seen.clone(),
        stats.clone(),
        events,
    )));

    let sweep_socket = socket.clone();
    let destinations = sweep_order(&peer_addrs, my_port, config.margin);
    let punch_interval = config.punch_interval;
    let sweep_identity = identity.clone();
    let sweep_keys = send_keys.clone();
    let sweeper = AbortOnDrop(tokio::spawn(async move {
        if destinations.is_empty() {
            return;
        }
        loop {
            for &dest in &destinations {
                let msg = sweep_identity.init(init_message::Payload::Punch(Punch {
                    target_port: u32::from(dest.port()),
                }));
                let packet = codec::encode(&msg, &sweep_keys);
                let _ = sweep_socket.send_to(&packet, dest).await;
                tokio::time::sleep(punch_interval).await;
            }
        }
    }));

    let endpoint = found_rx.clone();
    let confirmed_peer = loop {
        if found_rx.changed().await.is_err() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "receive loop ended",
            ));
        }
        if let Some(addr) = *found_rx.borrow() {
            break addr;
        }
    };
    drop(sweeper);

    let link_id = identity.link_id();
    let sender = Arc::new(LinkSender {
        connected: socket.peer_addr().ok(),
        socket,
        endpoint,
        identity: identity.clone(),
        keys: send_keys,
        seq: AtomicU32::new(0),
        stats,
    });

    Ok(PeerLink {
        slot: identity.slot,
        link_id,
        peer_addr: confirmed_peer,
        local_addr,
        sender,
        last_seen,
        keepalive_timeout: config.keepalive_timeout,
        _receiver: receiver,
    })
}

/// Любой пакет с верной подписью (и слотом / именем пира) доказывает, что
/// пир достижим с адреса `from`: считаем этот адрес текущим эндпоинтом, даже если он
/// отличается от того, куда мы стучались или откуда пришёл прежний пакет.
fn note_packet(
    slot: u8,
    from: SocketAddr,
    endpoint: &watch::Sender<Option<SocketAddr>>,
    last_seen: &Mutex<Instant>,
    stats: &LinkStats,
) {
    *last_seen.lock().unwrap() = Instant::now();
    stats.received.fetch_add(1, Ordering::Relaxed);
    endpoint.send_if_modified(|current| {
        if *current == Some(from) {
            return false;
        }
        if let Some(old) = current {
            log::info!("слот {slot}: эндпоинт пира сменился {old} -> {from}");
        }
        *current = Some(from);
        true
    });
}

async fn receive_loop(
    socket: Arc<UdpSocket>,
    identity: PeerIdentity,
    (send_keys, mut recv_keys): (Arc<SendKeys>, RecvKeys),
    endpoint: watch::Sender<Option<SocketAddr>>,
    last_seen: Arc<Mutex<Instant>>,
    stats: Arc<LinkStats>,
    events: mpsc::Sender<LinkEvent>,
) {
    let expected_from = peer_name(&identity.peer_id);
    let mut buf = [0u8; 1500];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Маску снимаем на месте и проверяем подпись: чужой, подделанный или повторённый пакет
        // (скан, мусор, атака) отбрасываем, не глядя внутрь, — и эндпоинт по нему не меняется.
        // Пакет данных разбираем без выделения памяти (`wire`), остальное — prost'ом.
        codec::mask(&mut buf[..n], &recv_keys.xor);
        let Some(len) = recv_keys.opener.open(&buf[..n]) else {
            continue;
        };
        let message = &buf[AUTH_LEN..AUTH_LEN + len];
        if let Some(fast) = wire::parse(message) {
            let (lite_slot, event) = match fast {
                Fast::Data { slot, payload } => (slot, Packet::copy_from(payload).map(|payload| LinkEvent::PeerData { slot: identity.slot, payload })),
                Fast::Wrapped { slot, seq, payload, client_id, flow } => (
                    slot,
                    Packet::copy_from(payload).map(|payload| LinkEvent::PeerWrapped { slot: identity.slot, seq, client_id, flow, payload }),
                ),
                Fast::Ordered { slot, flow, seq, payload } => (
                    slot,
                    Packet::copy_from(payload).map(|payload| LinkEvent::PeerOrdered { slot: identity.slot, flow, seq, payload }),
                ),
            };
            if lite_slot != u32::from(identity.slot) {
                continue;
            }
            note_packet(identity.slot, from, &endpoint, &last_seen, &stats);
            if let Some(event) = event {
                let _ = events.send(event).await;
            }
            continue;
        }
        let Ok(msg) = codec::decode_unmasked(message) else {
            continue;
        };
        match msg.body {
            Some(peer_message::Body::Init(init)) => {
                // Пробив: подпись уже проверена; имя пира — для ясности (пакет, вернувшийся к
                // нам по hairpin, и так не пройдёт: подписан ключом другого направления).
                if init.from_peer_id != expected_from {
                    continue;
                }
                log::debug!(
                    "слот {}: {} от {from}",
                    identity.slot,
                    match init.payload {
                        Some(init_message::Payload::Punch(_)) => "получен Punch",
                        Some(init_message::Payload::PunchAck(_)) => "получен PunchAck",
                        None => "получен Init без нагрузки",
                    }
                );
                note_packet(identity.slot, from, &endpoint, &last_seen, &stats);
                // Ack уходит туда, откуда реально пришёл Punch (а не туда, куда стучались мы).
                if let Some(init_message::Payload::Punch(p)) = init.payload {
                    let ack = identity.init(init_message::Payload::PunchAck(PunchAck {
                        target_port: p.target_port,
                    }));
                    let _ = socket
                        .send_to(&codec::encode(&ack, &send_keys), from)
                        .await;
                }
            }
            Some(peer_message::Body::Lite(lite)) => {
                // После установки: достаточно нашего слота (подпись проверена выше). Адрес
                // отправителя не проверяем: он становится текущим эндпоинтом пира.
                if lite.slot != u32::from(identity.slot) {
                    continue;
                }
                note_packet(identity.slot, from, &endpoint, &last_seen, &stats);
                handle_lite(lite, identity.slot, &events).await;
            }
            None => {}
        }
    }
}

async fn handle_lite(lite: Lite, slot: u8, events: &mpsc::Sender<LinkEvent>) {
    match lite.payload {
        Some(lite::Payload::Stats(s)) => {
            log::debug!(
                "слот {slot}: получена статистика пира ({} дыр)",
                s.links.len()
            );
            let peer_stats = s
                .links
                .into_iter()
                .filter_map(|l| {
                    u8::try_from(l.slot).ok().map(|slot| PeerLinkStat {
                        slot,
                        sent: l.sent,
                        received: l.received,
                    })
                })
                .collect();
            let _ = events.send(LinkEvent::PeerStats(peer_stats)).await;
        }
        Some(lite::Payload::DeleteLink(d)) => {
            log::debug!("слот {slot}: получен DeleteLink слота {}", d.slot);
            if let Ok(slot) = u8::try_from(d.slot) {
                let _ = events.send(LinkEvent::DeleteLink { slot }).await;
            }
        }
        Some(lite::Payload::Rendezvous(r)) => {
            log::debug!("слот {slot}: получен Rendezvous слота {}", r.slot);
            let _ = events.send(LinkEvent::PeerRendezvous(r)).await;
        }
        // Обычно пакеты данных разбирает быстрый путь (`wire`); сюда они попадают, только если
        // закодированы необычно (например, с повторными полями).
        Some(lite::Payload::Data(d)) => {
            if let Some(payload) = Packet::copy_from(&d.payload) {
                let _ = events.send(LinkEvent::PeerData { slot, payload }).await;
            }
        }
        Some(lite::Payload::Wrapped(w)) => {
            if let Some(payload) = Packet::copy_from(&w.payload) {
                let _ = events.send(LinkEvent::PeerWrapped { slot, seq: w.seq, client_id: w.client_id, flow: w.flow, payload }).await;
            }
        }
        Some(lite::Payload::Ordered(o)) => {
            if let Some(payload) = Packet::copy_from(&o.payload) {
                let _ = events.send(LinkEvent::PeerOrdered { slot, flow: o.flow, seq: o.seq, payload }).await;
            }
        }
        // last_seen/received уже обновлены выше.
        Some(lite::Payload::KeepAlive(k)) => {
            log::trace!("слот {slot}: keep-alive получен (seq={})", k.seq);
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Data;
    use std::net::IpAddr;

    fn test_config() -> PunchConfig {
        PunchConfig {
            margin: 0,
            punch_interval: Duration::from_millis(1),
            keepalive_timeout: Duration::from_millis(200),
        }
    }

    /// Канал событий, который в тестах не нужен: тесты не шлют Stats/DeleteLink,
    /// а `receive_loop` относится к send'ам как best-effort.
    fn null_events() -> mpsc::Sender<LinkEvent> {
        mpsc::channel(1).0
    }

    fn identity(session_id: Uuid, peer_session_id: Uuid, me: Uuid, peer: Uuid) -> PeerIdentity {
        PeerIdentity { session_id, peer_session_id, my_peer_id: me, peer_id: peer, slot: 0, pair: PairSecret::new(me, peer) }
    }

    /// Ключи, которыми настоящий пир шлёт владельцу `ident` (счётчик со случайного места, чтобы
    /// не пересечься со счётчиком живой дыры пира в окне против повтора).
    fn keys_to(ident: &PeerIdentity) -> SendKeys {
        SendKeys {
            xor: ident.pair.xor_key(ident.session_id),
            sealer: crate::auth::Sealer::with_random_start(&ident.pair.link_key(ident.peer_session_id, ident.session_id)),
        }
    }

    /// Ключи самозванца: без секрета пары.
    fn foreign_keys() -> SendKeys {
        SendKeys { xor: codec::random_key(), sealer: crate::auth::Sealer::new(&codec::random_key().repeat(2).try_into().unwrap()) }
    }

    /// Гоняет keep-alive по дыре, пока хэндл жив (в проде это делает менеджер).
    fn drive_keepalive(sender: Arc<LinkSender>) -> AbortOnDrop {
        AbortOnDrop(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(20));
            loop {
                ticker.tick().await;
                sender.send_keepalive().await;
            }
        }))
    }

    /// Два пира на loopback: каждому сообщён точный порт другого, margin=0
    /// сводит перебор к одной попытке.
    #[tokio::test]
    async fn punch_over_loopback_and_link_id_matches() {
        let peer_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let a_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let b_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let a_port = a_socket.local_addr().unwrap().port();
        let b_port = b_socket.local_addr().unwrap().port();

        let (a_id, b_id) = (Uuid::new_v4(), Uuid::new_v4());
        let (a_sess, b_sess) = (Uuid::new_v4(), Uuid::new_v4());

        let (link_a, link_b) = tokio::join!(
            establish(
                a_socket,
                a_port,
                vec![SocketAddr::new(peer_ip, b_port)],
                identity(a_sess, b_sess, a_id, b_id),
                test_config(),
                null_events()
            ),
            establish(
                b_socket,
                b_port,
                vec![SocketAddr::new(peer_ip, a_port)],
                identity(b_sess, a_sess, b_id, a_id),
                test_config(),
                null_events()
            ),
        );
        let link_a = link_a.unwrap();
        let link_b = link_b.unwrap();

        assert_eq!(link_a.peer_addr, SocketAddr::new(peer_ip, b_port));
        assert_eq!(link_b.peer_addr, SocketAddr::new(peer_ip, a_port));
        assert_eq!(link_a.local_addr, SocketAddr::new(peer_ip, a_port));
        // Обе стороны считают один и тот же PeerLinkId.
        assert_eq!(link_a.link_id, link_b.link_id);
    }

    /// Порядок отправки `Punch` по нескольким адресам: чередуем, у каждого свой
    /// зигзаг портов.
    #[test]
    fn sweep_order_interleaves_candidates() {
        let a: SocketAddr = "203.0.113.1:1000".parse().unwrap();
        let b: SocketAddr = "198.51.100.1:2000".parse().unwrap();

        let single = sweep_order(&[a], 1000, 0);
        assert_eq!(single, vec![a]);

        let both = sweep_order(&[a, b], 1500, 2);
        assert_eq!(both[0], a, "основной адрес первым");
        assert_eq!(both[1], b);
        assert!(both.iter().filter(|d| d.ip() == a.ip()).count() > 1, "у каждого адреса свой перебор");
        assert!(both.iter().filter(|d| d.ip() == b.ip()).count() > 1);
        assert_eq!(both.iter().filter(|d| **d == a).count(), 1, "порт без повторов внутри адреса");
    }

    /// Если окно пробива истекло и `establish` отменили, его фоновые задачи
    /// (перебор портов, приём) должны остановиться, а не стучаться дальше.
    #[tokio::test]
    async fn cancelled_establish_stops_knocking() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let socket = Arc::new(UdpSocket::bind((ip, 0)).await.unwrap());
        let my_port = socket.local_addr().unwrap().port();
        let listener = UdpSocket::bind((ip, 0)).await.unwrap();
        let target = listener.local_addr().unwrap();

        let ident = identity(Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let attempt = establish(socket, my_port, vec![target], ident, test_config(), null_events());
        assert!(tokio::time::timeout(Duration::from_millis(150), attempt).await.is_err(), "пира нет, пробив не должен завершиться");

        // Всё, что успели отправить до отмены, вычитываем; после — тишина.
        let mut buf = [0u8; 2048];
        while tokio::time::timeout(Duration::from_millis(50), listener.recv_from(&mut buf)).await.is_ok() {}
        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.recv_from(&mut buf)).await.is_err(),
            "после отмены establish перебор портов продолжает стучаться"
        );
    }

    /// Пробив принимает ответ с любого из адресов пира: даже если основной
    /// адрес ничей, дыра открывается по дополнительному.
    #[tokio::test]
    async fn punch_succeeds_through_the_second_candidate() {
        let peer_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let a_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let b_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let a_port = a_socket.local_addr().unwrap().port();
        let b_port = b_socket.local_addr().unwrap().port();
        let dead: SocketAddr = "127.0.0.1:9".parse().unwrap(); // сюда никто не слушает

        let (a_id, b_id) = (Uuid::new_v4(), Uuid::new_v4());
        let (a_sess, b_sess) = (Uuid::new_v4(), Uuid::new_v4());
        let (link_a, link_b) = tokio::join!(
            establish(a_socket, a_port, vec![dead, SocketAddr::new(peer_ip, b_port)], identity(a_sess, b_sess, a_id, b_id), test_config(), null_events()),
            establish(b_socket, b_port, vec![SocketAddr::new(peer_ip, a_port)], identity(b_sess, a_sess, b_id, a_id), test_config(), null_events()),
        );

        assert_eq!(link_a.unwrap().peer_addr, SocketAddr::new(peer_ip, b_port));
        assert!(link_b.is_ok());
    }

    /// `Data`, отправленная по дыре, приходит пиру событием `PeerData` с
    /// номером слота.
    #[tokio::test]
    async fn data_arrives_as_peer_data_event() {
        let peer_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let a_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let b_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let a_port = a_socket.local_addr().unwrap().port();
        let b_port = b_socket.local_addr().unwrap().port();

        let (a_id, b_id) = (Uuid::new_v4(), Uuid::new_v4());
        let (a_sess, b_sess) = (Uuid::new_v4(), Uuid::new_v4());
        let (b_events_tx, mut b_events) = mpsc::channel(8);

        let (link_a, link_b) = tokio::join!(
            establish(
                a_socket,
                a_port,
                vec![SocketAddr::new(peer_ip, b_port)],
                identity(a_sess, b_sess, a_id, b_id),
                test_config(),
                null_events()
            ),
            establish(
                b_socket,
                b_port,
                vec![SocketAddr::new(peer_ip, a_port)],
                identity(b_sess, a_sess, b_id, a_id),
                test_config(),
                b_events_tx
            ),
        );
        let link_a = link_a.unwrap();
        let _link_b = link_b.unwrap();

        link_a.sender.send_data(b"hello").await;

        let event = tokio::time::timeout(Duration::from_secs(2), b_events.recv())
            .await
            .expect("событие с данными не пришло")
            .unwrap();
        match event {
            LinkEvent::PeerData { slot, payload } => {
                assert_eq!(slot, 0);
                assert_eq!(payload, b"hello");
            }
            other => panic!("ожидали PeerData, пришло {other:?}"),
        }
    }

    /// `WrappedData`, отправленная по дыре, приходит пиру событием
    /// `PeerWrapped` с номером слота, `seq` и исходными байтами.
    #[tokio::test]
    async fn wrapped_data_arrives_as_peer_wrapped_event() {
        let peer_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let a_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let b_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let a_port = a_socket.local_addr().unwrap().port();
        let b_port = b_socket.local_addr().unwrap().port();

        let (a_id, b_id) = (Uuid::new_v4(), Uuid::new_v4());
        let (a_sess, b_sess) = (Uuid::new_v4(), Uuid::new_v4());
        let (b_events_tx, mut b_events) = mpsc::channel(8);

        let (link_a, link_b) = tokio::join!(
            establish(a_socket, a_port, vec![SocketAddr::new(peer_ip, b_port)], identity(a_sess, b_sess, a_id, b_id), test_config(), null_events()),
            establish(b_socket, b_port, vec![SocketAddr::new(peer_ip, a_port)], identity(b_sess, a_sess, b_id, a_id), test_config(), b_events_tx),
        );
        let link_a = link_a.unwrap();
        let _link_b = link_b.unwrap();

        link_a
            .sender
            .send_wrapped(42, 5, Some(3), b"wg-packet")
            .await;

        let event = tokio::time::timeout(Duration::from_secs(2), b_events.recv())
            .await
            .expect("событие с обёрнутым пакетом не пришло")
            .unwrap();
        match event {
            LinkEvent::PeerWrapped { slot, seq, client_id, flow, payload } => {
                assert_eq!(slot, 0);
                assert_eq!(seq, 42);
                assert_eq!(client_id, 5);
                assert_eq!(flow, Some(3));
                assert_eq!(payload, b"wg-packet");
            }
            other => panic!("ожидали PeerWrapped, пришло {other:?}"),
        }
    }

    /// Пока keep-alive гоняется, `lost()` не срабатывает; замолчал — срабатывает.
    #[tokio::test]
    async fn lost_fires_only_after_peer_goes_silent() {
        let peer_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let a_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let b_socket = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let a_port = a_socket.local_addr().unwrap().port();
        let b_port = b_socket.local_addr().unwrap().port();

        let (a_id, b_id) = (Uuid::new_v4(), Uuid::new_v4());
        let (a_sess, b_sess) = (Uuid::new_v4(), Uuid::new_v4());

        let (link_a, link_b) = tokio::join!(
            establish(
                a_socket,
                a_port,
                vec![SocketAddr::new(peer_ip, b_port)],
                identity(a_sess, b_sess, a_id, b_id),
                test_config(),
                null_events()
            ),
            establish(
                b_socket,
                b_port,
                vec![SocketAddr::new(peer_ip, a_port)],
                identity(b_sess, a_sess, b_id, a_id),
                test_config(),
                null_events()
            ),
        );
        let link_a = link_a.unwrap();
        let link_b = link_b.unwrap();

        let ka_a = drive_keepalive(link_a.sender.clone());
        let ka_b = drive_keepalive(link_b.sender.clone());

        // Пока оба гоняют keep-alive — не потеряно.
        assert!(
            tokio::time::timeout(Duration::from_millis(500), link_a.lost())
                .await
                .is_err(),
            "lost() сработал, хотя пир шлёт keep-alive"
        );

        // B замолчал: останавливаем его keep-alive и роняем сам линк.
        drop(ka_b);
        drop(link_b);
        tokio::time::timeout(Duration::from_secs(2), link_a.lost())
            .await
            .expect("lost() не сработал после того, как пир замолчал");
        drop(ka_a);
    }

    /// Пакет с чужим `from_peer_id` (форма self-loop или посторонний) не
    /// принимается за пира.
    #[tokio::test]
    async fn ignores_packets_with_unexpected_peer_id() {
        let peer_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let under_test = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let under_test_addr = under_test.local_addr().unwrap();
        let rogue = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let rogue_port = rogue.local_addr().unwrap().port();

        let my_id = Uuid::new_v4();
        let ident = identity(
            Uuid::new_v4(),
            Uuid::new_v4(),
            my_id,
            Uuid::new_v4()
        );
        let under_test_keys = keys_to(&ident);

        let establishing = tokio::spawn(establish(
            under_test,
            rogue_port,
            vec![SocketAddr::new(peer_ip, rogue_port)],
            ident,
            test_config(),
            null_events(),
        ));

        for from_peer_id in [my_id, Uuid::new_v4()] {
            let msg = PeerMessage {
                body: Some(peer_message::Body::Init(InitMessage {
                    session_id: Uuid::new_v4().to_string(),
                    from_peer_id: peer_name(&from_peer_id),
                    to_peer_id: peer_name(&my_id),
                    slot: 0,
                    payload: Some(init_message::Payload::Punch(Punch { target_port: 0 })),
                })),
            };
            rogue
                .send_to(&codec::encode(&msg, &under_test_keys), under_test_addr)
                .await
                .unwrap();
        }

        assert!(
            tokio::time::timeout(Duration::from_millis(200), establishing)
                .await
                .is_err(),
            "establish() принял пакет-самозванец"
        );
    }

    /// Пакет от «правильного» пира, но без секрета пары (чужие вектор и подпись),
    /// не принимается.
    #[tokio::test]
    async fn ignores_packets_encoded_with_a_foreign_key() {
        let peer_ip: IpAddr = "127.0.0.1".parse().unwrap();
        let under_test = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let under_test_addr = under_test.local_addr().unwrap();
        let peer = Arc::new(UdpSocket::bind((peer_ip, 0)).await.unwrap());
        let peer_port = peer.local_addr().unwrap().port();

        let (my_id, peer_id) = (Uuid::new_v4(), Uuid::new_v4());
        let ident = identity(
            Uuid::new_v4(),
            Uuid::new_v4(),
            my_id,
            peer_id
        );

        let establishing = tokio::spawn(establish(
            under_test,
            peer_port,
            vec![SocketAddr::new(peer_ip, peer_port)],
            ident,
            test_config(),
            null_events(),
        ));

        let msg = PeerMessage {
            body: Some(peer_message::Body::Init(InitMessage {
                session_id: Uuid::new_v4().to_string(),
                from_peer_id: peer_name(&peer_id),
                to_peer_id: peer_name(&my_id),
                slot: 0,
                payload: Some(init_message::Payload::Punch(Punch { target_port: 0 })),
            })),
        };
        peer.send_to(&codec::encode(&msg, &foreign_keys()), under_test_addr)
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(200), establishing)
                .await
                .is_err(),
            "establish() принял пакет без подписи пары"
        );
    }

    /// Пара установленных дыр на loopback: `a` — под проверкой, `b` — «настоящий» пир.
    struct Pair {
        link_a: PeerLink,
        _link_b: PeerLink,
        a_events: mpsc::Receiver<LinkEvent>,
        a_addr: SocketAddr,
        /// Как настоящий пир `b` подписывает пакеты к `a`.
        to_a: SendKeys,
        b_id: Uuid,
        b_addr: SocketAddr,
    }

    async fn linked_pair() -> Pair {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let a_socket = Arc::new(UdpSocket::bind((ip, 0)).await.unwrap());
        let b_socket = Arc::new(UdpSocket::bind((ip, 0)).await.unwrap());
        let a_addr = a_socket.local_addr().unwrap();
        let b_addr = b_socket.local_addr().unwrap();
        let (a_id, b_id) = (Uuid::new_v4(), Uuid::new_v4());
        let (a_sess, b_sess) = (Uuid::new_v4(), Uuid::new_v4());
        let (a_events_tx, a_events) = mpsc::channel(16);
        let to_a = keys_to(&identity(a_sess, b_sess, a_id, b_id));
        let (link_a, link_b) = tokio::join!(
            establish(
                a_socket,
                a_addr.port(),
                vec![b_addr],
                identity(a_sess, b_sess, a_id, b_id),
                test_config(),
                a_events_tx
            ),
            establish(
                b_socket,
                b_addr.port(),
                vec![a_addr],
                identity(b_sess, a_sess, b_id, a_id),
                test_config(),
                null_events()
            ),
        );
        Pair { link_a: link_a.unwrap(), _link_b: link_b.unwrap(), a_events, a_addr, to_a, b_id, b_addr }
    }

    fn lite_packet(slot: u32, payload: lite::Payload, key: &SendKeys) -> Vec<u8> {
        let msg = PeerMessage { body: Some(peer_message::Body::Lite(Lite { slot, payload: Some(payload) })) };
        codec::encode(&msg, key)
    }

    fn punch_packet(from: Uuid, to: Uuid, key: &SendKeys) -> Vec<u8> {
        let msg = PeerMessage {
            body: Some(peer_message::Body::Init(InitMessage {
                session_id: Uuid::new_v4().to_string(),
                from_peer_id: peer_name(&from),
                to_peer_id: peer_name(&to),
                slot: 0,
                payload: Some(init_message::Payload::Punch(Punch { target_port: 0 })),
            })),
        };
        codec::encode(&msg, key)
    }

    async fn next_event(events: &mut mpsc::Receiver<LinkEvent>) -> LinkEvent {
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("событие не пришло")
            .unwrap()
    }

    /// Валидный пакет с нашего слота от другого порта пира делает этот адрес текущим
    /// эндпоинтом: и данные принимаются, и дальше мы шлём туда.
    #[tokio::test]
    async fn valid_lite_from_a_new_port_moves_the_endpoint() {
        let mut p = linked_pair().await;
        assert_eq!(p.link_a.sender.peer_addr(), Some(p.b_addr));
        let moved = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let data = lite_packet(0, lite::Payload::Data(Data { payload: b"roam".to_vec() }), &p.to_a);
        moved.send_to(&data, p.a_addr).await.unwrap();

        match next_event(&mut p.a_events).await {
            LinkEvent::PeerData { payload, .. } => assert_eq!(payload, b"roam"),
            other => panic!("ожидали PeerData, пришло {other:?}"),
        }
        assert_eq!(p.link_a.sender.peer_addr(), Some(moved.local_addr().unwrap()));

        p.link_a.sender.send_keepalive().await;
        let mut buf = [0u8; 1500];
        tokio::time::timeout(Duration::from_secs(2), moved.recv_from(&mut buf))
            .await
            .expect("keep-alive не пришёл на новый эндпоинт")
            .unwrap();
    }

    /// Пир может прийти с другого IP (другой провайдер): фильтра по IP нет.
    #[tokio::test]
    async fn valid_lite_from_another_ip_is_accepted() {
        let mut p = linked_pair().await;
        let other_ip = UdpSocket::bind("127.0.0.2:0").await.unwrap();

        let data = lite_packet(0, lite::Payload::Data(Data { payload: b"other ip".to_vec() }), &p.to_a);
        other_ip.send_to(&data, p.a_addr).await.unwrap();

        match next_event(&mut p.a_events).await {
            LinkEvent::PeerData { payload, .. } => assert_eq!(payload, b"other ip"),
            other => panic!("ожидали PeerData, пришло {other:?}"),
        }
        assert_eq!(p.link_a.sender.peer_addr(), Some(other_ip.local_addr().unwrap()));
    }

    /// Чужой слот, чужой вектор или поддельная подпись эндпоинт не двигают.
    #[tokio::test]
    async fn wrong_slot_or_key_does_not_move_the_endpoint() {
        let p = linked_pair().await;
        let rogue = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let wrong_slot = lite_packet(5, lite::Payload::KeepAlive(KeepAlive { seq: 0 }), &p.to_a);
        let wrong_key = lite_packet(0, lite::Payload::KeepAlive(KeepAlive { seq: 0 }), &foreign_keys());
        // Вектор верный (его можно подобрать по трафику), подпись — нет.
        let right_vector = SendKeys { xor: p.to_a.xor, sealer: crate::auth::Sealer::new(&[1u8; 32]) };
        let forged = lite_packet(0, lite::Payload::KeepAlive(KeepAlive { seq: 0 }), &right_vector);
        rogue.send_to(&wrong_slot, p.a_addr).await.unwrap();
        rogue.send_to(&wrong_key, p.a_addr).await.unwrap();
        rogue.send_to(&forged, p.a_addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(p.link_a.sender.peer_addr(), Some(p.b_addr));
    }

    /// Пир, пока не получил наш Ack, шлёт `Punch` с разных портов (симметричный NAT);
    /// данные с первого из них после этого не должны отбрасываться.
    #[tokio::test]
    async fn data_from_an_earlier_punch_source_is_not_dropped() {
        let mut p = linked_pair().await;
        let first = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let second = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        first.send_to(&punch_packet(p.b_id, Uuid::new_v4(), &p.to_a), p.a_addr).await.unwrap();
        second.send_to(&punch_packet(p.b_id, Uuid::new_v4(), &p.to_a), p.a_addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let data = lite_packet(0, lite::Payload::Data(Data { payload: b"from first".to_vec() }), &p.to_a);
        first.send_to(&data, p.a_addr).await.unwrap();
        match next_event(&mut p.a_events).await {
            LinkEvent::PeerData { payload, .. } => assert_eq!(payload, b"from first"),
            other => panic!("ожидали PeerData, пришло {other:?}"),
        }
    }

    /// `Ack` уходит туда, откуда пришёл `Punch`, а не туда, куда стучались мы.
    #[tokio::test]
    async fn punch_is_acked_to_its_real_source() {
        let p = linked_pair().await;
        let source = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        source.send_to(&punch_packet(p.b_id, Uuid::new_v4(), &p.to_a), p.a_addr).await.unwrap();

        let mut buf = [0u8; 1500];
        let (n, from) = tokio::time::timeout(Duration::from_secs(2), source.recv_from(&mut buf))
            .await
            .expect("PunchAck не пришёл на адрес отправителя Punch")
            .unwrap();
        assert_eq!(from, p.a_addr);
        assert!(n > 0);
    }

    /// Если `Lite` от пира пришёл раньше `Punch`/`PunchAck` (Ack потерян), дыра всё
    /// равно считается установленной.
    #[tokio::test]
    async fn lite_before_any_punch_establishes_the_link() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let under_test = Arc::new(UdpSocket::bind((ip, 0)).await.unwrap());
        let under_test_addr = under_test.local_addr().unwrap();
        let peer = UdpSocket::bind((ip, 0)).await.unwrap();
        let ident = identity(
            Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()
        );
        let key = keys_to(&ident);
        let establishing = tokio::spawn(establish(
            under_test,
            peer.local_addr().unwrap().port(),
            vec![peer.local_addr().unwrap()],
            ident,
            test_config(),
            null_events(),
        ));

        peer.send_to(&lite_packet(0, lite::Payload::KeepAlive(KeepAlive { seq: 0 }), &key), under_test_addr)
            .await
            .unwrap();

        let link = tokio::time::timeout(Duration::from_secs(2), establishing)
            .await
            .expect("establish не завершился")
            .unwrap()
            .unwrap();
        assert_eq!(link.peer_addr, peer.local_addr().unwrap());
    }

    /// Пассивная сторона (белый адрес) не стучится сама, а принимает пробив активной;
    /// активная стучится ровно в один порт (margin 0).
    #[tokio::test]
    async fn passive_side_accepts_the_active_one() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let server = Arc::new(UdpSocket::bind((ip, 0)).await.unwrap());
        let client = Arc::new(UdpSocket::bind((ip, 0)).await.unwrap());
        let (server_addr, client_addr) = (server.local_addr().unwrap(), client.local_addr().unwrap());
        let (s_id, c_id) = (Uuid::new_v4(), Uuid::new_v4());
        let (s_sess, c_sess) = (Uuid::new_v4(), Uuid::new_v4());
        let (s_link, c_link) = tokio::join!(
            establish(server, server_addr.port(), Vec::new(), identity(s_sess, c_sess, s_id, c_id), test_config(), null_events()),
            establish(client, client_addr.port(), vec![server_addr], identity(c_sess, s_sess, c_id, s_id), test_config(), null_events()),
        );
        let (s_link, c_link) = (s_link.unwrap(), c_link.unwrap());
        assert_eq!(s_link.peer_addr, client_addr);
        assert_eq!(c_link.peer_addr, server_addr);
        assert_eq!(s_link.link_id, c_link.link_id);
    }
}
