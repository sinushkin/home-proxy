//! P2P-режим: оба пира за NAT, знакомство через STUN + MQTT (первая дыра) и по живым дырам
//! (виртуал-брокер), дыры пробиваются.
//!
//! **Набор дыр динамический** (как в VPS-режиме, `vps.rs`): дыра «поработала — умерла», номер
//! (`SlotId`) монотонный и не переиспользуется. **Ролей нет**: каждая сторона сама держит набор
//! (`holes::plan` по всем дырам) и открывает дыру, когда ей нужно; кто начал — тот начал. Пир,
//! увидев запись с неизвестным номером, открывает ответную дыру, если у него сейчас меньше
//! `max_total` дыр (иначе пропускает). Сливает дыру (`Drain`) любая сторона, слив — по
//! подтверждению (`SlotBase::drain`).
//!
//! Задача **одной дыры** (один проход, одинакова для обеих сторон):
//!
//! ```text
//!   Starting: свой сокет, новая сессия, STUN → свои внешние адреса
//!        │
//!   анонс: запись дыры уходит пиру по живым дырам (виртуал-брокер), а пока живых нет —
//!   в MQTT (bootstrap); держится, пока дыра не залинкована
//!        │
//!   Rendezvous: ждём запись пира для этого номера (у открывшей в ответ она уже есть)
//!        │
//!   Punching: пробив к его адресам; пришла запись новее — к ней; HOLE_DEADLINE вышел — дыра
//!        │    закрыта, замену откроет политика набора
//!   Connected: SlotBase::hold — пока дыра не потеряна и не слита (Drain)
//! ```
//!
//! Записи пира приходят из MQTT и по дырам (`Rendezvous` в `Lite`), их раскладывает `Manager`
//! по номерам дыр: известной дыре — в её канал, неизвестный номер — повод открыть ответную дыру. Дыра, по которой ответы не доходят, не используется для данных, пока пир не
//! подтвердит путь (`SlotBase::hold`, `confirm_peer`).

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::auth::PairSecret;
use crate::holes::{plan, Action, HoleInfo, HoleState, PoolPolicy};
use crate::label::Label;
use crate::multilink::{jittered, send_command, AbortOnDrop, HoleCmd, HoleFactory, LinkRegistry, SlotBase, SlotId, SlotPhase};
use crate::port_pool::{PortLease, PortPool};
use crate::port_utils;
use crate::proto::Rendezvous;
use crate::punch::{self, PeerIdentity, PunchConfig};
use crate::rendezvous::{self, PeerSession};

pub mod mqtt;
pub mod stun;

use self::mqtt::Registrar;

/// Сколько живёт одна дыра от начала знакомства до линка: STUN, запись пира и пробив. Не
/// получилось — дыра закрывается, политика набора открывает другую.
const HOLE_DEADLINE: Duration = Duration::from_secs(90);

/// Сколько дыра ждёт запись пира, прежде чем закрыться: ответ приходит за секунды, а молчит пир,
/// когда у него набор полон (ответную дыру он не открыл) — тогда незачем занимать место.
const RECORD_WAIT: Duration = Duration::from_secs(15);

/// Как часто обновляем запись дыры в MQTT, пока живых дыр нет и дыра не залинкована (TTL записи 60 с).
const REPUBLISH_INTERVAL: Duration = Duration::from_secs(30);

/// Как часто дыра шлёт свою запись по живым дырам (виртуал-брокер), пока не залинкована.
const HOLE_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(3);

/// Как часто зовём политику набора.
const MANAGE_INTERVAL: Duration = Duration::from_secs(1);

/// Сколько после запуска не открываем свои дыры: пир, который стартовал раньше, уже ждёт нас
/// (его записи приходят из MQTT сразу после подписки) — открываем ответные, а не вторую свою
/// четвёрку. Пира нет — через это время открываем сами.
const START_GRACE: Duration = Duration::from_secs(3);

/// Сколько закрытых дыр (номер, сессия пира) помним: запоздалая запись не открывает дыру заново.
const CLOSED_MEMORY: usize = 256;

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
    /// Если не 0 — локальные порты дыр берутся из `base..=base+99` (брандмауэр пропускает входящий
    /// UDP только в известном диапазоне); 0 — порт каждой дыры выбирает ОС.
    pub local_port_base: u16,
    /// Политика набора дыр и срок жизни дыры.
    pub pool: PoolPolicy,
    pub hole_age: (Duration, Duration),
}

/// Запущенный P2P-режим: задача менеджера и приёмник записей пира, пришедших по дырам.
pub(crate) struct Started {
    pub tasks: Vec<AbortOnDrop>,
    /// Сюда `control_loop` отдаёт `Rendezvous` пира, пришедшие по дырам (виртуал-брокер).
    pub hole_records: mpsc::Sender<Rendezvous>,
    pub registrar: Option<Arc<Registrar>>,
}

/// Канал знакомства, когда живых дыр ещё нет: публикация записи дыры для пира.
#[derive(Clone)]
pub(crate) enum Bootstrap {
    /// MQTT-брокер: retained-запись с TTL.
    Mqtt(Arc<Registrar>),
    /// В тестах: записи сразу уходят в канал другой стороны.
    #[cfg(test)]
    Memory(mpsc::Sender<PeerSession>),
}

impl Bootstrap {
    async fn publish(&self, slot: SlotId, session: Uuid, endpoints: &[SocketAddr]) -> Result<()> {
        match self {
            Bootstrap::Mqtt(registrar) => registrar.publish_slot(slot, session, endpoints).await,
            #[cfg(test)]
            Bootstrap::Memory(tx) => {
                let session = PeerSession {
                    slot,
                    session_id: session,
                    addr: endpoints[0],
                    extra: endpoints[1..].to_vec(),
                    registered_at_unix_ms: 0,
                };
                tx.try_send(session).map_err(|e| anyhow::anyhow!("{e}"))
            }
        }
    }
}

/// Подключается к MQTT и запускает менеджер набора дыр.
pub(crate) async fn start(label: &Label, config: Config, factory: HoleFactory) -> Result<Started> {
    let (registrar, mqtt_rx) =
        mqtt::connect(label.clone(), config.mqtt_addr, config.mqtt_ca_pem.clone(), config.my_peer_id, config.peer_id)
            .await
            .context("не удалось подключиться к MQTT-брокеру")?;
    let registrar = Arc::new(registrar);
    Ok(start_with(label, config, factory, Bootstrap::Mqtt(registrar.clone()), mqtt_rx, Some(registrar)))
}

/// То же с готовым каналом знакомства (в тестах — без брокера).
pub(crate) fn start_with(
    label: &Label,
    config: Config,
    factory: HoleFactory,
    bootstrap: Bootstrap,
    incoming: mpsc::Receiver<PeerSession>,
    registrar: Option<Arc<Registrar>>,
) -> Started {
    log::info!("{label}P2P: динамический набор дыр, в работе {}..{}", config.pool.min_active, config.pool.max_total);
    let (hole_tx, hole_rx) = mpsc::channel::<Rendezvous>(16);
    let (ended_tx, ended_rx) = mpsc::unbounded_channel();
    let ports = (config.local_port_base != 0).then(|| PortPool::new(config.local_port_base..=config.local_port_base.saturating_add(99)));
    let first_id = u32::from_le_bytes(Uuid::new_v4().into_bytes()[..4].try_into().expect("4 байта")) % 1_000_000;
    let manager = Manager {
        // Номера монотонные; старт случайный (до миллиона — короткие номера в статусе).
        core: Core::new(config.pool, first_id, Instant::now()),
        tasks: HashMap::new(),
        hole_age: config.hole_age,
        stun_addrs: config.stun_addrs,
        my_peer_id: config.my_peer_id,
        peer_id: config.peer_id,
        pair: config.pair,
        bind_ip: config.bind_ip,
        bind_ifindex: config.bind_ifindex,
        ports,
        factory,
        bootstrap,
        ended_tx,
    };
    let task = AbortOnDrop(tokio::spawn(manager.run(incoming, hole_rx, ended_rx)));
    Started { tasks: vec![task], hole_records: hole_tx, registrar }
}

/// Запись ядра о дыре.
struct Record {
    opened: Instant,
    max_age: Duration,
    /// Последняя сессия пира, пришедшая для этой дыры (в память о закрытых).
    peer_session: Option<Uuid>,
}

/// Что ядро просит сделать оболочку.
#[derive(Debug, PartialEq)]
enum Effect {
    /// Открыть дыру (`first_peer` — запись пира, на которую отвечаем; `None` — открываем мы).
    Spawn { id: SlotId, first_peer: Option<PeerSession> },
    /// Передать запись пира уже открытой дыре.
    Forward { id: SlotId, session: PeerSession },
    /// Слить дыру (политика набора).
    Retire(SlotId),
}

/// Решения менеджера набора — чистое ядро без сети, задач и часов: время и случайность приходят
/// параметрами, ответ — список `Effect`, поэтому всё проверяется юнит-тестами.
struct Core {
    pool: PoolPolicy,
    holes: HashMap<SlotId, Record>,
    closed: VecDeque<(SlotId, Uuid)>,
    next_id: SlotId,
    last_open: Instant,
    started: Instant,
    max_holes: usize,
}

impl Core {
    fn new(pool: PoolPolicy, first_id: SlotId, now: Instant) -> Self {
        Self {
            pool,
            holes: HashMap::new(),
            closed: VecDeque::new(),
            next_id: first_id,
            // Рост сверх минимума — не раньше `add_interval` после старта: пир мог прислать ответные дыры.
            last_open: now,
            started: now,
            max_holes: pool.max_total * 2,
        }
    }

    /// Записи для снимка набора (`holes::snapshot`).
    fn records(&self) -> impl Iterator<Item = (SlotId, Instant, Duration)> + '_ {
        self.holes.iter().map(|(&id, r)| (id, r.opened, r.max_age))
    }

    /// Запись пира: известной дыре — передать; неизвестный номер — открыть ответную дыру, если у
    /// нас сейчас меньше `max_total` дыр с известным пиром; закрытую (номер, сессия) повторно не
    /// открываем. Свои дыры, на которые пир ещё не ответил, место не занимают: иначе на последнем
    /// месте обе стороны открывают по дыре одновременно, каждая считает набор полным и отвергает
    /// запись другой — навсегда. Лучше выйдет на одну-две дыры больше `max_total`, чем тупик.
    fn on_peer(&mut self, now: Instant, session: PeerSession, pick_age: &mut impl FnMut() -> Duration) -> Option<Effect> {
        let id = session.slot;
        if let Some(rec) = self.holes.get_mut(&id) {
            rec.peer_session = Some(session.session_id);
            return Some(Effect::Forward { id, session });
        }
        let matched = self.holes.values().filter(|r| r.peer_session.is_some()).count();
        if self.closed.contains(&(id, session.session_id)) || matched >= self.pool.max_total {
            return None;
        }
        self.holes.insert(id, Record { opened: now, max_age: pick_age(), peer_session: Some(session.session_id) });
        Some(Effect::Spawn { id, first_peer: Some(session) })
    }

    /// Задача дыры закончилась.
    fn on_ended(&mut self, id: SlotId) {
        if let Some(rec) = self.holes.remove(&id)
            && let Some(session) = rec.peer_session
        {
            self.closed.push_back((id, session));
            if self.closed.len() > CLOSED_MEMORY {
                self.closed.pop_front();
            }
        }
    }

    /// Политика набора по снимку `infos` (состояние и потери — из реестра): открыть или слить.
    fn evaluate(&mut self, now: Instant, infos: &[HoleInfo], pick_age: &mut impl FnMut() -> Duration) -> Vec<Effect> {
        if now.duration_since(self.started) < START_GRACE {
            return Vec::new();
        }
        let mut effects = Vec::new();
        for action in plan(infos, now.duration_since(self.last_open), &self.pool) {
            match action {
                Action::Open if self.holes.len() < self.max_holes => {
                    // Номер не должен совпасть с уже занятым (пир мог взять тот же).
                    while self.holes.contains_key(&self.next_id) {
                        self.next_id = self.next_id.wrapping_add(1);
                    }
                    let id = self.next_id;
                    self.next_id = self.next_id.wrapping_add(1);
                    self.last_open = now;
                    self.holes.insert(id, Record { opened: now, max_age: pick_age(), peer_session: None });
                    effects.push(Effect::Spawn { id, first_peer: None });
                }
                Action::Open => {}
                Action::Retire(id) => effects.push(Effect::Retire(id)),
            }
        }
        effects
    }
}

/// Задача дыры и канал записей пира для неё.
struct HoleTask {
    peer_tx: mpsc::Sender<PeerSession>,
    _task: AbortOnDrop,
}

/// Менеджер набора дыр: оболочка вокруг `Core` — принимает записи пира (MQTT и по дырам), раз в
/// секунду зовёт политику и исполняет решения ядра (заводит задачи дыр, сливает).
struct Manager {
    core: Core,
    tasks: HashMap<SlotId, HoleTask>,
    hole_age: (Duration, Duration),
    stun_addrs: Vec<SocketAddr>,
    my_peer_id: Uuid,
    peer_id: Uuid,
    pair: PairSecret,
    bind_ip: IpAddr,
    bind_ifindex: Option<u32>,
    ports: Option<Arc<PortPool>>,
    factory: HoleFactory,
    bootstrap: Bootstrap,
    ended_tx: mpsc::UnboundedSender<SlotId>,
}

impl Manager {
    async fn run(
        mut self,
        mut mqtt: mpsc::Receiver<PeerSession>,
        mut holes_rx: mpsc::Receiver<Rendezvous>,
        mut ended: mpsc::UnboundedReceiver<SlotId>,
    ) {
        let mut ticker = tokio::time::interval(MANAGE_INTERVAL);
        loop {
            tokio::select! {
                _ = ticker.tick() => self.evaluate(),
                Some(session) = mqtt.recv() => self.on_peer(session),
                Some(record) = holes_rx.recv() => match rendezvous::peer_session_from(&record, &self.pair, self.peer_id) {
                    Ok(session) => self.on_peer(session),
                    Err(e) => log::warn!("{}некорректный Rendezvous по дыре: {e:#}", self.factory.label),
                },
                Some(id) = ended.recv() => {
                    self.tasks.remove(&id);
                    self.core.on_ended(id);
                    // Не ждём очередного тика: замена нужна сразу, если дыр не хватает.
                    self.evaluate();
                }
            }
        }
    }

    fn pick_age(&self) -> impl FnMut() -> Duration + use<> {
        let (min, max) = self.hole_age;
        move || jittered(min, max)
    }

    fn on_peer(&mut self, session: PeerSession) {
        let now = Instant::now();
        let effect = self.core.on_peer(now, session, &mut self.pick_age());
        self.execute(effect.into_iter().collect());
    }

    fn evaluate(&mut self) {
        let now = Instant::now();
        let infos = {
            let registry = self.factory.registry.lock().unwrap();
            crate::holes::snapshot(&registry, self.core.records(), now)
        };
        let effects = self.core.evaluate(now, &infos, &mut self.pick_age());
        // Причина слива в логе: по сроку или по потерям (иначе отладка набора слепая).
        for effect in &effects {
            if let Effect::Retire(id) = effect
                && let Some(h) = infos.iter().find(|h| h.id == *id)
            {
                let loss = |l: Option<f32>| l.map_or("—".to_string(), |l| format!("{:.1}%", l * 100.0));
                log::info!(
                    "{}политика набора: сливаем дыру {id} (возраст {} с из {} с, потери ↑{} ↓{})",
                    self.factory.label,
                    h.age.as_secs(),
                    h.max_age.as_secs(),
                    loss(h.loss_out),
                    loss(h.loss_in)
                );
            }
        }
        self.execute(effects);
    }

    fn execute(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Spawn { id, first_peer } => {
                    if first_peer.is_some() {
                        log::info!("{}пир открывает дыру {id}, открываем свою", self.factory.label);
                    }
                    self.spawn_hole(id, first_peer);
                }
                Effect::Forward { id, session } => {
                    if let Some(task) = self.tasks.get(&id) {
                        let _ = task.peer_tx.try_send(session);
                    }
                }
                Effect::Retire(id) => {
                    log::debug!("{}политика набора: сливаем дыру {id}", self.factory.label);
                    send_command(&self.factory.ctl, id, HoleCmd::Retire);
                }
            }
        }
    }

    fn spawn_hole(&mut self, id: SlotId, first_peer: Option<PeerSession>) {
        let (peer_tx, peer_rx) = mpsc::channel(8);
        let hole = P2pHole {
            stun_addrs: self.stun_addrs.clone(),
            my_peer_id: self.my_peer_id,
            peer_id: self.peer_id,
            pair: self.pair.clone(),
            bind_ip: self.bind_ip,
            bind_ifindex: self.bind_ifindex,
            ports: self.ports.clone(),
            bootstrap: self.bootstrap.clone(),
            peer_rx,
            base: self.factory.make(id, true, true),
            ended: self.ended_tx.clone(),
        };
        let task = AbortOnDrop(tokio::spawn(hole.run(first_peer)));
        self.tasks.insert(id, HoleTask { peer_tx, _task: task });
    }
}

/// Рабочая задача одной P2P-дыры (один проход, потом дыра не возвращается).
struct P2pHole {
    stun_addrs: Vec<SocketAddr>,
    my_peer_id: Uuid,
    peer_id: Uuid,
    pair: PairSecret,
    bind_ip: IpAddr,
    bind_ifindex: Option<u32>,
    ports: Option<Arc<PortPool>>,
    bootstrap: Bootstrap,
    peer_rx: mpsc::Receiver<PeerSession>,
    base: SlotBase,
    ended: mpsc::UnboundedSender<SlotId>,
}

impl P2pHole {
    async fn run(mut self, first_peer: Option<PeerSession>) {
        let slot = self.base.slot;
        self.serve(first_peer).await;
        let _ = self.ended.send(slot);
    }

    async fn serve(&mut self, first_peer: Option<PeerSession>) {
        let (slot, label) = (self.base.slot, self.base.label.clone());
        let deadline = tokio::time::sleep(HOLE_DEADLINE);
        tokio::pin!(deadline);

        self.base.phase(SlotPhase::Starting);
        let (lease, socket) = match self.bind().await {
            Ok(bound) => bound,
            Err(e) => {
                log::warn!("{label}дыра {slot}: сокет: {e:#}");
                return;
            }
        };
        let _lease = lease;
        let socket = Arc::new(socket);

        // STUN: свои внешние адреса; не ответил — повторяем, пока не вышло время дыры.
        let my_endpoints = loop {
            tokio::select! {
                result = observe_endpoints(&socket, &self.stun_addrs) => match result {
                    Ok(endpoints) => break endpoints,
                    Err(e) => {
                        log::warn!("{label}дыра {slot}: STUN не удался: {e:#}; повтор");
                        tokio::select! {
                            () = tokio::time::sleep(Duration::from_secs(2)) => {}
                            _ = self.base.cmd_rx.recv() => return,
                            () = &mut deadline => return,
                        }
                    }
                },
                _ = self.base.cmd_rx.recv() => return,
                () = &mut deadline => return,
            }
        };
        let my_port = my_endpoints[0].port();

        // Анонс держим, пока не залинкуемся (дроп хэндла его останавливает).
        let my_session = Uuid::new_v4();
        let announce = spawn_announce(
            self.base.registry.clone(),
            self.bootstrap.clone(),
            rendezvous::our_record(&self.pair, self.my_peer_id, slot, my_session, &my_endpoints, 0),
            my_endpoints.clone(),
        );

        // Запись пира: если дыра ответная, она уже есть; если открыли мы — ждём ответ.
        self.base.phase(SlotPhase::Rendezvous);
        let mut peer = match first_peer {
            Some(peer) => peer,
            None => tokio::select! {
                peer = self.peer_rx.recv() => match peer {
                    Some(peer) => peer,
                    None => return,
                },
                _ = self.base.cmd_rx.recv() => return,
                () = tokio::time::sleep(RECORD_WAIT) => {
                    log::info!("{label}дыра {slot}: пир не ответил за {RECORD_WAIT:?} (набор у него полон?), закрываем");
                    return;
                }
                () = &mut deadline => return,
            },
        };

        // Пробиваем к последней записи пира. Пришла новая (пир перезапустился, сменил сеть) —
        // бросаем текущий пробив и начинаем к ней: к старой сессии не пройдёт ни один пакет (подпись
        // другая). Свою сессию не меняем.
        let link = 'punch: loop {
            let candidates = peer.candidates();
            let (low, high) = port_utils::sweep_bounds(my_port, peer.addr.port(), PunchConfig::default().margin);
            log::info!(
                "{label}дыра {slot}: пробив {low}..={high} на {} (STUN-порт пира {}){}",
                peer.addr.ip(),
                peer.addr.port(),
                if candidates.len() > 1 { format!(", ещё адреса пира: {:?}", &candidates[1..]) } else { String::new() }
            );
            self.base.phase(SlotPhase::Punching);
            let attempt = punch::establish(
                socket.clone(),
                my_port,
                candidates,
                self.identity(my_session, peer.session_id),
                PunchConfig::default(),
                self.base.events.clone(),
            );
            tokio::pin!(attempt);
            loop {
                tokio::select! {
                    result = &mut attempt => match result {
                        Ok(link) => break 'punch link,
                        Err(e) => {
                            log::warn!("{label}дыра {slot}: пробив не удался: {e}");
                            return;
                        }
                    },
                    () = &mut deadline => {
                        log::info!("{label}дыра {slot}: пробив не уложился в {HOLE_DEADLINE:?}, закрываем");
                        return;
                    }
                    _ = self.base.cmd_rx.recv() => return,
                    newer = self.peer_rx.recv() => {
                        let Some(newer) = newer else { return };
                        if newer.session_id == peer.session_id {
                            continue;
                        }
                        log::info!("{label}дыра {slot}: у пира новая запись, пробиваем к ней");
                        peer = newer;
                        continue 'punch;
                    }
                }
            }
        };

        // Залинковались — анонс больше не нужен.
        drop(announce);
        self.base.hold(link, std::future::pending()).await;
    }

    /// Сокет дыры: порт из диапазона `local_port_base` или от ОС.
    async fn bind(&self) -> Result<(Option<PortLease>, UdpSocket)> {
        match &self.ports {
            Some(pool) => {
                for _ in 0..16 {
                    let lease = pool.lease().context("диапазон локальных портов исчерпан")?;
                    if let Ok(socket) = crate::bind::udp(self.bind_ip, lease.port(), self.bind_ifindex).await {
                        return Ok((Some(lease), socket));
                    }
                }
                anyhow::bail!("не удалось занять порт из диапазона локальных портов")
            }
            None => Ok((None, crate::bind::udp(self.bind_ip, 0, self.bind_ifindex).await.context("не удалось создать сокет дыры")?)),
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
}

/// Анонс дыры пиру, пока хэндл жив. Есть живые (подтверждённые) дыры — запись уходит по ним
/// (виртуал-брокер, MQTT не нужен); нет — публикуется в MQTT (bootstrap) и обновляется до истечения TTL.
fn spawn_announce(registry: Arc<Mutex<LinkRegistry>>, bootstrap: Bootstrap, record: Rendezvous, endpoints: Vec<SocketAddr>) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        let (slot, session) = (record.slot, record.session_id.parse::<Uuid>().unwrap_or_default());
        let mut ticker = tokio::time::interval(HOLE_ANNOUNCE_INTERVAL);
        let mut last_publish: Option<Instant> = None;
        loop {
            ticker.tick().await;
            let links: Vec<_> = registry.lock().unwrap().links().into_iter().filter(|l| l.state != HoleState::Warming).collect();
            if links.is_empty() {
                if last_publish.is_none_or(|t| t.elapsed() >= REPUBLISH_INTERVAL) {
                    match bootstrap.publish(slot, session, &endpoints).await {
                        Ok(()) => last_publish = Some(Instant::now()),
                        Err(e) => log::debug!("публикация записи дыры {slot}: {e:#}"),
                    }
                }
            } else {
                for link in links {
                    link.sender.send_rendezvous(record.clone()).await;
                }
            }
        }
    }))
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


#[cfg(test)]
mod integration;

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: SlotId, n: u8) -> PeerSession {
        PeerSession { slot: id, session_id: Uuid::from_bytes([n; 16]), addr: SocketAddr::from(([203, 0, 113, n], 1000)), extra: vec![], registered_at_unix_ms: 0 }
    }

    fn age() -> impl FnMut() -> Duration {
        || Duration::from_secs(120)
    }

    fn core(min: usize, max: usize, t0: Instant) -> Core {
        Core::new(PoolPolicy { min_active: min, max_total: max, ..PoolPolicy::default() }, 100, t0)
    }

    /// Снимок набора для `evaluate`: все дыры ядра в заданном состоянии и возрасте.
    fn infos(core: &Core, now: Instant, state: HoleState) -> Vec<HoleInfo> {
        core.records()
            .map(|(id, opened, max_age)| HoleInfo { id, state, age: now.duration_since(opened), max_age, loss_out: None, loss_in: None, sample: 1000 })
            .collect()
    }

    fn spawned(effects: &[Effect]) -> Vec<SlotId> {
        effects.iter().filter_map(|e| if let Effect::Spawn { id, .. } = e { Some(*id) } else { None }).collect()
    }

    #[test]
    fn peers_record_for_an_unknown_hole_is_answered_by_a_hole_of_ours() {
        let t0 = Instant::now();
        let mut core = core(4, 10, t0);
        let effect = core.on_peer(t0, session(7, 1), &mut age());
        assert_eq!(effect, Some(Effect::Spawn { id: 7, first_peer: Some(session(7, 1)) }), "кто начал — тот начал, мы открываем в ответ");
        assert_eq!(core.records().count(), 1);
    }

    #[test]
    fn peers_record_is_ignored_when_we_already_have_the_maximum() {
        let t0 = Instant::now();
        let mut core = core(2, 3, t0);
        for id in 1..=3 {
            assert!(matches!(core.on_peer(t0, session(id, 1), &mut age()), Some(Effect::Spawn { .. })));
        }
        assert_eq!(core.on_peer(t0, session(4, 1), &mut age()), None, "у нас уже max_total дыр — не открываем");
        // После того как одна закрылась, место снова есть.
        core.on_ended(1);
        assert!(matches!(core.on_peer(t0, session(4, 1), &mut age()), Some(Effect::Spawn { id: 4, .. })));
    }

    #[test]
    fn our_unanswered_hole_does_not_block_answering_the_peers_at_the_maximum() {
        // Последнее место: у нас 2 дыры с пиром и своя, на которую пир ещё не ответил (max = 3).
        // Пир одновременно открыл свою: её нужно принять, иначе обе стороны ждут друг друга вечно.
        let t0 = Instant::now();
        let mut core = core(2, 3, t0);
        core.on_peer(t0, session(1, 1), &mut age());
        core.on_peer(t0, session(2, 1), &mut age());
        let now = t0 + START_GRACE + Duration::from_secs(11);
        let snapshot = infos(&core, now, HoleState::Active);
        let own = spawned(&core.evaluate(now, &snapshot, &mut age()));
        assert_eq!(own.len(), 1, "своя дыра открыта и ждёт ответа");
        assert!(matches!(core.on_peer(now, session(50, 1), &mut age()), Some(Effect::Spawn { id: 50, .. })), "запись пира принимаем");
        // А набор из дыр с известным пиром по-прежнему не растёт сверх максимума.
        assert_eq!(core.on_peer(now, session(51, 1), &mut age()), None);
    }

    #[test]
    fn a_record_for_an_open_hole_is_forwarded_not_reopened() {
        let t0 = Instant::now();
        let mut core = core(4, 10, t0);
        core.on_peer(t0, session(7, 1), &mut age());
        // Пир перезапустился и прислал новую сессию для того же номера — дыра узнает о ней сама.
        assert_eq!(core.on_peer(t0, session(7, 2), &mut age()), Some(Effect::Forward { id: 7, session: session(7, 2) }));
        assert_eq!(core.records().count(), 1);
    }

    #[test]
    fn a_closed_hole_is_not_reopened_by_a_late_record_but_a_new_session_is() {
        let t0 = Instant::now();
        let mut core = core(4, 10, t0);
        core.on_peer(t0, session(7, 1), &mut age());
        core.on_ended(7);
        assert_eq!(core.on_peer(t0, session(7, 1), &mut age()), None, "запоздалая запись закрытой дыры");
        assert!(matches!(core.on_peer(t0, session(7, 2), &mut age()), Some(Effect::Spawn { id: 7, .. })), "новая сессия — новая дыра");
    }

    #[test]
    fn closed_memory_is_bounded() {
        let t0 = Instant::now();
        let mut core = core(4, 10, t0);
        for id in 0..(CLOSED_MEMORY as u32 + 50) {
            core.on_peer(t0, session(id, 1), &mut age());
            core.on_ended(id);
        }
        assert_eq!(core.closed.len(), CLOSED_MEMORY);
        assert!(!core.closed.contains(&(0, Uuid::from_bytes([1; 16]))), "самое старое забыто");
    }

    #[test]
    fn we_do_not_open_holes_right_after_start_so_a_waiting_peer_can_ask_first() {
        let t0 = Instant::now();
        let mut core = core(4, 10, t0);
        assert_eq!(core.evaluate(t0 + Duration::from_secs(1), &[], &mut age()), vec![]);
        let effects = core.evaluate(t0 + START_GRACE, &[], &mut age());
        let ids = spawned(&effects);
        assert_eq!(ids.len(), 4, "открываем недостающие до min_active: {effects:?}");
        assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(), 4, "номера разные");
    }

    #[test]
    fn counterpart_holes_count_toward_the_minimum_so_we_do_not_open_a_second_four() {
        let t0 = Instant::now();
        let mut core = core(4, 10, t0);
        // Пир уже ждал нас и успел прислать четыре записи.
        for id in 1..=4 {
            core.on_peer(t0 + Duration::from_secs(1), session(id, 1), &mut age());
        }
        let now = t0 + START_GRACE;
        let snapshot = infos(&core, now, HoleState::Warming);
        assert_eq!(core.evaluate(now, &snapshot, &mut age()), vec![], "набор уже есть (четыре ответные дыры)");
    }

    #[test]
    fn a_new_hole_never_takes_a_number_that_is_already_in_use() {
        let t0 = Instant::now();
        let mut core = core(2, 10, t0);
        // Пир занял те номера, которые мы собирались взять (100 и 101).
        core.on_peer(t0, session(100, 1), &mut age());
        core.on_peer(t0, session(101, 1), &mut age());
        let now = t0 + START_GRACE;
        let snapshot = infos(&core, now, HoleState::Active);
        // Два открыто и в работе, до min_active дыр хватает; ждём роста по add_interval.
        let effects = core.evaluate(now, &snapshot, &mut age());
        let ids = spawned(&effects);
        assert!(ids.iter().all(|id| *id != 100 && *id != 101), "{ids:?}");
    }

    #[test]
    fn the_set_grows_one_hole_per_add_interval_up_to_the_maximum() {
        let t0 = Instant::now();
        let mut core = core(2, 4, t0);
        let mut now = t0 + START_GRACE;
        let mut total = 0;
        for _ in 0..12 {
            let snapshot = infos(&core, now, HoleState::Active);
            total += spawned(&core.evaluate(now, &snapshot, &mut age())).len();
            now += Duration::from_secs(11);
        }
        assert_eq!(total, 4, "ровно до max_total");
    }

    #[test]
    fn an_expired_hole_is_retired_only_when_the_set_has_slack() {
        let t0 = Instant::now();
        let mut core = core(2, 10, t0);
        core.on_peer(t0, session(1, 1), &mut age());
        core.on_peer(t0, session(2, 1), &mut age());
        let late = t0 + Duration::from_secs(200); // max_age = 120 с — обе просрочены
        let snapshot = infos(&core, late, HoleState::Active);
        // Ровно min_active в работе: сливать нельзя, откроем недостающее позже; сейчас — рост.
        assert!(core.evaluate(late, &snapshot, &mut age()).iter().all(|e| !matches!(e, Effect::Retire(_))));
        // Появилась ещё одна дыра в работе — запас есть, самая старая уходит.
        core.on_peer(late, session(3, 1), &mut age());
        let snapshot = infos(&core, late, HoleState::Active);
        let effects = core.evaluate(late + Duration::from_secs(11), &snapshot, &mut age());
        assert!(effects.iter().any(|e| matches!(e, Effect::Retire(_))), "{effects:?}");
    }
}
