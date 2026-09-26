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
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::codec::{self, XorKey};
use crate::pool::Packet;
use crate::label::Label;
use crate::link_id::PeerLinkId;
use crate::port_utils;
use crate::proto::{LinkStat, Rendezvous};
use crate::reorder::{ReorderStats, Resequencer};
use crate::punch::{self, LinkEvent, LinkSender, PeerIdentity, PeerLinkStat, PunchConfig};
use crate::rendezvous::{self, PeerSession, Registrar};
use crate::stun;
use crate::vps;

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
    /// Нагрузка в буфере из банка (`pool`): буфер вернётся в банк, когда пакет отправят дальше.
    pub payload: Packet,
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
struct SlotPicker {
    /// Ещё не использованные в этом цикле слоты (без выделения памяти: слотов ≤ `TARGET_LINKS`).
    remaining: [u8; TARGET_LINKS as usize],
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
    fn pick(&mut self, live: &[u8], mut random_below: impl FnMut(usize) -> usize) -> Option<u8> {
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
    fn pick_random(&mut self, live: &[u8]) -> Option<u8> {
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
    /// VPS-сервер, слот 0: запись отдаём клиенту на порту знакомства.
    VpsServer(Arc<vps::ServerBootstrap>),
    /// VPS-клиент, слот 0: запись шлём серверу на порт знакомства.
    VpsClient(Arc<vps::ClientBootstrap>),
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

/// Как слот получает свои адреса и стучится к пиру.
#[derive(Clone)]
enum SlotMode {
    Stun(Vec<SocketAddr>),
    VpsServer { public_ip: IpAddr, bootstrap_port: u16, ports: RangeInclusive<u16> },
    VpsClient,
}

/// Снимает запись слота 0 сервера, когда анонс больше не нужен.
struct ClearOnDrop(Arc<vps::ServerBootstrap>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.set(None);
    }
}

/// Запущенный менеджер.
/// Сколько ждём недостающий пакет WireGuard, прежде чем отдать накопленное дальше.
pub const DEFAULT_REORDER_WAIT: Duration = Duration::from_millis(8);

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
}

impl Default for MultiLinkOptions {
    fn default() -> Self {
        Self { reorder_wait: DEFAULT_REORDER_WAIT, data_holes: 0, local_port_base: 0 }
    }
}

/// Локальный порт слота: `base + slot` или 0 (выбирает ОС), если `base` не задан.
fn slot_port(base: u16, slot: u8) -> u16 {
    if base == 0 { 0 } else { base.saturating_add(u16::from(slot)) }
}

/// Оставляет для отправки не больше `max` слотов с наименьшими номерами (0 — все); сортирует
/// на месте, возвращает, сколько слотов оставить.
fn limit_slots(slots: &mut [u8], max: u8) -> usize {
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

    /// Запуск с явным способом знакомства (обычный или VPS).
    pub async fn start_discovery(
        label: &str,
        discovery: Discovery,
        my_peer_id: Uuid,
        peer_id: Uuid,
        options: MultiLinkOptions,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        let label = Label::new(label);
        let registry = Arc::new(Mutex::new(LinkRegistry::default()));
        let (slot0_announce, mode, mqtt_rx, punch) = match &discovery {
            Discovery::StunMqtt { stun_addrs, mqtt_addr, mqtt_ca_pem } => {
                let (registrar, peer_rx) =
                    rendezvous::connect(label.clone(), *mqtt_addr, mqtt_ca_pem.clone(), my_peer_id, peer_id)
                        .await
                        .context("не удалось подключиться к MQTT-брокеру")?;
                (Announce::Mqtt(Arc::new(registrar)), SlotMode::Stun(stun_addrs.clone()), Some(peer_rx), PunchConfig::default())
            }
            Discovery::VpsServer { public_ip, bootstrap_port, ports } => (
                Announce::VpsServer(Arc::new(vps::ServerBootstrap::default())),
                SlotMode::VpsServer { public_ip: *public_ip, bootstrap_port: *bootstrap_port, ports: ports.clone() },
                None,
                PunchConfig::default(),
            ),
            Discovery::VpsClient { server } => (
                Announce::VpsClient(Arc::new(
                    vps::ClientBootstrap::new(*server, vps::bootstrap_key(my_peer_id, peer_id)).await?,
                )),
                SlotMode::VpsClient,
                None,
                PunchConfig { margin: 0, ..PunchConfig::default() },
            ),
        };

        let (events_tx, events_rx) = mpsc::channel::<LinkEvent>(64);
        let (incoming_tx, incoming_rx) = mpsc::channel::<Incoming>(64);
        let state = Arc::new(StateTracker::new(TARGET_LINKS as usize, label.clone()));

        let mut slot_txs: Vec<mpsc::Sender<PeerSession>> = Vec::new();
        let mut redrop_txs: Vec<mpsc::Sender<()>> = Vec::new();
        for slot in 0..TARGET_LINKS {
            let port = match mode {
                SlotMode::Stun(_) => slot_port(options.local_port_base, slot),
                // В VPS-режимах слот занимает порт заново на каждую регистрацию.
                _ => 0,
            };
            let socket = Arc::new(
                UdpSocket::bind(("0.0.0.0", port))
                    .await
                    .with_context(|| format!("не удалось создать сокет для слота {slot}"))?,
            );
            let (slot_tx, slot_rx) = mpsc::channel::<PeerSession>(8);
            let (redrop_tx, redrop_rx) = mpsc::channel::<()>(1);
            slot_txs.push(slot_tx);
            redrop_txs.push(redrop_tx);
            let announce = if slot == BOOTSTRAP_SLOT {
                slot0_announce.clone()
            } else {
                Announce::VirtualBroker(registry.clone())
            };
            tokio::spawn(slot_worker(SlotCtx {
                slot,
                socket,
                mode: mode.clone(),
                my_peer_id,
                peer_id,
                announce,
                registry: registry.clone(),
                punch: punch.clone(),
                peer_rx: slot_rx,
                redrop_rx,
                events: events_tx.clone(),
                state: state.clone(),
                label: label.clone(),
            }));
        }

        match (&slot0_announce, &discovery) {
            (Announce::VpsServer(boot), Discovery::VpsServer { bootstrap_port, .. }) => {
                let socket = UdpSocket::bind(("0.0.0.0", *bootstrap_port))
                    .await
                    .with_context(|| format!("не удалось занять порт знакомства {bootstrap_port}"))?;
                log::info!("{label}VPS-сервер: порт знакомства {bootstrap_port}");
                tokio::spawn(vps::serve_bootstrap(
                    socket,
                    boot.clone(),
                    vps::bootstrap_key(my_peer_id, peer_id),
                    peer_id,
                    slot_txs[BOOTSTRAP_SLOT as usize].clone(),
                ));
            }
            (Announce::VpsClient(boot), _) => {
                log::info!("{label}VPS-клиент: сервер {}", boot.server);
                let (boot, tx) = (boot.clone(), slot_txs[BOOTSTRAP_SLOT as usize].clone());
                tokio::spawn(async move { boot.receive(peer_id, tx).await });
            }
            _ => {}
        }
        if let Some(peer_rx) = mqtt_rx {
            tokio::spawn(demux(peer_rx, slot_txs.clone()));
        }
        let redrop_for_api = redrop_txs.clone();
        tokio::spawn(keepalive_loop(registry.clone()));
        tokio::spawn(stats_loop(registry.clone()));
        tokio::spawn(control_loop(
            label.clone(),
            events_rx,
            registry.clone(),
            redrop_txs,
            slot_txs,
            incoming_tx,
            options.reorder_wait,
        ));

        let multilink = Self {
            registry,
            my_peer_id,
            peer_id,
            picker: Mutex::new(SlotPicker::default()),
            wrap_seq: Mutex::new(SeqCounters::default()),
            data_holes: options.data_holes,
            redrop_txs: redrop_for_api,
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
        let mut slots = [0u8; TARGET_LINKS as usize];
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
    pub async fn send_data(&self, payload: &[u8]) -> Result<u8> {
        let link = self.choose_link(payload.len())?;
        link.sender.send_data(payload).await;
        Ok(link.slot)
    }

    /// Оборачивает `payload` в `WrappedData` для клиента `client_id` (с его
    /// порядковым номером) и отправляет по одной из живых дыр. Возвращает номер
    /// дыры и присвоенный `seq`.
    pub async fn send_wrapped(&self, client_id: u8, payload: &[u8]) -> Result<(u8, u64)> {
        let link = self.choose_link(payload.len())?;
        let seq = self.wrap_seq.lock().unwrap().next(client_id);
        link.sender.send_wrapped(seq, u32::from(client_id), payload).await;
        Ok((link.slot, seq))
    }

    pub fn live_count(&self) -> usize {
        self.registry.lock().unwrap().live_count()
    }

    /// Перенести дыру `slot` на новые порты: слот регистрируется заново (в VPS-режиме —
    /// на новом порту сервера), пиру уходит `DeleteLink`, чтобы и он бросил старую.
    pub async fn move_slot(&self, slot: u8) {
        request_redrop(&self.redrop_txs, slot);
        broadcast_delete_link(&self.registry, slot).await;
    }

    /// Живые дыры: слот и текущий адрес пира.
    pub fn live_links(&self) -> Vec<(u8, Option<SocketAddr>)> {
        let mut links: Vec<_> =
            self.registry.lock().unwrap().links().into_iter().map(|l| (l.slot, l.sender.peer_addr())).collect();
        links.sort_by_key(|l| l.0);
        links
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

/// Разбирает события с дыр: статистику пира, `DeleteLink`, `Rendezvous` пира. Данные пира
/// проходят через буфер порядка (`reorder`), если `reorder_wait` не ноль.
async fn control_loop(
    label: Label,
    mut events: mpsc::Receiver<LinkEvent>,
    registry: Arc<Mutex<LinkRegistry>>,
    redrop_txs: Vec<mpsc::Sender<()>>,
    slot_txs: Vec<mpsc::Sender<PeerSession>>,
    incoming: mpsc::Sender<Incoming>,
    reorder_wait: Duration,
) {
    let mut reorder = (!reorder_wait.is_zero()).then(|| Resequencer::adaptive(reorder_wait));
    let mut ready: Vec<Incoming> = Vec::with_capacity(64);
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
                deliver(&incoming, &mut reorder, Incoming { slot, payload, wrapped: None }, &mut ready).await;
            }
            LinkEvent::PeerWrapped { slot, seq, client_id, payload } => {
                let Ok(client_id) = u8::try_from(client_id) else {
                    log::warn!("{label}слот {slot}: WrappedData с client_id {client_id} вне 0..=255");
                    continue;
                };
                let packet = Incoming { slot, payload, wrapped: Some(WrappedInfo { client_id, seq }) };
                deliver(&incoming, &mut reorder, packet, &mut ready).await;
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
    mode: SlotMode,
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

        // VPS-режимы: на каждую регистрацию слот занимает новый порт (плохую дыру
        // переносим на другой порт, у клиента — новый локальный адрес).
        let rebound = match &ctx.mode {
            SlotMode::Stun(_) => None,
            SlotMode::VpsServer { ports, bootstrap_port, .. } => Some(vps::bind_random_port(ports, *bootstrap_port).await),
            SlotMode::VpsClient => Some(UdpSocket::bind(("0.0.0.0", 0)).await.context("сокет слота")),
        };
        match rebound {
            Some(Ok(socket)) => ctx.socket = Arc::new(socket),
            Some(Err(e)) => {
                log::warn!("{label}слот {}: не удалось занять порт: {e:#}; повтор", ctx.slot);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            None => {}
        }
        let local_port = ctx.socket.local_addr().map(|a| a.port()).unwrap_or(0);

        let my_endpoints = match &ctx.mode {
            SlotMode::Stun(stun_addrs) => match observe_endpoints(&ctx.socket, stun_addrs).await {
                Ok(endpoints) => endpoints,
                Err(e) => {
                    log::warn!("{label}слот {}: STUN не удался: {e:#}; повтор", ctx.slot);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            },
            SlotMode::VpsServer { public_ip, .. } => vec![SocketAddr::new(*public_ip, local_port)],
            SlotMode::VpsClient => vec![SocketAddr::from(([0, 0, 0, 0], local_port))],
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
        let candidates = match &ctx.mode {
            SlotMode::Stun(_) => {
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
                candidates
            }
            SlotMode::VpsServer { .. } => {
                log::info!("{label}слот {}: ждём клиента на порту {local_port}", ctx.slot);
                Vec::new()
            }
            SlotMode::VpsClient => {
                log::info!("{label}слот {}: идём на порт сервера {}", ctx.slot, peer.addr);
                vec![peer.addr]
            }
        };

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
        Announce::VpsServer(boot) => {
            boot.set(Some(our_rendezvous(my_peer_id, slot, session, endpoints, key)));
            let guard = ClearOnDrop(boot.clone());
            Ok(AbortOnDrop(tokio::spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            })))
        }
        Announce::VpsClient(boot) => {
            let (boot, record) = (boot.clone(), our_rendezvous(my_peer_id, slot, session, endpoints, key));
            Ok(AbortOnDrop(tokio::spawn(async move { boot.announce(record).await })))
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

    fn wg_event(counter: u64) -> LinkEvent {
        let mut payload = vec![0u8; 48];
        payload[0] = 4;
        payload[4..8].copy_from_slice(&7u32.to_le_bytes());
        payload[8..16].copy_from_slice(&counter.to_le_bytes());
        LinkEvent::PeerData { slot: (counter % 10) as u8, payload: Packet::copy_from(&payload).unwrap() }
    }

    fn counter_of(packet: &Incoming) -> u64 {
        crate::reorder::parse_transport(&packet.payload).unwrap().1
    }

    /// Пакеты WireGuard, пришедшие по разным дырам не по порядку, выходят к приложению
    /// по порядку; пропавший пакет ждём `reorder_wait`, потом отдаём остальное.
    #[tokio::test]
    async fn control_loop_restores_wireguard_order_and_skips_a_missing_packet() {
        let (events_tx, events_rx) = mpsc::channel(16);
        let (incoming_tx, mut incoming_rx) = mpsc::channel(16);
        let wait = Duration::from_millis(60);
        tokio::spawn(control_loop(
            Label::new(""),
            events_rx,
            Arc::new(Mutex::new(LinkRegistry::default())),
            Vec::new(),
            Vec::new(),
            incoming_tx,
            wait,
        ));

        for counter in [0u64, 2, 1, 3] {
            events_tx.send(wg_event(counter)).await.unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(counter_of(&tokio::time::timeout(Duration::from_millis(30), incoming_rx.recv()).await.unwrap().unwrap()));
        }
        assert_eq!(got, vec![0, 1, 2, 3], "порядок восстановлен без ожидания таймаута");

        // 4 пропал: 5 и 6 придерживаются и выходят только после ожидания
        events_tx.send(wg_event(5)).await.unwrap();
        events_tx.send(wg_event(6)).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(20), incoming_rx.recv()).await.is_err());
        let first = tokio::time::timeout(Duration::from_millis(500), incoming_rx.recv()).await.unwrap().unwrap();
        let second = tokio::time::timeout(Duration::from_millis(50), incoming_rx.recv()).await.unwrap().unwrap();
        assert_eq!((counter_of(&first), counter_of(&second)), (5, 6));
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
            Vec::new(),
            incoming_tx,
            Duration::ZERO,
        ));
        for counter in [0u64, 2, 1] {
            events_tx.send(wg_event(counter)).await.unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(counter_of(&tokio::time::timeout(Duration::from_millis(100), incoming_rx.recv()).await.unwrap().unwrap()));
        }
        assert_eq!(got, vec![0, 2, 1]);
    }

    #[test]
    fn limit_slots_keeps_the_lowest_numbers_and_zero_means_all() {
        let limited = |mut v: Vec<u8>, max: u8| {
            let n = limit_slots(&mut v, max);
            v.truncate(n);
            v
        };
        assert_eq!(limited(vec![5, 1, 9, 3], 0), vec![1, 3, 5, 9]);
        assert_eq!(limited(vec![5, 1, 9, 3], 1), vec![1]);
        assert_eq!(limited(vec![5, 1, 9, 3], 2), vec![1, 3]);
        assert_eq!(limited(vec![4], 3), vec![4]);
        assert_eq!(limited(vec![], 1), Vec::<u8>::new());
    }

    #[test]
    fn slot_ports_follow_the_base_or_stay_random() {
        assert_eq!(slot_port(0, 3), 0);
        assert_eq!(slot_port(51410, 0), 51410);
        assert_eq!(slot_port(51410, 9), 51419);
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

        let old = client.live_links().into_iter().find(|l| l.0 == 3).unwrap().1.unwrap();
        server.move_slot(3).await;
        wait_until("слот 3 на новом порту сервера", 40, || {
            client.live_links().iter().any(|l| l.0 == 3 && l.1.is_some_and(|a| a != old)) && client.live_count() == 10
        })
        .await;
    }
}
