//! Менеджер набора дыр (слотов) до одного и того же пира.
//!
//! Идея — размазать трафик по многим независимым парам «локальный ↔ удалённый адрес», чтобы
//! поток не выглядел как один устойчивый канал.
//!
//! Здесь — общее для всех режимов: реестр живых дыр, выбор дыры для отправки, keep-alive
//! (случайный период 2..10 c), статистика и её разбор (плохая дыра → пробить заново), приём
//! данных с восстановлением порядка, состояние набора. Как слот находит пира и получает дыру —
//! у каждого режима своя стейт-машина, они не смешиваются:
//! - `p2p` — оба пира за NAT: STUN + MQTT + пробив;
//! - `vps` — у сервера белый IP: клиент спрашивает порт знакомства, сервер выдаёт порт слота.
//!
//! Слот любого режима, получив дыру, отдаёт её `SlotBase::hold`: дыра в реестре, пока не
//! потеряна или не помечена плохой.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::auth::PairSecret;
use crate::label::Label;
use crate::link_id::PeerLinkId;
use crate::pool::Packet;
use crate::proto::{LinkStat, Rendezvous};
use crate::punch::{LinkEvent, LinkSender, PeerLink, PeerLinkStat};
use crate::reorder::{ReorderStats, Resequencer};
use crate::rendezvous::{PeerRegistration, Registrar};
#[cfg(feature = "stats")]
use crate::stats_feedback;
use crate::{p2p, vps};

/// Номер дыры (слота). `u32`, не `u8`: в динамическом наборе (PLAN-dynamic-holes-relay.md) номер
/// монотонный и не переиспользуется — дыра «поработала — умерла», при частой ротации u8
/// переполнился бы за несколько минут. P2P/VPS с фиксированным набором продолжают использовать
/// значения `0..TARGET_LINKS`, тип общий.
pub type SlotId = u32;

/// Сколько дыр набираем.
pub const TARGET_LINKS: SlotId = 10;

/// Keep-alive шлём со случайным периодом в этих пределах (маскировка ритма).
const KEEPALIVE_MIN: Duration = Duration::from_secs(2);
const KEEPALIVE_MAX: Duration = Duration::from_secs(10);

/// Как часто шлём пиру статистику.
const STATS_INTERVAL: Duration = Duration::from_secs(10);

/// Не судим о качестве дыры по выборке меньше этого числа пакетов.
const MIN_STATS_SAMPLE: u64 = 5;

/// Максимальный размер полезной нагрузки одного `send_data`/`send_client`: с
/// запасом (протокольные заголовки ~30 байт) влезает в приёмный буфер (1500
/// байт с подписью). Это IP-пакет целиком: MTU TUN — не больше 1400.
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
    pub slot: SlotId,
    /// Нагрузка в буфере из банка (`pool`): буфер вернётся в банк, когда пакет отправят дальше.
    pub payload: Packet,
    pub wrapped: Option<WrappedInfo>,
    /// Номер в потоке (`Ordered`, TUN-режим): корзина и номер; по нему восстанавливается порядок.
    pub order: Option<(u32, u64)>,
}

/// Служебное сообщение между пирами (не данные): запрос и выдача адреса в туннеле.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    AddressRequest(crate::proto::AddressRequest),
    AddressAssign(crate::proto::AddressAssign),
}

/// Сколько служебных сообщений держим, пока приложение их не забрало.
const CONTROL_CHANNEL_CAPACITY: usize = 16;

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
pub(crate) enum SlotPhase {
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

    fn current(&self) -> ConnState {
        aggregate(&self.inner.lock().unwrap().phases)
    }

    fn set(&self, slot: SlotId, phase: SlotPhase) {
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
struct SlotPicker {
    /// Ещё не использованные в этом цикле слоты (без выделения памяти: слотов ≤ `TARGET_LINKS`).
    remaining: [SlotId; TARGET_LINKS as usize],
    len: usize,
    rng: XorShift32,
}

impl Default for SlotPicker {
    fn default() -> Self {
        Self { remaining: [0; TARGET_LINKS as usize], len: 0, rng: XorShift32::seeded() }
    }
}

impl SlotPicker {
    /// `random_below(n)` — случайное число из `0..n`.
    fn pick(&mut self, live: &[SlotId], mut random_below: impl FnMut(usize) -> usize) -> Option<SlotId> {
        let mut kept = 0;
        for i in 0..self.len {
            if live.contains(&self.remaining[i]) {
                self.remaining[kept] = self.remaining[i];
                kept += 1;
            }
        }
        self.len = kept;
        if self.len == 0 {
            self.len = live.len().min(self.remaining.len());
            self.remaining[..self.len].copy_from_slice(&live[..self.len]);
        }
        if self.len == 0 {
            return None;
        }
        let index = random_below(self.len);
        let slot = self.remaining[index];
        self.len -= 1;
        self.remaining[index] = self.remaining[self.len];
        Some(slot)
    }

    /// То же со своим генератором случайных чисел.
    fn pick_random(&mut self, live: &[SlotId]) -> Option<SlotId> {
        let mut rng = self.rng;
        let slot = self.pick(live, |n| rng.below(n));
        self.rng = rng;
        slot
    }
}

/// Быстрый генератор случайных чисел для выбора дыры (xorshift32, 32 бита — годится и для
/// MIPS32). Это распределение нагрузки, а не криптография: сид один раз из CSPRNG ОС (v4 UUID),
/// дальше без системных вызовов (раньше на каждый пакет был `getrandom`).
#[derive(Clone, Copy)]
struct XorShift32(u32);

impl XorShift32 {
    fn seeded() -> Self {
        let bytes = Uuid::new_v4().into_bytes();
        Self(u32::from_le_bytes(bytes[..4].try_into().expect("4 байта")) | 1)
    }

    fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Случайное число из `0..n` (`n > 0`); смещение от остатка при `n` порядка десятка пренебрежимо.
    fn below(&mut self, n: usize) -> usize {
        self.next() as usize % n
    }
}

/// Сендер одной живой дыры плюс её слот.
#[derive(Clone)]
pub(crate) struct LiveLink {
    pub slot: SlotId,
    pub sender: Arc<LinkSender>,
    /// hp-stats (фича `stats`): когда дыра встала в реестр — для `hole_age_ms` в записях.
    #[cfg(feature = "stats")]
    pub established_at: Instant,
}

/// Общий реестр живых дыр. `index` — тот самый `Map<PeerLinkId, SlotId>`.
#[derive(Default)]
pub(crate) struct LinkRegistry {
    by_slot: HashMap<SlotId, LiveLink>,
    index: HashMap<PeerLinkId, SlotId>,
    /// Последний отчёт пира по дыре и наши счётчики на момент его прихода — для потерь.
    reports: HashMap<SlotId, SlotReport>,
}

/// Отчёт пира о дыре (`Stats`) и наши счётчики той же дыры в момент его получения.
#[derive(Clone, Copy, Debug, Default)]
struct SlotReport {
    peer_sent: u64,
    peer_received: u64,
    my_sent: u64,
    my_received: u64,
}

/// Минимум пакетов для оценки потерь: на меньшей выборке доля бессмысленна.
const MIN_LOSS_SAMPLE: u64 = 20;

/// Доля потерь: отправлено `sent`, дошло `received`; `None` — мало данных.
fn loss(sent: u64, received: u64) -> Option<f32> {
    (sent >= MIN_LOSS_SAMPLE).then(|| (1.0 - received as f32 / sent as f32).clamp(0.0, 1.0))
}

/// Состояние одной живой дыры.
#[derive(Clone, Debug, PartialEq)]
pub struct HoleStatus {
    pub slot: SlotId,
    /// Адрес пира на этой дыре.
    pub peer_addr: Option<SocketAddr>,
    /// Пакетов отправлено и получено по дыре с её открытия.
    pub sent: u64,
    pub received: u64,
    /// Потери от нас к пиру и от пира к нам по последнему отчёту пира (раз в 10 с); `None` —
    /// отчёта ещё нет или мало пакетов.
    pub loss_out: Option<f32>,
    pub loss_in: Option<f32>,
}

/// Снимок набора дыр: общее состояние и живые дыры по порядку слотов.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkStatus {
    pub state: ConnState,
    pub holes: Vec<HoleStatus>,
    /// Текущее ожидание недостающего TCP-пакета в буфере порядка (адаптивное, 3..=30 мс; 0 —
    /// буфер порядка выключен, `REORDER_WAIT_MS=0`).
    pub reorder_wait_ms: u32,
    /// Последняя регистрация пира на MQTT-брокере; `None` — не было или режим без MQTT (VPS).
    pub peer_registration: Option<PeerRegistration>,
}

impl LinkRegistry {
    fn insert(&mut self, link_id: PeerLinkId, link: LiveLink) {
        self.reports.remove(&link.slot);
        self.index.insert(link_id, link.slot);
        self.by_slot.insert(link.slot, link);
    }

    fn remove(&mut self, link_id: PeerLinkId) {
        if let Some(slot) = self.index.remove(&link_id) {
            self.by_slot.remove(&slot);
            self.reports.remove(&slot);
        }
    }

    fn live_count(&self) -> usize {
        self.by_slot.len()
    }

    pub(crate) fn links(&self) -> Vec<LiveLink> {
        self.by_slot.values().cloned().collect()
    }

    fn received_on(&self, slot: SlotId) -> u64 {
        self.by_slot.get(&slot).map(|l| l.sender.stats().1).unwrap_or(0)
    }

    /// Запоминает отчёт пира о дыре вместе с нашими счётчиками на этот момент.
    fn record_report(&mut self, stat: &PeerLinkStat) {
        if let Some(link) = self.by_slot.get(&stat.slot) {
            let (my_sent, my_received) = link.sender.stats();
            self.reports.insert(stat.slot, SlotReport { peer_sent: stat.sent, peer_received: stat.received, my_sent, my_received });
        }
    }

    fn holes(&self) -> Vec<HoleStatus> {
        let mut holes: Vec<HoleStatus> = self
            .by_slot
            .values()
            .map(|link| {
                let (sent, received) = link.sender.stats();
                let report = self.reports.get(&link.slot);
                HoleStatus {
                    slot: link.slot,
                    peer_addr: link.sender.peer_addr(),
                    sent,
                    received,
                    loss_out: report.and_then(|r| loss(r.my_sent, r.peer_received)),
                    loss_in: report.and_then(|r| loss(r.peer_sent, r.my_received)),
                }
            })
            .collect();
        holes.sort_by_key(|h| h.slot);
        holes
    }
}

/// Как стороны узнают друг о друге.
#[derive(Clone, Debug)]
pub enum Discovery {
    /// Обычный режим: STUN + MQTT, пробив NAT.
    StunMqtt { stun_addrs: Vec<SocketAddr>, mqtt_addr: SocketAddr, mqtt_ca_pem: Vec<u8> },
    /// Сервер с белым IP: слушает порт знакомства, слоты занимают случайные порты из
    /// `ports`, пробив пассивный (ждём клиента).
    VpsServer { public_ip: IpAddr, bootstrap_port: u16, ports: RangeInclusive<u16> },
    /// Клиент VPS-сервера: знает `ip:порт знакомства`, ни STUN, ни MQTT не нужен.
    VpsClient { server: SocketAddr },
}

/// Запущенный менеджер.
/// Сколько ждём недостающий TCP-пакет, прежде чем отдать накопленное дальше.
pub const DEFAULT_REORDER_WAIT: Duration = Duration::from_millis(30);

/// Настройки `MultiLink`.
#[derive(Debug, Clone, Copy)]
pub struct MultiLinkOptions {
    /// Начальное ожидание недостающего пакета при восстановлении порядка на приёме
    /// (`reorder`), дальше оно подстраивается под сеть в пределах 3–30 мс; ноль отключает
    /// буфер порядка.
    pub reorder_wait: Duration,
    /// Сколько дыр использовать для отправки данных: 0 — все живые, `N` — только `N` живых
    /// дыр с наименьшими номерами слотов (`1` — всё через одну дыру, для замеров и
    /// отладки; поток тогда не перемешивается).
    pub data_holes: u8,
    /// Первый локальный UDP-порт слотов: слот `k` занимает `base + k`. 0 — порты выбирает ОС.
    /// Нужен, когда брандмауэр пропускает входящий UDP только в известном диапазоне.
    pub local_port_base: u16,
    /// Восстанавливать ли порядок у пакетов клиентов (`WrappedData` с корзиной потока). Конечный
    /// получатель (VPS) — да; роутер, который только перекладывает пакеты телефонов, — нет:
    /// порядок вернёт тот, кому пакет адресован.
    pub reorder_clients: bool,
    /// Локальный адрес сокетов слотов (и их STUN-запросов); `None` — `0.0.0.0`. Адрес физического
    /// адаптера уводит дыры мимо VPN на этой машине: система отправляет пакет с интерфейса, которому
    /// принадлежит адрес источника (Windows — всегда, Linux — при правиле по источнику).
    pub bind_ip: Option<IpAddr>,
    /// Номер интерфейса, к которому привязать сокеты слотов (Linux: без него сокет с адресом
    /// физического адаптера всё равно ушёл бы по маршруту по умолчанию — в VPN); см. `bind`.
    pub bind_ifindex: Option<u32>,
}

impl Default for MultiLinkOptions {
    fn default() -> Self {
        Self { reorder_wait: DEFAULT_REORDER_WAIT, data_holes: 0, local_port_base: 0, reorder_clients: true, bind_ip: None, bind_ifindex: None }
    }
}

/// Оставляет для отправки не больше `max` слотов с наименьшими номерами (0 — все); сортирует
/// на месте, возвращает, сколько слотов оставить.
fn limit_slots(slots: &mut [SlotId], max: u8) -> usize {
    slots.sort_unstable();
    if max > 0 { slots.len().min(usize::from(max)) } else { slots.len() }
}

pub struct MultiLink {
    registry: Arc<Mutex<LinkRegistry>>,
    my_peer_id: Uuid,
    peer_id: Uuid,
    picker: Mutex<SlotPicker>,
    wrap_seq: Mutex<SeqCounters>,
    data_holes: u8,
    redrop_txs: Vec<mpsc::Sender<()>>,
    control_rx: Mutex<Option<mpsc::Receiver<Control>>>,
    state: Arc<StateTracker>,
    /// Текущее ожидание буфера порядка (обновляется раз в 30 с из `control_loop`); см. `LinkStatus`.
    reorder_wait_ms: Arc<AtomicU32>,
    /// MQTT-регистратор (только `Discovery::StunMqtt`) — для последней регистрации пира в статусе.
    registrar: Option<Arc<Registrar>>,
    /// hp-stats (фича `stats`, PLAN-ML.md): хэндл сборщика, если сбор включён (`attach_stats`);
    /// `None` — ничего не собираем, даже если бинарь собран с фичей.
    #[cfg(feature = "stats")]
    stats: Arc<Mutex<Option<hp_stats::StatsHandle>>>,
    /// Счётчик номеров пакетов вниз (hp-stats); реально используется, только когда `stats` — `Some`.
    #[cfg(feature = "stats")]
    pid_counter: AtomicU32,
    /// Задачи набора: дроп `MultiLink` останавливает их, а с ними — дыры, сокеты и MQTT.
    _tasks: Vec<AbortOnDrop>,
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
        Self::start_with(label, stun_addrs, mqtt_addr, mqtt_ca_pem, my_peer_id, peer_id, MultiLinkOptions::default())
            .await
    }

    /// То же, что `start`, с явными настройками.
    pub async fn start_with(
        label: &str,
        stun_addrs: Vec<SocketAddr>,
        mqtt_addr: SocketAddr,
        mqtt_ca_pem: Vec<u8>,
        my_peer_id: Uuid,
        peer_id: Uuid,
        options: MultiLinkOptions,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        let discovery = Discovery::StunMqtt { stun_addrs, mqtt_addr, mqtt_ca_pem };
        Self::start_discovery(label, discovery, my_peer_id, peer_id, options).await
    }

    /// Запуск с явным способом знакомства: P2P (`p2p`) или VPS (`vps`). Общее — реестр дыр,
    /// keep-alive, статистика, приём данных; слоты — у каждого режима свои.
    pub async fn start_discovery(
        label: &str,
        discovery: Discovery,
        my_peer_id: Uuid,
        peer_id: Uuid,
        options: MultiLinkOptions,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        let label = Label::new(label);
        let registry = Arc::new(Mutex::new(LinkRegistry::default()));
        // Из двух полных GUID — секрет пары: из него все ключи и подписи (`auth`).
        let pair = PairSecret::new(my_peer_id, peer_id);
        let (events_tx, events_rx) = mpsc::channel::<LinkEvent>(64);
        let (incoming_tx, incoming_rx) = mpsc::channel::<Incoming>(64);
        let (control_tx, control_rx) = mpsc::channel::<Control>(CONTROL_CHANNEL_CAPACITY);
        let state = Arc::new(StateTracker::new(TARGET_LINKS as usize, label.clone()));

        let mut redrop_txs = Vec::new();
        let mut bases = Vec::new();
        for slot in 0..TARGET_LINKS {
            let (redrop_tx, redrop_rx) = mpsc::channel::<()>(1);
            redrop_txs.push(redrop_tx);
            bases.push(SlotBase {
                slot,
                label: label.clone(),
                registry: registry.clone(),
                events: events_tx.clone(),
                state: state.clone(),
                redrop_rx,
            });
        }

        let bind_ip = options.bind_ip.unwrap_or(IpAddr::from([0, 0, 0, 0]));
        let vps_pair = || vps::Pair { my_peer_id, peer_id, secret: pair.clone() };
        let (mut tasks, hole_records, registrar) = match discovery {
            Discovery::StunMqtt { stun_addrs, mqtt_addr, mqtt_ca_pem } => {
                let config = p2p::Config {
                    stun_addrs,
                    mqtt_addr,
                    mqtt_ca_pem,
                    my_peer_id,
                    peer_id,
                    pair: pair.clone(),
                    bind_ip,
                    bind_ifindex: options.bind_ifindex,
                    local_port_base: options.local_port_base,
                };
                let started = p2p::start(&label, config, bases).await?;
                (started.tasks, Some(started.hole_records), Some(started.registrar))
            }
            Discovery::VpsServer { public_ip, bootstrap_port, ports } => {
                let config = vps::ServerConfig { public_ip, bootstrap_port, ports, pair: vps_pair() };
                (vps::start_server(&label, config, bases).await?, None, None)
            }
            Discovery::VpsClient { server } => {
                let config = vps::ClientConfig { server, pair: vps_pair(), bind_ip, bind_ifindex: options.bind_ifindex };
                (vps::start_client(&label, config, bases).await?, None, None)
            }
        };

        let reorder_wait_ms = Arc::new(AtomicU32::new(options.reorder_wait.as_millis() as u32));
        tasks.push(AbortOnDrop(tokio::spawn(keepalive_loop(registry.clone()))));
        tasks.push(AbortOnDrop(tokio::spawn(stats_loop(registry.clone()))));
        // hp-stats (фича `stats`): `stats_cell` общая с `MultiLink.stats` — `attach_stats`,
        // вызванный после возврата из этой функции, должен быть виден уже запущенному
        // `control_loop`. `time_sync`/`pid_feedback` общие между `control_loop` (разбирает
        // входящие `TimeEcho`/`pid`) и их собственными таймерными задачами.
        #[cfg(feature = "stats")]
        let stats_cell: Arc<Mutex<Option<hp_stats::StatsHandle>>> = Arc::new(Mutex::new(None));
        #[cfg(feature = "stats")]
        let time_sync = Arc::new(Mutex::new(stats_feedback::TimeSync::new()));
        #[cfg(feature = "stats")]
        let pid_feedback = Arc::new(Mutex::new(stats_feedback::PidFeedbackBuilder::new()));
        #[cfg(feature = "stats")]
        tasks.push(AbortOnDrop(tokio::spawn(time_probe_loop(registry.clone(), time_sync.clone()))));
        #[cfg(feature = "stats")]
        tasks.push(AbortOnDrop(tokio::spawn(pid_report_loop(registry.clone(), pid_feedback.clone()))));
        tasks.push(AbortOnDrop(tokio::spawn(control_loop(
            label.clone(),
            events_rx,
            registry.clone(),
            redrop_txs.clone(),
            hole_records,
            (incoming_tx, control_tx),
            options,
            reorder_wait_ms.clone(),
            #[cfg(feature = "stats")]
            stats_cell.clone(),
            #[cfg(feature = "stats")]
            time_sync,
            #[cfg(feature = "stats")]
            pid_feedback,
        ))));

        let multilink = Self {
            registry,
            my_peer_id,
            peer_id,
            picker: Mutex::new(SlotPicker::default()),
            wrap_seq: Mutex::new(SeqCounters::default()),
            data_holes: options.data_holes,
            redrop_txs,
            control_rx: Mutex::new(Some(control_rx)),
            state,
            reorder_wait_ms,
            registrar,
            #[cfg(feature = "stats")]
            stats: stats_cell,
            #[cfg(feature = "stats")]
            pid_counter: AtomicU32::new(0),
            _tasks: tasks,
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
        // Без выделения памяти: живых дыр ≤ TARGET_LINKS, список — на стеке.
        let registry = self.registry.lock().unwrap();
        let mut slots = [0; TARGET_LINKS as usize];
        let mut count = 0;
        for &slot in registry.by_slot.keys() {
            if count < slots.len() {
                slots[count] = slot;
                count += 1;
            }
        }
        let count = limit_slots(&mut slots[..count], self.data_holes);
        let picked = self.picker.lock().unwrap().pick_random(&slots[..count]);
        let Some(slot) = picked else {
            anyhow::bail!("нет живых дыр");
        };
        Ok(registry.by_slot.get(&slot).expect("слот выбран из живых").clone())
    }

    /// Отправляет полезную нагрузку пиру по одной из живых дыр обычной `Data`.
    /// Возвращает номер дыры, по которой ушло.
    pub async fn send_data(&self, payload: &[u8]) -> Result<SlotId> {
        let link = self.choose_link(payload.len())?;
        let pid = self.next_pid();
        #[cfg(feature = "stats")]
        if let Some(pid) = pid {
            self.record_sent(pid, &link, payload, hp_stats::MsgKind::Data, None, None);
        }
        link.sender.send_data(payload, pid).await;
        Ok(link.slot)
    }

    /// Отправляет пакет клиента `client_id` (`WrappedData`). `order` — корзина TCP-потока и номер в
    /// ней, выставленные исходным отправителем (TUN-режим): их не меняем, порядок вернёт конечный
    /// получатель. Без `order` пакет идёт без порядка (номер — свой счётчик клиента). Возвращает
    /// номер дыры.
    pub async fn send_client(&self, client_id: u8, order: Option<(u32, u64)>, payload: &[u8]) -> Result<SlotId> {
        let link = self.choose_link(payload.len())?;
        let (flow, seq) = match order {
            Some((flow, seq)) => (Some(flow), seq),
            None => (None, self.wrap_seq.lock().unwrap().next(client_id)),
        };
        let pid = self.next_pid();
        #[cfg(feature = "stats")]
        if let Some(pid) = pid {
            self.record_sent(pid, &link, payload, hp_stats::MsgKind::Wrapped, Some(client_id), flow);
        }
        link.sender.send_wrapped(seq, u32::from(client_id), flow, payload, pid).await;
        Ok(link.slot)
    }

    /// Отправляет IP-пакет с номером в потоке (`Ordered`, TUN-режим): получатель вернёт порядок
    /// внутри корзины `flow`. Возвращает номер дыры.
    pub async fn send_ordered(&self, flow: u32, seq: u64, payload: &[u8]) -> Result<SlotId> {
        let link = self.choose_link(payload.len())?;
        let pid = self.next_pid();
        #[cfg(feature = "stats")]
        if let Some(pid) = pid {
            self.record_sent(pid, &link, payload, hp_stats::MsgKind::Ordered, None, Some(flow));
        }
        link.sender.send_ordered(flow, seq, payload, pid).await;
        Ok(link.slot)
    }

    /// Отправляет пиру служебное сообщение по одной из живых дыр (без гарантии доставки:
    /// запросы повторяются вызывающим).
    pub async fn send_control(&self, control: Control) -> Result<SlotId> {
        let link = self.choose_link(0)?;
        link.sender.send_control(control).await;
        Ok(link.slot)
    }

    /// Приёмник служебных сообщений пира; отдаётся один раз (дальше `None`). Пока его не
    /// забрали, сообщения сверх небольшого запаса отбрасываются.
    pub fn take_control(&self) -> Option<mpsc::Receiver<Control>> {
        self.control_rx.lock().unwrap().take()
    }

    pub fn live_count(&self) -> usize {
        self.registry.lock().unwrap().live_count()
    }

    /// Перенести дыру `slot` на новые порты: слот регистрируется заново (в VPS-режиме —
    /// на новом порту сервера), пиру уходит `DeleteLink`, чтобы и он бросил старую.
    pub async fn move_slot(&self, slot: SlotId) {
        request_redrop(&self.redrop_txs, slot);
        broadcast_delete_link(&self.registry, slot).await;
    }

    /// Живые дыры: слот и текущий адрес пира.
    pub fn live_links(&self) -> Vec<(SlotId, Option<SocketAddr>)> {
        let mut links: Vec<_> =
            self.registry.lock().unwrap().links().into_iter().map(|l| (l.slot, l.sender.peer_addr())).collect();
        links.sort_by_key(|l| l.0);
        links
    }

    /// Снимок состояния: общее состояние и живые дыры со счётчиками и потерями.
    pub fn status(&self) -> LinkStatus {
        LinkStatus {
            state: self.state.current(),
            holes: self.registry.lock().unwrap().holes(),
            reorder_wait_ms: self.reorder_wait_ms.load(Ordering::Relaxed),
            peer_registration: self.registrar.as_ref().and_then(|r| r.last_peer_registration()),
        }
    }

    pub fn my_peer_id(&self) -> Uuid {
        self.my_peer_id
    }

    pub fn peer_id(&self) -> Uuid {
        self.peer_id
    }

    /// hp-stats (фича `stats`, PLAN-ML.md): включает сбор статистики пакетов вниз на этом наборе
    /// дыр. Вызывается один раз сервером (`vps-server`) после `start_discovery`, когда задан
    /// `STATS_FILE`; без вызова (даже если бинарь собран с фичей) ничего не собирается и `pid`
    /// пакетам не ставится — нулевые накладные расходы на пути данных.
    #[cfg(feature = "stats")]
    pub fn attach_stats(&self, handle: hp_stats::StatsHandle) {
        *self.stats.lock().unwrap() = Some(handle);
    }

    /// Следующий номер пакета вниз, если сбор статистики включён; иначе `None` (пакет не
    /// нумеруется — ни на проводе, ни лишней работы здесь).
    #[cfg(feature = "stats")]
    fn next_pid(&self) -> Option<u32> {
        self.stats.lock().unwrap().is_some().then(|| self.pid_counter.fetch_add(1, Ordering::Relaxed))
    }

    #[cfg(not(feature = "stats"))]
    fn next_pid(&self) -> Option<u32> {
        None
    }

    /// Строит и отправляет в сборщик запись об отправленном пакете (hp-stats).
    #[cfg(feature = "stats")]
    fn record_sent(&self, pid: u32, link: &LiveLink, payload: &[u8], kind: hp_stats::MsgKind, client_id: Option<u8>, flow: Option<u32>) {
        let stats = self.stats.lock().unwrap().clone();
        let Some(stats) = stats else { return };
        // Приблизительная длина на проводе (подпись + protobuf-заголовки `Lite`): точная потребовала
        // бы прокидывать её из `wire::encode_*` наружу — для целей ML (какие пакеты режутся, а не
        // точный байт-каунтинг) оценки достаточно.
        const OVERHEAD_ESTIMATE: usize = crate::auth::AUTH_LEN + 16;
        let clamp_u16 = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
        stats.record_sent(hp_stats::SentRecord {
            pid,
            t_send_unix_ms: stats_feedback::unix_ms_now(),
            slot: u32::from(link.slot),
            via_relay: false, // релея пока нет (PLAN-dynamic-holes-relay.md, этап M7)
            local_port: link.sender.local_port(),
            dst_port: link.sender.peer_addr().map_or(0, |a| a.port()),
            wire_len: clamp_u16(payload.len() + OVERHEAD_ESTIMATE),
            payload_len: clamp_u16(payload.len()),
            inner: hp_stats::InnerInfo::parse(payload),
            kind,
            client_id,
            flow,
            hole_age_ms: u32::try_from(link.established_at.elapsed().as_millis()).unwrap_or(u32::MAX),
        });
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
                LinkStat { slot: l.slot, sent, received }
            })
            .collect();
        for link in &links {
            link.sender.send_stats(table.clone()).await;
        }
        log::debug!("отправлена статистика по {} дырам", table.len());
    }
}

/// hp-stats (фича `stats`, PLAN-ML.md §2.3): не больше стольких записей в одном `PidReport` —
/// грубый запас, чтобы при растущих номерах (крупные дельты — больше байт на варинт) сообщение
/// не вылезло за типичный MTU дыры (1500, из них часть уходит под подпись и заголовки `Lite`).
#[cfg(feature = "stats")]
const PID_REPORT_BATCH: usize = 64;
/// Как часто проверяем, не накопилось ли что отправить (ниже `PID_REPORT_BATCH` тоже шлём, но не
/// реже этого периода — PLAN-ML.md §2.3, «раз в ~50 мс»).
#[cfg(feature = "stats")]
const PID_REPORT_TICK: Duration = Duration::from_millis(50);
/// hp-stats §2.2: период проверки, не пора ли слать пробу синхронизации часов.
#[cfg(feature = "stats")]
const TIME_PROBE_TICK: Duration = Duration::from_millis(500);
/// Во сколько тиков `TIME_PROBE_TICK` укладывается ресинк после первого раунда (≈10 с, §2.2).
#[cfg(feature = "stats")]
const TIME_RESYNC_EVERY_N_TICKS: u32 = 20;

/// hp-stats: разбирает `PidReport` пира (клиента) и кормит сборщик (сервер).
#[cfg(feature = "stats")]
fn handle_pid_report(stats_cell: &Mutex<Option<hp_stats::StatsHandle>>, report: crate::proto::PidReport) {
    let Some(stats) = stats_cell.lock().unwrap().clone() else { return };
    let report_arrival_unix_ms = stats_feedback::unix_ms_now();
    let (ack_through, base) = (report.ack_through, report.recv_base_server_ms);
    for e in report.entries {
        stats.record_ack(hp_stats::RecvAck {
            pid: ack_through.wrapping_add(e.pid_delta),
            recv_server_ms: base.saturating_add(u64::from(e.recv_delta_ms)),
            reorder_wait_ms: u16::try_from(e.reorder_wait_ms).unwrap_or(u16::MAX),
            out_of_order: e.out_of_order,
            report_arrival_unix_ms,
        });
    }
}

/// hp-stats (клиент): отдельная задача — раз в `TIME_PROBE_TICK` решает, не пора ли слать пробу
/// синхронизации часов (часто в начале, пока не пройден раунд из `stats_feedback::INITIAL_PROBES`,
/// дальше — раз в `TIME_RESYNC_EVERY_N_TICKS` тиков, против дрейфа). Разбор ответов (`TimeEcho`) —
/// в `control_loop` (там приходят события с дыр), состояние — общее через `Mutex`.
#[cfg(feature = "stats")]
async fn time_probe_loop(registry: Arc<Mutex<LinkRegistry>>, time_sync: Arc<Mutex<stats_feedback::TimeSync>>) {
    let mut ticker = tokio::time::interval(TIME_PROBE_TICK);
    let mut ticks: u32 = 0;
    loop {
        ticker.tick().await;
        ticks += 1;
        let due = {
            let ts = time_sync.lock().unwrap();
            !ts.synced() || ticks.is_multiple_of(TIME_RESYNC_EVERY_N_TICKS)
        };
        if !due {
            continue;
        }
        let links = registry.lock().unwrap().links();
        let Some(link) = links.first() else { continue };
        let (t0, seq) = time_sync.lock().unwrap().send_probe();
        link.sender.send_time_probe(t0, seq).await;
    }
}

/// hp-stats (клиент): отдельная задача — раз в `PID_REPORT_TICK` отправляет накопленный
/// `PidReport`, если накопитель не пуст (досрочный сброс по `PID_REPORT_BATCH` — в `control_loop`,
/// где записи добавляются). Общее состояние — через `Mutex`.
#[cfg(feature = "stats")]
async fn pid_report_loop(registry: Arc<Mutex<LinkRegistry>>, pid_feedback: Arc<Mutex<stats_feedback::PidFeedbackBuilder>>) {
    let mut ticker = tokio::time::interval(PID_REPORT_TICK);
    loop {
        ticker.tick().await;
        let Some(report) = pid_feedback.lock().unwrap().take() else { continue };
        let links = registry.lock().unwrap().links();
        if let Some(link) = links.first() {
            link.sender.send_pid_report(report).await;
        }
    }
}

/// Разбирает события с дыр: статистику пира, `DeleteLink`, `Rendezvous` пира. Данные пира
/// проходят через буфер порядка (`reorder`), если `reorder_wait` не ноль.
#[allow(clippy::too_many_arguments)]
async fn control_loop(
    label: Label,
    mut events: mpsc::Receiver<LinkEvent>,
    registry: Arc<Mutex<LinkRegistry>>,
    redrop_txs: Vec<mpsc::Sender<()>>,
    hole_records: Option<mpsc::Sender<Rendezvous>>,
    (incoming, control): (mpsc::Sender<Incoming>, mpsc::Sender<Control>),
    options: MultiLinkOptions,
    reorder_wait_ms: Arc<AtomicU32>,
    #[cfg(feature = "stats")] stats_cell: Arc<Mutex<Option<hp_stats::StatsHandle>>>,
    #[cfg(feature = "stats")] time_sync: Arc<Mutex<stats_feedback::TimeSync>>,
    #[cfg(feature = "stats")] pid_feedback: Arc<Mutex<stats_feedback::PidFeedbackBuilder>>,
) {
    let MultiLinkOptions { reorder_wait, reorder_clients, .. } = options;
    let mut reorder = (!reorder_wait.is_zero()).then(|| Resequencer::adaptive(reorder_wait));
    let mut ready: Vec<Incoming> = Vec::with_capacity(64);
    // Дедупликация по `pid` (PLAN-dynamic-holes-relay.md, раздел 5): активна только для
    // пронумерованных пакетов — без включённого где-либо `pid` (hp-stats/будущий релей) никто не
    // шлёт `Some(pid)`, окно простаивает без накладных расходов. До буфера порядка, как в плане.
    let mut dedup = crate::dedup::DedupWindow::new();
    let mut stats_tick = tokio::time::interval(REORDER_STATS_INTERVAL);
    let mut logged = ReorderStats::default();
    loop {
        let deadline = reorder.as_ref().and_then(Resequencer::next_deadline);
        let sleep_until = deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        let event = tokio::select! {
            event = events.recv() => event,
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(sleep_until)), if deadline.is_some() => {
                if let Some(reorder) = reorder.as_mut() {
                    reorder.expire_into(Instant::now(), &mut ready);
                    for packet in ready.drain(..) {
                        let _ = incoming.send(packet).await;
                    }
                }
                continue;
            }
            _ = stats_tick.tick() => {
                if let Some(reorder) = reorder.as_ref() {
                    let now = reorder.stats();
                    reorder_wait_ms.store(now.wait_ms as u32, Ordering::Relaxed);
                    if now != logged {
                        log::info!(
                            "{label}порядок пакетов: ожидание {} мс, по порядку {}, переставлено {}, по таймауту {}, опоздавших {}, принудительно {}, прочих {}, макс. придержано {}",
                            now.wait_ms, now.in_order, now.reordered, now.timed_out, now.late, now.forced, now.passthrough, now.max_held
                        );
                        logged = now;
                    }
                }
                continue;
            }
        };
        let Some(event) = event else { break };
        match event {
            LinkEvent::PeerStats(peer_stats) => {
                log::debug!("получена статистика пира: {} дыр", peer_stats.len());
                for stat in peer_stats {
                    registry.lock().unwrap().record_report(&stat);
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
            // Динамический набор дыр (PLAN-dynamic-holes-relay.md, M5): пока только проводка —
            // никто `Drain` не инициирует (нет политики, какая дыра и когда сливается) и реестр
            // не умеет состояние `Draining`. Когда M5 появится, здесь будет: пометить дыру
            // `Draining` (не выбирать для отправки, приём — как у любой живой), ответить `Drain`
            // (идемпотентно), подождать `drain_grace` и закрыть. Сейчас — только лог.
            LinkEvent::PeerDrain { slot } => {
                log::info!("{label}слот {slot}: получен Drain (M5 ещё не реализован, без действия)");
            }
            LinkEvent::PeerData { slot, payload, pid } => {
                if !dedup.admit(pid) {
                    continue; // дубликат (второй путь — релей, PLAN-dynamic-holes-relay.md M7)
                }
                #[cfg(feature = "stats")]
                observe_pid(&pid_feedback, &time_sync, pid);
                deliver(&incoming, &mut reorder, Incoming { slot, payload, wrapped: None, order: None }, &mut ready).await;
            }
            LinkEvent::PeerWrapped { slot, seq, client_id, flow, payload, pid } => {
                if !dedup.admit(pid) {
                    continue;
                }
                #[cfg(feature = "stats")]
                observe_pid(&pid_feedback, &time_sync, pid);
                let Ok(client_id) = u8::try_from(client_id) else {
                    log::warn!("{label}слот {slot}: WrappedData с client_id {client_id} вне 0..=255");
                    continue;
                };
                let order = flow.map(|flow| (flow, seq));
                let packet = Incoming { slot, payload, wrapped: Some(WrappedInfo { client_id, seq }), order };
                if reorder_clients {
                    deliver(&incoming, &mut reorder, packet, &mut ready).await;
                } else {
                    let _ = incoming.send(packet).await;
                }
            }
            LinkEvent::PeerOrdered { slot, flow, seq, payload, pid } => {
                if !dedup.admit(pid) {
                    continue;
                }
                #[cfg(feature = "stats")]
                observe_pid(&pid_feedback, &time_sync, pid);
                deliver(&incoming, &mut reorder, Incoming { slot, payload, wrapped: None, order: Some((flow, seq)) }, &mut ready).await;
            }
            LinkEvent::PeerControl(message) => {
                if control.try_send(message).is_err() {
                    log::debug!("{label}служебное сообщение пира отброшено: его никто не читает");
                }
            }
            // Виртуал-брокер P2P: запись пира о другом слоте. В VPS-режиме по дырам не ходит.
            LinkEvent::PeerRendezvous(record) => match &hole_records {
                Some(tx) => {
                    let _ = tx.send(record).await;
                }
                None => log::debug!("{label}Rendezvous по дыре в VPS-режиме пропущен"),
            },
            // hp-stats (сервер): отчёт клиента о принятых номерах пакетов вниз.
            LinkEvent::PeerPidReport { report, .. } => {
                #[cfg(feature = "stats")]
                handle_pid_report(&stats_cell, report);
                #[cfg(not(feature = "stats"))]
                let _ = report;
            }
            // hp-stats (клиент): ответ сервера на нашу пробу синхронизации часов.
            LinkEvent::PeerTimeEcho { echo, .. } => {
                #[cfg(feature = "stats")]
                time_sync.lock().unwrap().on_echo(&echo);
                #[cfg(not(feature = "stats"))]
                let _ = echo;
            }
        }
        // hp-stats: партия PidReport выросла достаточно — не ждём таймер, шлём сейчас же (иначе
        // при высоком темпе приёма сообщение растёт неограниченно между тиками `pid_report_loop`).
        #[cfg(feature = "stats")]
        if pid_feedback.lock().unwrap().len() >= PID_REPORT_BATCH {
            let report = pid_feedback.lock().unwrap().take();
            if let Some(report) = report {
                let links = registry.lock().unwrap().links();
                if let Some(link) = links.first() {
                    link.sender.send_pid_report(report).await;
                }
            }
        }
    }
}

/// hp-stats: если пакет пронумерован (отправитель умеет и включил сбор), запоминает его в
/// накопителе `PidReport` — время приёма переводим в часы сервера через текущую `ServerTime`.
#[cfg(feature = "stats")]
fn observe_pid(pid_feedback: &Mutex<stats_feedback::PidFeedbackBuilder>, time_sync: &Mutex<stats_feedback::TimeSync>, pid: Option<u32>) {
    if let Some(pid) = pid {
        let recv_server_ms = time_sync.lock().unwrap().server_time().to_server_ms(stats_feedback::unix_ms_now());
        pid_feedback.lock().unwrap().observe(pid, recv_server_ms);
    }
}

/// Отдаёт пакет приложению: через буфер порядка (если включён) или сразу.
/// `ready` — переиспользуемый буфер выдачи (без выделения памяти на пакет).
async fn deliver(
    incoming: &mpsc::Sender<Incoming>,
    reorder: &mut Option<Resequencer>,
    packet: Incoming,
    ready: &mut Vec<Incoming>,
) {
    let Some(reorder) = reorder else {
        let _ = incoming.send(packet).await;
        return;
    };
    reorder.push_into(packet, Instant::now(), ready);
    for packet in ready.drain(..) {
        let _ = incoming.send(packet).await;
    }
}

const REORDER_STATS_INTERVAL: Duration = Duration::from_secs(30);

fn is_link_bad(registry: &Arc<Mutex<LinkRegistry>>, stat: &PeerLinkStat) -> bool {
    let my_received = registry.lock().unwrap().received_on(stat.slot);
    link_is_bad(stat.sent, my_received)
}

/// Чистое решение: пир отправил `peer_sent`, мы получили `my_received`.
fn link_is_bad(peer_sent: u64, my_received: u64) -> bool {
    peer_sent >= MIN_STATS_SAMPLE && my_received * 2 < peer_sent
}

fn request_redrop(redrop_txs: &[mpsc::Sender<()>], slot: SlotId) {
    if let Some(tx) = redrop_txs.get(slot as usize) {
        let _ = tx.try_send(());
    }
}

async fn broadcast_delete_link(registry: &Arc<Mutex<LinkRegistry>>, slot: SlotId) {
    let links = registry.lock().unwrap().links();
    for link in links {
        link.sender.send_delete_link(slot).await;
    }
}

/// Общее для рабочей задачи слота любого режима: реестр живых дыр, события дыр, фазы слота и
/// просьбы пробить дыру заново (плохая дыра, `DeleteLink` пира, `move_slot`).
pub(crate) struct SlotBase {
    pub slot: SlotId,
    pub label: Label,
    pub registry: Arc<Mutex<LinkRegistry>>,
    pub events: mpsc::Sender<LinkEvent>,
    state: Arc<StateTracker>,
    pub redrop_rx: mpsc::Receiver<()>,
}

impl SlotBase {
    pub(crate) fn phase(&self, phase: SlotPhase) {
        self.state.set(self.slot, phase);
    }

    /// Держит живую дыру в реестре, пока она не потеряна (`PeerLink::lost`), не помечена
    /// плохой или не завершился `interrupt` (режим сам решил её бросить). Затем убирает из
    /// реестра — слот начинает новую регистрацию.
    pub(crate) async fn hold(&mut self, link: PeerLink, interrupt: impl std::future::Future<Output = ()>) {
        let (slot, label) = (self.slot, &self.label);
        // Просьбы пробить заново, пришедшие до этой дыры, к ней не относятся.
        while self.redrop_rx.try_recv().is_ok() {}
        let link_id = link.link_id;
        {
            let mut reg = self.registry.lock().unwrap();
            reg.insert(
                link_id,
                LiveLink {
                    slot,
                    sender: link.sender.clone(),
                    #[cfg(feature = "stats")]
                    established_at: Instant::now(),
                },
            );
            log::info!("{label}слот {slot}: дыра открыта {} <-> {} (живых дыр: {})", link.local_addr, link.peer_addr, reg.live_count());
        }
        self.phase(SlotPhase::Connected);
        tokio::select! {
            _ = link.lost() => log::warn!("{label}слот {slot}: дыра потеряна, перерегистрация"),
            _ = self.redrop_rx.recv() => log::warn!("{label}слот {slot}: дыра помечена плохой, пробиваем заново"),
            _ = interrupt => log::info!("{label}слот {slot}: пир пришёл заново, перерегистрация"),
        }
        let mut reg = self.registry.lock().unwrap();
        reg.remove(link_id);
        log::debug!("{label}слот {slot}: дыра удалена из реестра (живых: {})", reg.live_count());
    }
}

/// Псевдослучайная длительность в [min, max] на основе текущего времени
/// (для джиттера качество ГПСЧ не важно, поэтому без внешних зависимостей).
fn jittered(min: Duration, max: Duration) -> Duration {
    let span = max.saturating_sub(min).as_millis().max(1) as u64;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64;
    min + Duration::from_millis(nanos % span)
}

/// Абортит задачу при дропе.
pub(crate) struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_needs_a_sample_and_is_clamped() {
        assert_eq!(loss(10, 5), None, "мало пакетов — оценки нет");
        assert_eq!(loss(100, 75), Some(0.25));
        assert_eq!(loss(100, 100), Some(0.0));
        // Пакеты в пути на момент отчёта могут дать «получено больше отправленного».
        assert_eq!(loss(100, 104), Some(0.0));
    }

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
        let live: Vec<SlotId> = (0..10).collect();
        let mut picker = SlotPicker::default();
        let mut rand = scripted(&[7, 3, 9, 0, 5, 5, 1, 2, 8, 4, 6]);
        for cycle in 0..3 {
            let mut got: Vec<SlotId> = (0..10).map(|_| picker.pick(&live, &mut rand).unwrap()).collect();
            got.sort_unstable();
            assert_eq!(got, live, "цикл {cycle}: каждая дыра ровно один раз");
        }
    }

    #[test]
    fn picker_order_follows_the_random_source_not_slot_order() {
        let live: [SlotId; 4] = [0, 1, 2, 3];
        let mut picker = SlotPicker::default();
        // Всегда берём последний из оставшихся: 0,1,2,3 -> 3, потом 2 ...
        let picked: Vec<SlotId> = (0..4).map(|_| picker.pick(&live, |n| n - 1).unwrap()).collect();
        assert_eq!(picked, [3, 2, 1, 0]);
    }

    #[test]
    fn picker_drops_dead_slots_and_adds_new_ones_next_cycle() {
        let mut picker = SlotPicker::default();
        assert!(picker.pick(&[1, 2, 3], |_| 0).is_some()); // убрали один из трёх
        // слот 2 умер, появился слот 9: 9 подключится только в следующем цикле
        let live: [SlotId; 3] = [1, 3, 9];
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
        let mut rng = XorShift32::seeded();
        let values: Vec<usize> = (0..200).map(|_| rng.below(10)).collect();
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

    /// TCP-пакет корзины 7 с номером `counter`.
    fn tcp_event(counter: u64) -> LinkEvent {
        LinkEvent::PeerOrdered { slot: (counter % 10) as SlotId, flow: 7, seq: counter, payload: Packet::copy_from(&[0x45u8; 48]).unwrap(), pid: None }
    }

    fn counter_of(packet: &Incoming) -> u64 {
        packet.order.unwrap().1
    }

    /// TCP-пакеты, пришедшие по разным дырам не по порядку, выходят к приложению
    /// по порядку; пропавший пакет ждём `reorder_wait`, потом отдаём остальное.
    #[tokio::test]
    async fn control_loop_restores_tcp_order_and_skips_a_missing_packet() {
        let (events_tx, events_rx) = mpsc::channel(16);
        let (incoming_tx, mut incoming_rx) = mpsc::channel(16);
        let wait = Duration::from_millis(60);
        tokio::spawn(control_loop(
            Label::new(""),
            events_rx,
            Arc::new(Mutex::new(LinkRegistry::default())),
            Vec::new(),
            None,
            (incoming_tx, mpsc::channel(4).0),
            MultiLinkOptions { reorder_wait: wait, ..MultiLinkOptions::default() },
            Arc::new(AtomicU32::new(0)),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(None)),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(stats_feedback::TimeSync::new())),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(stats_feedback::PidFeedbackBuilder::new())),
        ));

        for counter in [0u64, 2, 1, 3] {
            events_tx.send(tcp_event(counter)).await.unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(counter_of(&tokio::time::timeout(Duration::from_millis(30), incoming_rx.recv()).await.unwrap().unwrap()));
        }
        assert_eq!(got, vec![0, 1, 2, 3], "порядок восстановлен без ожидания таймаута");

        // 4 пропал: 5 и 6 придерживаются и выходят только после ожидания
        events_tx.send(tcp_event(5)).await.unwrap();
        events_tx.send(tcp_event(6)).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), incoming_rx.recv()).await.is_err());
        let first = tokio::time::timeout(Duration::from_millis(500), incoming_rx.recv()).await.unwrap().unwrap();
        let second = tokio::time::timeout(Duration::from_millis(50), incoming_rx.recv()).await.unwrap().unwrap();
        assert_eq!((counter_of(&first), counter_of(&second)), (5, 6));
    }

    /// Пакет с уже виденным `pid` (второй путь — например, релей, PLAN-dynamic-holes-relay.md
    /// M7) отбрасывается в `control_loop` до буфера порядка и до приложения; пакеты без `pid`
    /// дедупликации не подлежат (нынешнее поведение, пока никто `pid` не ставит).
    #[tokio::test]
    async fn control_loop_drops_a_duplicate_pid_but_passes_packets_without_one() {
        let (events_tx, events_rx) = mpsc::channel(16);
        let (incoming_tx, mut incoming_rx) = mpsc::channel(16);
        tokio::spawn(control_loop(
            Label::new(""),
            events_rx,
            Arc::new(Mutex::new(LinkRegistry::default())),
            Vec::new(),
            None,
            (incoming_tx, mpsc::channel(4).0),
            MultiLinkOptions { reorder_wait: Duration::ZERO, ..MultiLinkOptions::default() },
            Arc::new(AtomicU32::new(0)),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(None)),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(stats_feedback::TimeSync::new())),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(stats_feedback::PidFeedbackBuilder::new())),
        ));

        let data_event = |pid: Option<u32>| LinkEvent::PeerData { slot: 0, payload: Packet::copy_from(b"x").unwrap(), pid };
        events_tx.send(data_event(Some(42))).await.unwrap();
        events_tx.send(data_event(Some(42))).await.unwrap(); // дубликат — не должен дойти
        events_tx.send(data_event(Some(43))).await.unwrap();
        events_tx.send(data_event(None)).await.unwrap(); // без номера — дедупликация не применяется
        events_tx.send(data_event(None)).await.unwrap(); // тоже без номера — тоже проходит

        let mut got = 0;
        for _ in 0..4 {
            tokio::time::timeout(Duration::from_millis(100), incoming_rx.recv()).await.unwrap().unwrap();
            got += 1;
        }
        assert_eq!(got, 4, "4 пакета дошли (42, 43, None, None), дубликат 42 отброшен");
        assert!(
            tokio::time::timeout(Duration::from_millis(30), incoming_rx.recv()).await.is_err(),
            "лишних пакетов быть не должно"
        );
    }

    /// `reorder_wait = 0` отключает буфер: пакеты идут как пришли.
    #[tokio::test]
    async fn control_loop_without_reorder_forwards_in_arrival_order() {
        let (events_tx, events_rx) = mpsc::channel(16);
        let (incoming_tx, mut incoming_rx) = mpsc::channel(16);
        tokio::spawn(control_loop(
            Label::new(""),
            events_rx,
            Arc::new(Mutex::new(LinkRegistry::default())),
            Vec::new(),
            None,
            (incoming_tx, mpsc::channel(4).0),
            MultiLinkOptions { reorder_wait: Duration::ZERO, ..MultiLinkOptions::default() },
            Arc::new(AtomicU32::new(0)),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(None)),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(stats_feedback::TimeSync::new())),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(stats_feedback::PidFeedbackBuilder::new())),
        ));
        for counter in [0u64, 2, 1] {
            events_tx.send(tcp_event(counter)).await.unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(counter_of(&tokio::time::timeout(Duration::from_millis(100), incoming_rx.recv()).await.unwrap().unwrap()));
        }
        assert_eq!(got, vec![0, 2, 1]);
    }

    fn ordered_event(flow: u32, seq: u64, client_id: Option<u32>) -> LinkEvent {
        let payload = Packet::copy_from(&[0x45u8; 40]).unwrap();
        match client_id {
            Some(client_id) => LinkEvent::PeerWrapped { slot: 0, seq, client_id, flow: Some(flow), payload, pid: None },
            None => LinkEvent::PeerOrdered { slot: 0, flow, seq, payload, pid: None },
        }
    }

    /// Роутер (`reorder_clients = false`): пакеты клиентов идут насквозь как пришли, с корзиной и
    /// номером отправителя, а свой поток (`Ordered`) по-прежнему упорядочивается.
    #[tokio::test]
    async fn client_packets_pass_through_when_reorder_clients_is_off() {
        let (events_tx, events_rx) = mpsc::channel(16);
        let (incoming_tx, mut incoming_rx) = mpsc::channel(16);
        tokio::spawn(control_loop(
            Label::new(""),
            events_rx,
            Arc::new(Mutex::new(LinkRegistry::default())),
            Vec::new(),
            None,
            (incoming_tx, mpsc::channel(4).0),
            MultiLinkOptions { reorder_wait: Duration::from_millis(500), reorder_clients: false, ..MultiLinkOptions::default() },
            Arc::new(AtomicU32::new(0)),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(None)),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(stats_feedback::TimeSync::new())),
            #[cfg(feature = "stats")]
            Arc::new(Mutex::new(stats_feedback::PidFeedbackBuilder::new())),
        ));
        events_tx.send(ordered_event(3, 0, None)).await.unwrap();
        events_tx.send(ordered_event(3, 2, None)).await.unwrap();
        events_tx.send(ordered_event(3, 2, Some(7))).await.unwrap();
        events_tx.send(ordered_event(3, 0, Some(7))).await.unwrap();
        let mut got = Vec::new();
        for _ in 0..3 {
            let p = tokio::time::timeout(Duration::from_millis(100), incoming_rx.recv()).await.unwrap().unwrap();
            got.push((p.wrapped.map(|w| w.client_id), p.order));
        }
        assert_eq!(got, vec![(None, Some((3, 0))), (Some(7), Some((3, 2))), (Some(7), Some((3, 0)))]);
        // Свой пакет 2 придержан (ждали номер 1) и выходит по таймауту — после пакетов клиента.
        let held = tokio::time::timeout(Duration::from_millis(500), incoming_rx.recv()).await.unwrap().unwrap();
        assert_eq!((held.wrapped, held.order), (None, Some((3, 2))));
    }

    #[test]
    fn limit_slots_keeps_the_lowest_numbers_and_zero_means_all() {
        let limited = |mut v: Vec<SlotId>, max: u8| {
            let n = limit_slots(&mut v, max);
            v.truncate(n);
            v
        };
        assert_eq!(limited(vec![5, 1, 9, 3], 0), vec![1, 3, 5, 9]);
        assert_eq!(limited(vec![5, 1, 9, 3], 1), vec![1]);
        assert_eq!(limited(vec![5, 1, 9, 3], 2), vec![1, 3]);
        assert_eq!(limited(vec![4], 3), vec![4]);
        assert_eq!(limited(vec![], 1), Vec::<SlotId>::new());
    }

    async fn wait_until(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        while !ok() {
            assert!(tokio::time::Instant::now() < deadline, "не дождались: {what}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// VPS-режим целиком на loopback: знакомство через порт сервера, 10 дыр без STUN и
    /// MQTT, данные в обе стороны, перенос слота на новый порт сервера.
    #[tokio::test]
    async fn vps_server_and_client_link_all_slots_and_move_a_slot() {
        let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
        let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let options = MultiLinkOptions::default();
        let (server, mut server_rx) = MultiLink::start_discovery(
            "",
            Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap_port, ports: 47100..=47299 },
            server_id,
            client_id,
            options,
        )
        .await
        .unwrap();
        let (client, mut client_rx) = MultiLink::start_discovery(
            "",
            Discovery::VpsClient { server: SocketAddr::from(([127, 0, 0, 1], bootstrap_port)) },
            client_id,
            server_id,
            options,
        )
        .await
        .unwrap();

        wait_until("10 дыр с обеих сторон", 40, || server.live_count() == 10 && client.live_count() == 10).await;
        for (_, addr) in client.live_links() {
            let port = addr.unwrap().port();
            assert!((47100..=47299).contains(&port), "клиент ходит на порт из диапазона сервера: {port}");
        }

        client.send_data(b"ping").await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), server_rx.recv()).await.unwrap().unwrap();
        assert_eq!(got.payload, b"ping");
        server.send_data(b"pong").await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), client_rx.recv()).await.unwrap().unwrap();
        assert_eq!(got.payload, b"pong");

        // TUN-режим: пакеты с номером в потоке выходят у получателя по порядку.
        for seq in 0..20u64 {
            let mut packet = vec![0x45u8; 60];
            packet[59] = seq as u8;
            server.send_ordered(5, seq, &packet).await.unwrap();
        }
        let mut got = Vec::new();
        while got.len() < 20 {
            let p = tokio::time::timeout(Duration::from_secs(2), client_rx.recv()).await.unwrap().unwrap();
            got.push(p.order.unwrap());
        }
        assert_eq!(got, (0..20u64).map(|s| (5u32, s)).collect::<Vec<_>>());

        let old = client.live_links().into_iter().find(|l| l.0 == 3).unwrap().1.unwrap();
        server.move_slot(3).await;
        wait_until("слот 3 на новом порту сервера", 40, || {
            client.live_links().iter().any(|l| l.0 == 3 && l.1.is_some_and(|a| a != old)) && client.live_count() == 10
        })
        .await;
    }

    /// Клиент пропал (процесс убит) и пришёл снова с новыми сессиями, а сервер своих дыр ещё не
    /// потерял: сервер обязан отвечать на порту знакомства и принять нового клиента сразу, а не
    /// после тайм-аутов (раньше на этом сервер замолкал навсегда).
    #[tokio::test]
    async fn vps_server_accepts_a_restarted_client_at_once() {
        let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
        let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let server_discovery = || Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap_port, ports: 47300..=47499 };
        let client_discovery = || Discovery::VpsClient { server: SocketAddr::from(([127, 0, 0, 1], bootstrap_port)) };
        let options = MultiLinkOptions::default();
        let (server, _server_rx) = MultiLink::start_discovery("", server_discovery(), server_id, client_id, options).await.unwrap();

        let (first, _first_rx) = MultiLink::start_discovery("", client_discovery(), client_id, server_id, options).await.unwrap();
        wait_until("первый клиент: 10 дыр", 20, || server.live_count() == 10 && first.live_count() == 10).await;
        drop(first);

        // Потеря дыры на сервере — через 15 с тишины; новый клиент должен встать раньше.
        let (second, mut second_rx) = MultiLink::start_discovery("", client_discovery(), client_id, server_id, options).await.unwrap();
        wait_until("второй клиент: 10 дыр до тайм-аута потери", 10, || second.live_count() == 10).await;
        server.send_data(b"hello").await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), second_rx.recv()).await.unwrap().unwrap();
        assert_eq!(got.payload, b"hello");

    }

    /// hp-stats целиком на loopback (PLAN-ML.md): сервер нумерует пакеты вниз, клиент (сам
    /// синхронизировавшись по часам через `TimeProbe`/`TimeEcho`) шлёт обратно `PidReport`,
    /// сборщик сопоставляет и пишет в CSV строку `delivered=1` с разумным `flight_ms`. Проверяет
    /// сквозную проводку всех частей модуля, а не только их по отдельности (остальные тесты).
    #[cfg(feature = "stats")]
    #[tokio::test]
    async fn stats_pipeline_records_a_delivered_packet_end_to_end() {
        let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
        let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let options = MultiLinkOptions::default();
        let (server, _server_rx) = MultiLink::start_discovery(
            "",
            Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap_port, ports: 47600..=47799 },
            server_id,
            client_id,
            options,
        )
        .await
        .unwrap();
        let (client, _client_rx) = MultiLink::start_discovery(
            "",
            Discovery::VpsClient { server: SocketAddr::from(([127, 0, 0, 1], bootstrap_port)) },
            client_id,
            server_id,
            options,
        )
        .await
        .unwrap();
        wait_until("10 дыр с обеих сторон", 40, || server.live_count() == 10 && client.live_count() == 10).await;

        let dir = std::env::temp_dir().join(format!("hp-stats-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (handle, _collector_task) = hp_stats::spawn(hp_stats::CollectorConfig {
            out_dir: dir.clone(),
            loss_timeout: Duration::from_secs(2),
            sweep_interval: Duration::from_millis(50),
            ..hp_stats::CollectorConfig::default()
        })
        .unwrap();
        server.attach_stats(handle);

        // Несколько пакетов вниз — каждый получает номер и попадёт в CSV после PidReport клиента.
        for i in 0..5u8 {
            server.send_data(&[i; 10]).await.unwrap();
        }

        // PidReport клиента уходит раз в ~50 мс; даём время обратной связи дойти и записаться
        // (плюс начальный раунд синхронизации часов клиента — восемь проб по 500 мс).
        wait_until("CSV получил хотя бы одну доставленную строку", 15, || {
            read_csv_rows(&dir).iter().any(|r| r.get(2).map(String::as_str) == Some("1"))
        })
        .await;

        let rows = read_csv_rows(&dir);
        let delivered: Vec<&Vec<String>> = rows.iter().filter(|r| r[2] == "1").collect();
        assert!(!delivered.is_empty(), "хотя бы один пакет должен быть зафиксирован как доставленный");
        for row in &delivered {
            let flight_ms: i64 = row[3].parse().expect("flight_ms должен быть числом у доставленного пакета");
            assert!(flight_ms.abs() < 2000, "задержка на loopback должна быть разумной: {flight_ms} мс, строка {row:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Читает все строки (без заголовка) из всех CSV в каталоге сборщика.
    #[cfg(feature = "stats")]
    fn read_csv_rows(dir: &std::path::Path) -> Vec<Vec<String>> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut rows = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let Ok(content) = std::fs::read_to_string(entry.path()) else { continue };
            for line in content.lines().skip(1) {
                if !line.is_empty() {
                    rows.push(line.split(',').map(str::to_string).collect());
                }
            }
        }
        rows
    }
}
