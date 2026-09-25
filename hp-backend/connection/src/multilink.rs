//! Менеджер набора дыр (слотов) до одного и того же пира.
//!
//! Идея — размазать трафик по многим независимым парам «локальный ↔ удалённый
//! адрес», чтобы поток не выглядел как один устойчивый канал. Слот k у нас
//! пробивается только к слоту k пира, так что стороны сходятся без N×N.
//!
//! Рандеву. Через MQTT договариваемся только о **bootstrap-слоте** (0). Как
//! только по нему (или по любой другой уже живой дыре) есть канал, адреса
//! остальных слотов пиры обменивают напрямую по этой дыре — «виртуал-брокер»
//! (`SlotOffer`). Так MQTT перестаёт быть точкой отказа для 9 из 10 дыр.
//!
//! Задачи:
//! - на слот — рабочая задача: STUN → анонс (MQTT для слота 0 / таблица
//!   оферов для остальных) → ожидание сессии пира → пробив; держим, пока
//!   `lost()` или пока не попросили пробить заново.
//! - общие таски: keep-alive (случайный период 2..10 c), stats, рассылка
//!   `SlotOffer`, `control` (разбор статистики/DeleteLink/оферов пира).

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::codec::{self, XorKey};
use crate::label::Label;
use crate::link_id::PeerLinkId;
use crate::port_utils;
use crate::proto::{LinkStat, Rendezvous, WrappedData};
use crate::punch::{self, LinkEvent, LinkSender, PeerIdentity, PeerLinkStat, PunchConfig};
use crate::rendezvous::{self, PeerSession, Registrar};
use crate::stun;

/// Сколько дыр набираем.
pub const TARGET_LINKS: u8 = 10;

/// Слот, о котором договариваемся через MQTT; остальные — через виртуал-брокер.
const BOOTSTRAP_SLOT: u8 = 0;

/// Окно пробива одной попытки. Длиннее TTL регистрации (60 c) — с перекрытием.
const PUNCH_WINDOW: Duration = Duration::from_secs(80);

/// Как часто обновляем регистрацию bootstrap-слота в MQTT, пока не залинкован.
const REPUBLISH_INTERVAL: Duration = Duration::from_secs(30);

/// Keep-alive шлём со случайным периодом в этих пределах (маскировка ритма).
const KEEPALIVE_MIN: Duration = Duration::from_secs(2);
const KEEPALIVE_MAX: Duration = Duration::from_secs(10);

/// Как часто шлём пиру статистику.
const STATS_INTERVAL: Duration = Duration::from_secs(10);

/// Как часто небутстрап-слот шлёт свой `Rendezvous` по живым дырам, пока не
/// залинкован (виртуал-брокер).
const HOLE_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(3);

/// Не судим о качестве дыры по выборке меньше этого числа пакетов.
const MIN_STATS_SAMPLE: u64 = 5;

/// Максимальный размер полезной нагрузки одного `send_data`/`send_wrapped`: с
/// запасом (протокольные заголовки ~30 байт) влезает в приёмный буфер (1500
/// байт). Пакет WireGuard на 32 байта длиннее вложенного, так что MTU
/// WireGuard за роутером — не больше 1368.
pub const MAX_DATA_LEN: usize = 1400;

/// Что известно об обёрнутом пакете (`WrappedData`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrappedInfo {
    /// Номер клиента (телефона), которому принадлежит пакет.
    pub client_id: u8,
    pub seq: u64,
}

/// Полезная нагрузка от пира и номер дыры, по которой она пришла.
/// `wrapped` — `Some`, если пришла обёрнутая (`WrappedData`), иначе обычная `Data`.
#[derive(Debug)]
pub struct Incoming {
    pub slot: u8,
    pub payload: Vec<u8>,
    pub wrapped: Option<WrappedInfo>,
}

/// Порядковые номера обёрнутых пакетов: свой счётчик на каждого клиента.
#[derive(Default)]
struct SeqCounters(HashMap<u8, u32>);

impl SeqCounters {
    fn next(&mut self, client_id: u8) -> u64 {
        // 32 бита достаточно (на 32-битных MIPS 64-битных атомиков нет, а здесь
        // обычный счётчик под мьютексом), в пакет уходит как u64.
        let counter = self.0.entry(client_id).or_insert(0);
        let seq = *counter;
        *counter = counter.wrapping_add(1);
        u64::from(seq)
    }
}

/// Фаза одного слота.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotPhase {
    /// STUN и анонс.
    Starting,
    /// Анонс сделан, ждём запись пира.
    Rendezvous,
    /// Идёт пробив.
    Punching,
    /// Дыра открыта.
    Connected,
}

/// Общее состояние соединения с пиром.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnState {
    /// Ни одной дыры не пробивается: идёт обмен адресами.
    Rendezvous,
    /// Нет живых дыр, но хотя бы одна пробивается.
    Punching,
    /// Столько дыр открыто.
    Connected(usize),
}

impl fmt::Display for ConnState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnState::Rendezvous => write!(f, "rendezvous"),
            ConnState::Punching => write!(f, "punching"),
            ConnState::Connected(n) => write!(f, "connected({n})"),
        }
    }
}

fn aggregate(phases: &[SlotPhase]) -> ConnState {
    let connected = phases.iter().filter(|p| **p == SlotPhase::Connected).count();
    if connected > 0 {
        ConnState::Connected(connected)
    } else if phases.contains(&SlotPhase::Punching) {
        ConnState::Punching
    } else {
        ConnState::Rendezvous
    }
}

/// Фазы всех слотов; при каждой смене общего состояния пишет его в лог.
struct StateTracker {
    inner: Mutex<TrackerInner>,
    label: Label,
}

struct TrackerInner {
    phases: Vec<SlotPhase>,
    last: Option<ConnState>,
}

impl StateTracker {
    fn new(slots: usize, label: Label) -> Self {
        Self {
            inner: Mutex::new(TrackerInner {
                phases: vec![SlotPhase::Starting; slots],
                last: None,
            }),
            label,
        }
    }

    fn set(&self, slot: u8, phase: SlotPhase) {
        let label = &self.label;
        let mut inner = self.inner.lock().unwrap();
        log::debug!("{label}слот {slot}: фаза {phase:?}");
        inner.phases[slot as usize] = phase;
        let now = aggregate(&inner.phases);
        if inner.last != Some(now) {
            match inner.last {
                Some(prev) => log::info!("{label}состояние соединения: {prev} -> {now}"),
                None => log::info!("{label}состояние соединения: {now}"),
            }
            inner.last = Some(now);
        }
    }
}

/// Выбор дыры для очередной отправки: случайно из ещё не использованных в
/// текущем цикле — сначала из всех живых, потом из оставшихся, и так пока не
/// выберут все; затем цикл начинается заново. Так порядок непредсказуем, а
/// нагрузка делится поровну. Слоты, которые перестали быть живыми, из цикла
/// выпадают; новые живые слоты попадают в него со следующего цикла.
#[derive(Default)]
struct SlotPicker {
    remaining: Vec<u8>,
}

impl SlotPicker {
    /// `random_below(n)` — случайное число из `0..n`.
    fn pick(&mut self, live: &[u8], mut random_below: impl FnMut(usize) -> usize) -> Option<u8> {
        self.remaining.retain(|slot| live.contains(slot));
        if self.remaining.is_empty() {
            self.remaining = live.to_vec();
        }
        if self.remaining.is_empty() {
            return None;
        }
        let index = random_below(self.remaining.len());
        Some(self.remaining.swap_remove(index))
    }
}

/// Случайное число из `0..n` (`n > 0`) из CSPRNG ОС (через v4 UUID: свежие
/// случайные байты без лишней зависимости). Смещение от взятия остатка от
/// 64 бит при `n` порядка десятка пренебрежимо.
fn random_below(n: usize) -> usize {
    let bytes = Uuid::new_v4().into_bytes();
    let random = u64::from_le_bytes(bytes[..8].try_into().expect("8 байт"));
    (random % n as u64) as usize
}

/// Сендер одной живой дыры плюс её слот.
#[derive(Clone)]
struct LiveLink {
    slot: u8,
    sender: Arc<LinkSender>,
}

/// Общий реестр живых дыр. `index` — тот самый `Map<PeerLinkId, u8>`.
#[derive(Default)]
struct LinkRegistry {
    by_slot: HashMap<u8, LiveLink>,
    index: HashMap<PeerLinkId, u8>,
}

impl LinkRegistry {
    fn insert(&mut self, link_id: PeerLinkId, link: LiveLink) {
        self.index.insert(link_id, link.slot);
        self.by_slot.insert(link.slot, link);
    }

    fn remove(&mut self, link_id: PeerLinkId) {
        if let Some(slot) = self.index.remove(&link_id) {
            self.by_slot.remove(&slot);
        }
    }

    fn live_count(&self) -> usize {
        self.by_slot.len()
    }

    fn links(&self) -> Vec<LiveLink> {
        self.by_slot.values().cloned().collect()
    }

    fn received_on(&self, slot: u8) -> u64 {
        self.by_slot.get(&slot).map(|l| l.sender.stats().1).unwrap_or(0)
    }
}

/// Как слот анонсирует себя пиру.
#[derive(Clone)]
enum Announce {
    /// Bootstrap-слот: публикуем `Rendezvous` в MQTT.
    Mqtt(Arc<Registrar>),
    /// Остальные: тот же `Rendezvous` шлём напрямую по живым дырам
    /// (виртуал-брокер).
    VirtualBroker(Arc<Mutex<LinkRegistry>>),
}

/// Запущенный менеджер.
pub struct MultiLink {
    registry: Arc<Mutex<LinkRegistry>>,
    my_peer_id: Uuid,
    peer_id: Uuid,
    picker: Mutex<SlotPicker>,
    wrap_seq: Mutex<SeqCounters>,
}

impl MultiLink {
    /// `label` — метка набора для логов (пустая — без метки). `stun_addrs` — один
    /// или несколько STUN-серверов: сокет каждого слота опрашивает их все, и всё
    /// увиденное публикуется как адреса слота (пир стучится по каждому).
    pub async fn start(
        label: &str,
        stun_addrs: Vec<SocketAddr>,
        mqtt_addr: SocketAddr,
        mqtt_ca_pem: Vec<u8>,
        my_peer_id: Uuid,
        peer_id: Uuid,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        let label = Label::new(label);
        let (registrar, peer_rx) =
            rendezvous::connect(label.clone(), mqtt_addr, mqtt_ca_pem, my_peer_id, peer_id)
            .await
            .context("не удалось подключиться к MQTT-брокеру")?;
        let registrar = Arc::new(registrar);
        let registry = Arc::new(Mutex::new(LinkRegistry::default()));

        let (events_tx, events_rx) = mpsc::channel::<LinkEvent>(64);
        let (incoming_tx, incoming_rx) = mpsc::channel::<Incoming>(64);
        let state = Arc::new(StateTracker::new(TARGET_LINKS as usize, label.clone()));

        let mut slot_txs: Vec<mpsc::Sender<PeerSession>> = Vec::new();
        let mut redrop_txs: Vec<mpsc::Sender<()>> = Vec::new();
        for slot in 0..TARGET_LINKS {
            let socket = Arc::new(
                UdpSocket::bind(("0.0.0.0", 0))
                    .await
                    .with_context(|| format!("не удалось создать сокет для слота {slot}"))?,
            );
            let (slot_tx, slot_rx) = mpsc::channel::<PeerSession>(8);
            let (redrop_tx, redrop_rx) = mpsc::channel::<()>(1);
            slot_txs.push(slot_tx);
            redrop_txs.push(redrop_tx);
            let announce = if slot == BOOTSTRAP_SLOT {
                Announce::Mqtt(registrar.clone())
            } else {
                Announce::VirtualBroker(registry.clone())
            };
            tokio::spawn(slot_worker(SlotCtx {
                slot,
                socket,
                stun_addrs: stun_addrs.clone(),
                my_peer_id,
                peer_id,
                announce,
                registry: registry.clone(),
                punch: PunchConfig::default(),
                peer_rx: slot_rx,
                redrop_rx,
                events: events_tx.clone(),
                state: state.clone(),
                label: label.clone(),
            }));
        }

        tokio::spawn(demux(peer_rx, slot_txs.clone()));
        tokio::spawn(keepalive_loop(registry.clone()));
        tokio::spawn(stats_loop(registry.clone()));
        tokio::spawn(control_loop(label.clone(), events_rx, registry.clone(), redrop_txs, slot_txs, incoming_tx));

        let multilink = Self {
            registry,
            my_peer_id,
            peer_id,
            picker: Mutex::new(SlotPicker::default()),
            wrap_seq: Mutex::new(SeqCounters::default()),
        };
        Ok((multilink, incoming_rx))
    }

    /// Выбирает живую дыру для очередной отправки (случайную из ещё не
    /// использованных в цикле, см. `SlotPicker`).
    fn choose_link(&self, payload_len: usize) -> Result<LiveLink> {
        anyhow::ensure!(
            payload_len <= MAX_DATA_LEN,
            "сообщение {payload_len} байт длиннее лимита {MAX_DATA_LEN}"
        );
        let mut links = self.registry.lock().unwrap().links();
        links.sort_by_key(|l| l.slot);
        let slots: Vec<u8> = links.iter().map(|l| l.slot).collect();
        let picked = self.picker.lock().unwrap().pick(&slots, random_below);
        let Some(slot) = picked else {
            anyhow::bail!("нет живых дыр");
        };
        Ok(links.into_iter().find(|l| l.slot == slot).expect("слот выбран из этого списка"))
    }

    /// Отправляет полезную нагрузку пиру по одной из живых дыр обычной `Data`.
    /// Возвращает номер дыры, по которой ушло.
    pub async fn send_data(&self, payload: Vec<u8>) -> Result<u8> {
        let link = self.choose_link(payload.len())?;
        link.sender.send_data(payload).await;
        Ok(link.slot)
    }

    /// Оборачивает `payload` в `WrappedData` для клиента `client_id` (с его
    /// порядковым номером) и отправляет по одной из живых дыр. Возвращает номер
    /// дыры и присвоенный `seq`.
    pub async fn send_wrapped(&self, client_id: u8, payload: Vec<u8>) -> Result<(u8, u64)> {
        let link = self.choose_link(payload.len())?;
        let seq = self.wrap_seq.lock().unwrap().next(client_id);
        let wrapped = WrappedData { seq, payload, client_id: u32::from(client_id) };
        link.sender.send_wrapped(wrapped).await;
        Ok((link.slot, seq))
    }

    pub fn live_count(&self) -> usize {
        self.registry.lock().unwrap().live_count()
    }

    pub fn my_peer_id(&self) -> Uuid {
        self.my_peer_id
    }

    pub fn peer_id(&self) -> Uuid {
        self.peer_id
    }
}

/// Раскладывает записи пира из MQTT по слотовым очередям (по факту — только
/// bootstrap-слот).
async fn demux(mut peer_rx: mpsc::Receiver<PeerSession>, slot_txs: Vec<mpsc::Sender<PeerSession>>) {
    while let Some(session) = peer_rx.recv().await {
        feed_slot(&slot_txs, session).await;
    }
}

/// Отдаёт запись пира рабочей задаче её слота.
async fn feed_slot(slot_txs: &[mpsc::Sender<PeerSession>], session: PeerSession) {
    match slot_txs.get(session.slot as usize) {
        Some(tx) => {
            let _ = tx.send(session).await;
        }
        None => log::warn!("запись пира на неизвестный слот {}", session.slot),
    }
}

/// Одна общая keep-alive-таска на все живые дыры. Период случайный в
/// [`KEEPALIVE_MIN`, `KEEPALIVE_MAX`] — чтобы у трафика не было ровного ритма.
async fn keepalive_loop(registry: Arc<Mutex<LinkRegistry>>) {
    loop {
        tokio::time::sleep(jittered(KEEPALIVE_MIN, KEEPALIVE_MAX)).await;
        let links = registry.lock().unwrap().links();
        for link in links {
            link.sender.send_keepalive().await;
        }
    }
}

/// Раз в `STATS_INTERVAL` шлём пиру статистику по всем живым дырам — по каждой
/// живой дыре (избыточно, зато дойдёт даже если часть деградировала).
async fn stats_loop(registry: Arc<Mutex<LinkRegistry>>) {
    let mut ticker = tokio::time::interval(STATS_INTERVAL);
    loop {
        ticker.tick().await;
        let links = registry.lock().unwrap().links();
        if links.is_empty() {
            continue;
        }
        let table: Vec<LinkStat> = links
            .iter()
            .map(|l| {
                let (sent, received) = l.sender.stats();
                LinkStat { slot: l.slot as u32, sent, received }
            })
            .collect();
        for link in &links {
            link.sender.send_stats(table.clone()).await;
        }
        log::debug!("отправлена статистика по {} дырам", table.len());
    }
}

/// Разбирает события с дыр: статистику пира, `DeleteLink`, `Rendezvous` пира.
async fn control_loop(
    label: Label,
    mut events: mpsc::Receiver<LinkEvent>,
    registry: Arc<Mutex<LinkRegistry>>,
    redrop_txs: Vec<mpsc::Sender<()>>,
    slot_txs: Vec<mpsc::Sender<PeerSession>>,
    incoming: mpsc::Sender<Incoming>,
) {
    while let Some(event) = events.recv().await {
        match event {
            LinkEvent::PeerStats(peer_stats) => {
                log::debug!("получена статистика пира: {} дыр", peer_stats.len());
                for stat in peer_stats {
                    if is_link_bad(&registry, &stat) {
                        log::warn!(
                            "{label}слот {}: пир отправил {}, мы получили {} — дыра плохая, пробиваем заново",
                            stat.slot,
                            stat.sent,
                            registry.lock().unwrap().received_on(stat.slot)
                        );
                        request_redrop(&redrop_txs, stat.slot);
                        broadcast_delete_link(&registry, stat.slot).await;
                    }
                }
            }
            LinkEvent::DeleteLink { slot } => {
                log::info!("{label}слот {slot}: пир просит удалить линк, пробиваем заново");
                request_redrop(&redrop_txs, slot);
            }
            LinkEvent::PeerData { slot, payload } => {
                let _ = incoming.send(Incoming { slot, payload, wrapped: None }).await;
            }
            LinkEvent::PeerWrapped { slot, wrapped } => {
                let Ok(client_id) = u8::try_from(wrapped.client_id) else {
                    log::warn!("{label}слот {slot}: WrappedData с client_id {} вне 0..=255", wrapped.client_id);
                    continue;
                };
                let incoming_packet = Incoming {
                    slot,
                    payload: wrapped.payload,
                    wrapped: Some(WrappedInfo { client_id, seq: wrapped.seq }),
                };
                let _ = incoming.send(incoming_packet).await;
            }
            LinkEvent::PeerRendezvous(r) => {
                match rendezvous::peer_session_from(&r) {
                    Ok(session) => feed_slot(&slot_txs, session).await,
                    Err(e) => log::warn!("некорректный Rendezvous по дыре: {e:#}"),
                }
            }
        }
    }
}

fn is_link_bad(registry: &Arc<Mutex<LinkRegistry>>, stat: &PeerLinkStat) -> bool {
    let my_received = registry.lock().unwrap().received_on(stat.slot);
    link_is_bad(stat.sent, my_received)
}

/// Чистое решение: пир отправил `peer_sent`, мы получили `my_received`.
fn link_is_bad(peer_sent: u64, my_received: u64) -> bool {
    peer_sent >= MIN_STATS_SAMPLE && my_received * 2 < peer_sent
}

fn request_redrop(redrop_txs: &[mpsc::Sender<()>], slot: u8) {
    if let Some(tx) = redrop_txs.get(slot as usize) {
        let _ = tx.try_send(());
    }
}

async fn broadcast_delete_link(registry: &Arc<Mutex<LinkRegistry>>, slot: u8) {
    let links = registry.lock().unwrap().links();
    for link in links {
        link.sender.send_delete_link(slot).await;
    }
}

struct SlotCtx {
    slot: u8,
    socket: Arc<UdpSocket>,
    stun_addrs: Vec<SocketAddr>,
    my_peer_id: Uuid,
    peer_id: Uuid,
    announce: Announce,
    registry: Arc<Mutex<LinkRegistry>>,
    punch: PunchConfig,
    peer_rx: mpsc::Receiver<PeerSession>,
    redrop_rx: mpsc::Receiver<()>,
    events: mpsc::Sender<LinkEvent>,
    state: Arc<StateTracker>,
    label: Label,
}

/// Жизненный цикл одного слота.
async fn slot_worker(mut ctx: SlotCtx) {
    let label = ctx.label.clone();
    let mut last_linked_peer_session: Option<Uuid> = None;
    loop {
        let my_session = Uuid::new_v4();
        // Каждая новая регистрация слота — новый вектор: у каждой дыры свой.
        let my_key = codec::random_key();
        ctx.state.set(ctx.slot, SlotPhase::Starting);

        let my_endpoints = match observe_endpoints(&ctx.socket, &ctx.stun_addrs).await {
            Ok(endpoints) => endpoints,
            Err(e) => {
                log::warn!("{label}слот {}: STUN не удался: {e:#}; повтор", ctx.slot);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        // Анонсируем себя пиру: bootstrap-слот — публикацией в MQTT, остальные
        // — отправкой того же `Rendezvous` по живым дырам. Держим анонс, пока
        // не залинкуемся.
        let announce = match announce_slot(
            &ctx.announce,
            ctx.my_peer_id,
            ctx.slot,
            my_session,
            &my_endpoints,
            my_key,
        )
        .await
        {
            Ok(guard) => guard,
            Err(e) => {
                log::warn!("{label}слот {}: анонс не удался: {e:#}; повтор", ctx.slot);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        ctx.state.set(ctx.slot, SlotPhase::Rendezvous);
        let peer = match wait_fresh_peer(&mut ctx.peer_rx, last_linked_peer_session).await {
            Some(peer) => peer,
            None => return,
        };

        let identity = PeerIdentity {
            session_id: my_session,
            peer_session_id: peer.session_id,
            my_peer_id: ctx.my_peer_id,
            peer_id: ctx.peer_id,
            slot: ctx.slot,
            my_key,
            peer_key: peer.key,
        };
        let my_endpoint = my_endpoints[0];
        let candidates = peer.candidates();
        let (low, high) = port_utils::sweep_bounds(my_endpoint.port(), peer.addr.port(), ctx.punch.margin);
        log::info!(
            "{label}слот {}: пробив {low}..={high} на {} (STUN-порт пира {}){}",
            ctx.slot,
            peer.addr.ip(),
            peer.addr.port(),
            if candidates.len() > 1 {
                format!(", ещё адреса пира: {:?}", &candidates[1..])
            } else {
                String::new()
            }
        );

        ctx.state.set(ctx.slot, SlotPhase::Punching);
        let attempt = punch::establish(
            ctx.socket.clone(),
            my_endpoint.port(),
            candidates,
            identity,
            ctx.punch.clone(),
            ctx.events.clone(),
        );
        let link = match tokio::time::timeout(PUNCH_WINDOW, attempt).await {
            Ok(Ok(link)) => link,
            Ok(Err(e)) => {
                log::warn!("{label}слот {}: пробив не удался: {e}; новая попытка", ctx.slot);
                continue;
            }
            Err(_) => {
                log::warn!("{label}слот {}: пробив не уложился в {PUNCH_WINDOW:?}; новая попытка", ctx.slot);
                continue;
            }
        };

        // Залинковались — анонс больше не нужен (ждём новую сессию пира при
        // следующей потере).
        drop(announce);
        while ctx.redrop_rx.try_recv().is_ok() {}

        let link_id = link.link_id;
        last_linked_peer_session = Some(peer.session_id);
        {
            let mut reg = ctx.registry.lock().unwrap();
            reg.insert(link_id, LiveLink { slot: ctx.slot, sender: link.sender.clone() });
            log::info!(
                "{label}слот {}: дыра открыта {} <-> {} (живых дыр: {})",
                ctx.slot,
                link.local_addr,
                link.peer_addr,
                reg.live_count()
            );
            log::debug!("{label}слот {}: дыра добавлена в реестр (живых: {})", ctx.slot, reg.live_count());
        }
        ctx.state.set(ctx.slot, SlotPhase::Connected);

        tokio::select! {
            _ = link.lost() => log::warn!("{label}слот {}: дыра потеряна, перерегистрация", ctx.slot),
            _ = ctx.redrop_rx.recv() => log::warn!("{label}слот {}: дыра помечена плохой, пробиваем заново", ctx.slot),
        }
        {
            let mut reg = ctx.registry.lock().unwrap();
            reg.remove(link_id);
            log::debug!("{label}слот {}: дыра удалена из реестра (живых: {})", ctx.slot, reg.live_count());
        }
    }
}

/// Строит запись `Rendezvous` о нашем слоте (одинаковую для MQTT и для отправки
/// по дыре). `registered_at_unix_ms` над дырой не используется.
fn our_rendezvous(
    my_peer_id: Uuid,
    slot: u8,
    session: Uuid,
    endpoints: &[SocketAddr],
    key: XorKey,
) -> Rendezvous {
    rendezvous::our_record(my_peer_id, slot, session, endpoints, key, 0)
}

/// Анонс слота; возвращает `AbortOnDrop` фоновой задачи анонса, которую держим
/// до линковки (дроп её останавливает).
async fn announce_slot(
    announce: &Announce,
    my_peer_id: Uuid,
    slot: u8,
    session: Uuid,
    endpoints: &[SocketAddr],
    key: XorKey,
) -> Result<AbortOnDrop> {
    match announce {
        Announce::Mqtt(registrar) => {
            registrar
                .publish_slot(slot, session, endpoints, key)
                .await
                .context("публикация в MQTT")?;
            Ok(spawn_mqtt_republish(registrar.clone(), slot, session, endpoints.to_vec(), key))
        }
        Announce::VirtualBroker(registry) => {
            let rendezvous = our_rendezvous(my_peer_id, slot, session, endpoints, key);
            Ok(spawn_hole_announce(registry.clone(), rendezvous))
        }
    }
}

/// Периодически обновляет MQTT-регистрацию bootstrap-слота, пока хэндл жив.
fn spawn_mqtt_republish(
    registrar: Arc<Registrar>,
    slot: u8,
    session_id: Uuid,
    endpoints: Vec<SocketAddr>,
    key: XorKey,
) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REPUBLISH_INTERVAL);
        ticker.tick().await; // первый тик сразу — публикацию уже сделали снаружи
        loop {
            ticker.tick().await;
            if registrar.publish_slot(slot, session_id, &endpoints, key).await.is_err() {
                return;
            }
        }
    }))
}

/// Виртуал-брокер: периодически шлёт наш `Rendezvous` по всем живым дырам,
/// пока хэндл жив. Пока живых дыр нет — просто ждёт следующего тика.
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
async fn wait_fresh_peer(
    peer_rx: &mut mpsc::Receiver<PeerSession>,
    already_linked: Option<Uuid>,
) -> Option<PeerSession> {
    loop {
        let peer = peer_rx.recv().await?;
        if Some(peer.session_id) == already_linked {
            continue;
        }
        return Some(peer);
    }
}

/// Опрашивает все STUN-серверы с сокета слота. Возвращает увиденные адреса без
/// повторов (первый — от первого ответившего сервера). Не ответил ни один —
/// ошибка. Если серверы видят сокет по-разному, это пишется в лог: разные
/// адреса — разные маршруты, разные порты — NAT с зависимостью от адресата.
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

/// Псевдослучайная длительность в [min, max] на основе текущего времени
/// (для джиттера качество ГПСЧ не важно, поэтому без внешних зависимостей).
fn jittered(min: Duration, max: Duration) -> Duration {
    let span = max.saturating_sub(min).as_millis().max(1) as u64;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64;
    min + Duration::from_millis(nanos % span)
}

/// Абортит задачу при дропе.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_link_needs_a_sample() {
        assert!(!link_is_bad(MIN_STATS_SAMPLE - 1, 0));
    }

    #[test]
    fn bad_link_when_delivery_below_half() {
        assert!(link_is_bad(100, 10));
        assert!(link_is_bad(100, 49));
        assert!(!link_is_bad(100, 50));
        assert!(!link_is_bad(100, 90));
    }

    #[test]
    fn state_is_connected_count_when_any_hole_is_live() {
        use SlotPhase::*;
        assert_eq!(aggregate(&[Starting, Rendezvous]), ConnState::Rendezvous);
        assert_eq!(aggregate(&[Rendezvous, Punching]), ConnState::Punching);
        assert_eq!(aggregate(&[Connected, Punching, Starting]), ConnState::Connected(1));
        assert_eq!(aggregate(&[Connected, Connected, Punching]), ConnState::Connected(2));
    }

    #[test]
    fn state_display_names() {
        assert_eq!(ConnState::Rendezvous.to_string(), "rendezvous");
        assert_eq!(ConnState::Punching.to_string(), "punching");
        assert_eq!(ConnState::Connected(2).to_string(), "connected(2)");
    }

    /// Детерминированный «генератор» для тестов: берёт по очереди значения из
    /// списка (по модулю размера мешка).
    fn scripted(values: &[usize]) -> impl FnMut(usize) -> usize {
        let values = values.to_vec();
        let mut next = 0;
        move |n| {
            let value = values[next % values.len()];
            next += 1;
            value % n
        }
    }

    #[test]
    fn picker_has_nothing_to_pick_without_live_slots() {
        assert_eq!(SlotPicker::default().pick(&[], scripted(&[0])), None);
    }

    #[test]
    fn picker_uses_every_live_slot_once_per_cycle_then_starts_over() {
        let live: Vec<u8> = (0..10).collect();
        let mut picker = SlotPicker::default();
        let mut rand = scripted(&[7, 3, 9, 0, 5, 5, 1, 2, 8, 4, 6]);
        for cycle in 0..3 {
            let mut got: Vec<u8> = (0..10).map(|_| picker.pick(&live, &mut rand).unwrap()).collect();
            got.sort_unstable();
            assert_eq!(got, live, "цикл {cycle}: каждая дыра ровно один раз");
        }
    }

    #[test]
    fn picker_order_follows_the_random_source_not_slot_order() {
        let live = [0u8, 1, 2, 3];
        let mut picker = SlotPicker::default();
        // Всегда берём последний из оставшихся: 0,1,2,3 -> 3, потом 2 ...
        let picked: Vec<u8> = (0..4).map(|_| picker.pick(&live, |n| n - 1).unwrap()).collect();
        assert_eq!(picked, [3, 2, 1, 0]);
    }

    #[test]
    fn picker_drops_dead_slots_and_adds_new_ones_next_cycle() {
        let mut picker = SlotPicker::default();
        assert!(picker.pick(&[1, 2, 3], |_| 0).is_some()); // убрали один из трёх
        // слот 2 умер, появился слот 9: 9 подключится только в следующем цикле
        let live = [1u8, 3, 9];
        let mut got = Vec::new();
        for _ in 0..2 {
            got.push(picker.pick(&live, |_| 0).unwrap());
        }
        assert!(got.iter().all(|s| [1, 3].contains(s)), "в этом цикле только прежние живые: {got:?}");
        assert_eq!(got.len(), 2);
        assert!(!got.contains(&2), "мёртвый слот выбран: {got:?}");
    }

    #[test]
    fn random_below_stays_in_range_and_varies() {
        let values: Vec<usize> = (0..200).map(|_| random_below(10)).collect();
        assert!(values.iter().all(|v| *v < 10));
        assert!(values.iter().collect::<std::collections::HashSet<_>>().len() > 5);
    }

    #[test]
    fn wrap_seq_counts_separately_per_client() {
        let mut counters = SeqCounters::default();
        assert_eq!(counters.next(1), 0);
        assert_eq!(counters.next(1), 1);
        assert_eq!(counters.next(2), 0, "у клиента 2 свой счётчик");
        assert_eq!(counters.next(1), 2);
        assert_eq!(counters.next(2), 1);
    }

    #[test]
    fn wrap_seq_wraps_at_32_bits() {
        let mut counters = SeqCounters::default();
        counters.0.insert(7, u32::MAX);
        assert_eq!(counters.next(7), u64::from(u32::MAX));
        assert_eq!(counters.next(7), 0);
    }

    #[test]
    fn jitter_stays_in_bounds() {
        let d = jittered(KEEPALIVE_MIN, KEEPALIVE_MAX);
        assert!(d >= KEEPALIVE_MIN && d < KEEPALIVE_MAX);
    }
}
