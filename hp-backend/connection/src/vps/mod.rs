//! VPS-режим: у сервера белый IP, пробивать ничего не нужно.
//!
//! Клиент заранее знает `ip:порт знакомства` сервера. Никаких STUN, MQTT и окон пробива: клиент
//! спрашивает, сервер отвечает «твоя дыра k — мой порт P, сессия S», клиент идёт на этот порт.
//!
//! **Набор дыр динамический** (`PLAN-dynamic-holes-relay.md`): дыра «поработала — умерла». Номер
//! дыры (`SlotId`) монотонный, назначает его клиент, и он не переиспользуется. Клиент — координатор:
//! `Manager` раз в секунду зовёт `holes::plan` и открывает новые дыры или сливает старые и плохие;
//! сервер реактивный: на запрос знакомства с новым номером он заводит дыру (`ServerHole`).
//!
//! **Порт знакомства**: запрос начинается с открытого имени клиента (`auth::peer_name`); чужое имя
//! сервер молча отбрасывает. На подписанный запрос клиента по дыре k (его запись `Rendezvous` с
//! сессией) сервер отвечает записью своей дыры k, когда она готова. Состояние знакомства клиента
//! держит его актор `ClientActor`; дыры сообщают ему о себе через канал. Недавно закрытые номера
//! актор помнит, чтобы запоздалый запрос не воскресил дыру.
//!
//! Задача **дыры сервера** (один проход):
//!
//! ```text
//!   Listening: порт P из банка и сессия S; запись (P, S) — актору порта знакомства
//!        │
//!   Accepting: ждём на P первый подписанный пакет клиента (не дольше PUNCH_WINDOW)
//!        │
//!   Connected: дыра в реестре (SlotBase::hold), пока не потеряна и не слита (Drain) → конец
//! ```
//!
//! Задача **дыры клиента** (один проход):
//!
//! ```text
//!   Asking: новый локальный сокет и сессия C; раз в секунду шлём запрос на порт знакомства
//!        │ ответ сервера (P, S)
//!   Connecting(P, S): стучимся на P (запросы продолжаются); ответ сменился — идём на новый
//!        │
//!   Connected: дыра в реестре, пока не потеряна и не слита → конец; Manager откроет замену
//! ```
//!
//! Слив (`Drain`) — в `multilink::SlotBase::hold`: любая сторона, признавшая дыру плохой или
//! просроченной, помечает её `Draining` (данные по ней больше не шлёт), сообщает пиру `Drain`
//! по нескольким дырам, ещё ~2 с принимает и закрывает; порт возвращается в банк.
//!
//! Обмен на порту знакомства — обычный `PeerMessage::Lite` с `Rendezvous` внутри (номер дыры — в
//! записи), подписанный и замаскированный ключами знакомства из секрета пары
//! (`auth::PairSecret::bootstrap_keys`); сама запись тоже подписана. Без полного GUID обеих
//! сторон такой пакет не подделать.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::auth::{peer_name, PairSecret, RecvKeys, SendKeys};
use crate::codec;
use crate::holes::{plan, Action, PoolPolicy};
use crate::multilink::{jittered, send_command, AbortOnDrop, HoleCmd, HoleFactory, SlotBase, SlotId, SlotPhase};
use crate::port_pool::{PortLease, PortPool};
use crate::proto::{lite, peer_message, Lite, PeerMessage, Rendezvous};
use crate::punch::{self, PeerIdentity, PunchConfig};
use crate::rendezvous::{self, PeerSession};

/// Порт знакомства по умолчанию и диапазон портов слотов (10 000 портов вместе с ним).
pub const DEFAULT_BOOTSTRAP_PORT: u16 = 40000;
pub const DEFAULT_SLOT_PORTS: RangeInclusive<u16> = 40001..=49999;

/// Как часто клиент повторяет запрос, пока слот не залинкован.
const ASK_INTERVAL: Duration = Duration::from_secs(1);

/// Длина открытого имени клиента перед запросом знакомства (`auth::peer_name`).
const NAME_LEN: usize = 8;

/// Сколько клиент ждёт свою дыру (ответ сервера и пробив), а сервер — клиента на выданном порту.
/// Дольше не ждём: бывает, что ответы по новой паре портов не доходят, тогда нужна другая дыра.
const PUNCH_WINDOW: Duration = Duration::from_secs(30);

/// Сколько клиент ждёт, пока дыра откроется (ответ сервера и пробив), прежде чем бросить её и открыть
/// другую: бывает, что по новой паре портов ответы не доходят.
const OPEN_TIMEOUT: Duration = Duration::from_secs(12);

/// Сколько недавно закрытых номеров дыр помнит актор клиента (запоздалый запрос их не воскресит).
const CLOSED_MEMORY: usize = 256;

/// Как часто менеджер набора зовёт политику.
const MANAGE_INTERVAL: Duration = Duration::from_secs(1);

/// Очередь команд листенера (регистрация и снятие клиентов).
const COMMAND_QUEUE: usize = 16;

/// Очередь запросов знакомства к актору клиента: запросов ~по одному в секунду на слот.
const KNOCK_QUEUE: usize = 64;

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
    fn identity(&self, slot: crate::multilink::SlotId, my_session: Uuid, peer_session: Uuid) -> PeerIdentity {
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
    /// `started_ms` — время запуска процесса клиента (мс Unix; у сервера `0`): по его росту
    /// сервер узнаёт, что клиент перезапущен и его прошлые дыры мертвы.
    fn record(&self, slot: crate::multilink::SlotId, session: Uuid, endpoint: SocketAddr, started_ms: u64) -> Rendezvous {
        rendezvous::our_record(&self.secret, self.my_peer_id, slot, session, &[endpoint], started_ms)
    }
}

fn wrap(record: Rendezvous) -> PeerMessage {
    let slot = record.slot;
    PeerMessage { body: Some(peer_message::Body::Lite(Lite { slot, payload: Some(lite::Payload::Rendezvous(record)), pid: None })) }
}

/// Достаёт запись дыры пира (подпись пакета и записи проверены); всё остальное — `None`.
fn unwrap(data: &[u8], keys: &mut RecvKeys, pair: &Pair) -> Option<PeerSession> {
    let msg = codec::decode(data.to_vec(), keys)?;
    let Some(peer_message::Body::Lite(Lite { payload: Some(lite::Payload::Rendezvous(r)), .. })) = msg.body else {
        return None;
    };
    rendezvous::peer_session_from(&r, &pair.secret, pair.peer_id).ok()
}

/// Занимает порт из банка и привязывает к нему сокет. Если привязка не удалась (порт занят
/// чужой службой), порт возвращается в банк и берётся следующий.
async fn lease_and_bind(ports: &Arc<PortPool>) -> Result<(PortLease, UdpSocket)> {
    for _ in 0..BIND_ATTEMPTS {
        let lease = ports.lease().context("банк портов слотов исчерпан")?;
        if let Ok(socket) = UdpSocket::bind(("0.0.0.0", lease.port())).await {
            return Ok((lease, socket));
        }
    }
    anyhow::bail!("не удалось занять порт из банка за {BIND_ATTEMPTS} попыток")
}

// ---------------------------------------------------------------------------------------------
// Сервер
// ---------------------------------------------------------------------------------------------

/// Порт знакомства процесса: один сокет на всех клиентов. Актор-листенер раздаёт запросы по
/// открытому имени клиента; таблица «имя → канал» живёт только в его задаче, блокировок нет.
#[derive(Clone)]
pub struct Bootstrap {
    commands: mpsc::Sender<Command>,
    socket: Arc<UdpSocket>,
    ports: Arc<PortPool>,
}

impl std::fmt::Debug for Bootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bootstrap(порт {})", self.port())
    }
}

enum Command {
    Register { name: String, token: Uuid, knocks: mpsc::Sender<Knock> },
    Unregister { name: String, token: Uuid },
}

/// Регистрация клиента: общий сокет для ответов, канал его запросов и страж, который снимает
/// клиента с порта знакомства при дропе.
pub(crate) struct Registration {
    socket: Arc<UdpSocket>,
    knocks: mpsc::Receiver<Knock>,
    guard: Unregistration,
}

pub(crate) struct Unregistration {
    commands: mpsc::Sender<Command>,
    name: String,
    token: Uuid,
}

impl Drop for Unregistration {
    fn drop(&mut self) {
        let _ = self.commands.try_send(Command::Unregister { name: std::mem::take(&mut self.name), token: self.token });
    }
}

impl Bootstrap {
    /// Занимает порт знакомства и запускает листенер (один на процесс). `ports` — банк портов
    /// слотов, общий для всех клиентов; порт знакомства в него входить не может.
    pub async fn bind(port: u16, ports: RangeInclusive<u16>) -> Result<Self> {
        anyhow::ensure!(!ports.contains(&port), "порт знакомства {port} внутри диапазона слотов");
        let socket = Arc::new(
            UdpSocket::bind(("0.0.0.0", port)).await.with_context(|| format!("не удалось занять порт знакомства {port}"))?,
        );
        let (commands, commands_rx) = mpsc::channel(COMMAND_QUEUE);
        log::info!("VPS-сервер: порт знакомства {port}");
        tokio::spawn(listen(socket.clone(), commands_rx));
        Ok(Self { commands, socket, ports: PortPool::new(ports) })
    }

    /// Банк портов слотов процесса.
    pub fn ports(&self) -> Arc<PortPool> {
        self.ports.clone()
    }

    /// Порт, на котором слушает процесс.
    pub fn port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    async fn register(&self, name: String) -> Registration {
        let (knocks_tx, knocks) = mpsc::channel(KNOCK_QUEUE);
        let token = Uuid::new_v4();
        let _ = self.commands.send(Command::Register { name: name.clone(), token, knocks: knocks_tx }).await;
        Registration {
            socket: self.socket.clone(),
            knocks,
            guard: Unregistration { commands: self.commands.clone(), name, token },
        }
    }
}

/// Настройки сервера одного клиента.
pub(crate) struct ServerConfig {
    pub public_ip: IpAddr,
    pub pair: Pair,
    pub bootstrap: Bootstrap,
    pub pool: PoolPolicy,
}

/// Запрос знакомства, уже отделённый от открытого имени клиента.
struct Knock {
    packet: Vec<u8>,
    from: SocketAddr,
}

/// Сообщения дыр сервера актору своего клиента.
enum HoleMsg {
    /// Дыра заняла порт: что отвечать на знакомство.
    Offer { slot: SlotId, epoch: u32, record: Rendezvous },
    /// Задача дыры закончилась. `retry` — по нашей вине (не нашёлся порт): номер не «закрыт»,
    /// следующий запрос клиента заведёт дыру заново.
    Ended { slot: SlotId, epoch: u32, retry: bool },
}

/// Регистрирует клиента на общем порту знакомства и запускает его актор (он заводит дыры).
pub(crate) async fn start_server(label: &crate::label::Label, config: ServerConfig, factory: HoleFactory) -> Result<Vec<AbortOnDrop>> {
    let name = peer_name(&config.pair.peer_id);
    let Registration { socket, knocks, guard } = config.bootstrap.register(name.clone()).await;
    log::info!("{label}VPS-сервер: клиент {name} на порту знакомства {}", config.bootstrap.port());

    let (send, recv) = config.pair.secret.bootstrap_keys();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let actor = ClientActor {
        socket,
        pair: config.pair,
        send,
        recv,
        public_ip: config.public_ip,
        ports: config.bootstrap.ports(),
        factory,
        msg_tx,
        holes: HashMap::new(),
        // С запасом на сливаемые дыры: клиент открывает замену, пока старая ещё закрывается.
        core: KnockCore::new(config.pool.max_total * 2),
    };
    Ok(vec![AbortOnDrop(tokio::spawn(actor.run(knocks, msg_rx, guard)))])
}

/// Листенер порта знакомства: команды регистрации и раздача запросов по открытому имени.
async fn listen(socket: Arc<UdpSocket>, mut commands: mpsc::Receiver<Command>) {
    let mut clients: HashMap<String, (Uuid, mpsc::Sender<Knock>)> = HashMap::new();
    let mut buf = [0u8; 1500];
    loop {
        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = received else { continue };
                if n <= NAME_LEN {
                    continue;
                }
                let Ok(name) = std::str::from_utf8(&buf[..NAME_LEN]) else { continue };
                if let Some((_, knocks)) = clients.get(name) {
                    let _ = knocks.try_send(Knock { packet: buf[NAME_LEN..n].to_vec(), from });
                }
            }
            command = commands.recv() => match command {
                Some(Command::Register { name, token, knocks }) => {
                    clients.insert(name, (token, knocks));
                }
                Some(Command::Unregister { name, token }) => {
                    if clients.get(&name).is_some_and(|(current, _)| *current == token) {
                        clients.remove(&name);
                    }
                }
                None => return,
            }
        }
    }
}

/// Решение по запросу знакомства.
#[derive(Debug, PartialEq)]
enum KnockAction {
    /// Запрос не по делу — отбросить молча.
    Ignore,
    /// Дыра с таким номером и сессией уже есть — ответить её записью, если готова.
    Existing,
    /// Завести дыру (`replace` — прошлая дыра с этим номером брошена клиентом и ей на смену).
    Spawn { epoch: u32, replace: bool },
}

#[derive(Debug, PartialEq)]
struct KnockDecision {
    /// Клиент перезапущен (время запуска в запросе выросло): все его прошлые дыры закрыть до `action`.
    restarted: bool,
    action: KnockAction,
}

/// Решения сервера по запросам знакомства одного клиента — чистое ядро без сети и задач (оболочка —
/// `ClientActor`): какие дыры заводить, какие запросы отбрасывать, когда клиент перезапущен.
struct KnockCore {
    /// Дыры клиента: номер → (эпоха заведения, сессия клиента).
    holes: HashMap<SlotId, (u32, Uuid)>,
    /// Недавно закрытые (номер, сессия клиента): запоздалый запрос их не воскрешает.
    closed: VecDeque<(SlotId, Uuid)>,
    next_epoch: u32,
    max_holes: usize,
    /// Время запуска процесса клиента по его последнему запросу (`0` — ещё не знаем).
    client_started: u64,
}

impl KnockCore {
    fn new(max_holes: usize) -> Self {
        Self { holes: HashMap::new(), closed: VecDeque::new(), next_epoch: 0, max_holes, client_started: 0 }
    }

    fn knock(&mut self, slot: SlotId, session: Uuid, started_ms: u64) -> KnockDecision {
        let mut restarted = false;
        if started_ms > self.client_started {
            restarted = self.client_started != 0;
            if restarted {
                self.holes.clear();
            }
            self.closed.clear();
            self.client_started = started_ms;
        } else if started_ms != 0 && started_ms < self.client_started {
            // Запоздалый запрос прошлого экземпляра клиента.
            return KnockDecision { restarted: false, action: KnockAction::Ignore };
        }
        let ignore = |restarted| KnockDecision { restarted, action: KnockAction::Ignore };
        if self.closed.contains(&(slot, session)) {
            return ignore(restarted);
        }
        let replace = match self.holes.get(&slot) {
            Some(&(_, known)) if known == session => return KnockDecision { restarted, action: KnockAction::Existing },
            Some(_) => true,
            None => {
                if self.holes.len() >= self.max_holes {
                    return ignore(restarted);
                }
                false
            }
        };
        self.next_epoch = self.next_epoch.wrapping_add(1);
        self.holes.insert(slot, (self.next_epoch, session));
        KnockDecision { restarted, action: KnockAction::Spawn { epoch: self.next_epoch, replace } }
    }

    /// Задача дыры закончилась. `true` — это текущая дыра номера (а не вытесненная прошлая).
    /// `retry` — по нашей вине (не нашёлся порт): номер не «закрыт», следующий запрос заведёт дыру заново.
    fn on_ended(&mut self, slot: SlotId, epoch: u32, retry: bool) -> bool {
        let Some(&(current, session)) = self.holes.get(&slot) else { return false };
        if current != epoch {
            return false;
        }
        self.holes.remove(&slot);
        if !retry {
            self.closed.push_back((slot, session));
            if self.closed.len() > CLOSED_MEMORY {
                self.closed.pop_front();
            }
        }
        true
    }
}

/// Дыра клиента на сервере: что отвечать на знакомство и задача дыры.
struct ServerHoleRec {
    epoch: u32,
    offer: Option<Rendezvous>,
    _task: AbortOnDrop,
}

/// Владелец состояния клиента на сервере: его дыры, ответы на знакомство, недавно закрытые номера.
/// Блокировок нет: состояние меняет только эта задача, дыры пишут ей через канал.
struct ClientActor {
    socket: Arc<UdpSocket>,
    pair: Pair,
    send: SendKeys,
    recv: RecvKeys,
    public_ip: IpAddr,
    ports: Arc<PortPool>,
    factory: HoleFactory,
    msg_tx: mpsc::UnboundedSender<HoleMsg>,
    holes: HashMap<SlotId, ServerHoleRec>,
    /// Решения по запросам знакомства (чистое ядро).
    core: KnockCore,
}

impl ClientActor {
    async fn run(mut self, mut knocks: mpsc::Receiver<Knock>, mut messages: mpsc::UnboundedReceiver<HoleMsg>, _registration: Unregistration) {
        loop {
            tokio::select! {
                knock = knocks.recv() => match knock {
                    Some(knock) => self.knock(knock).await,
                    None => return,
                },
                Some(message) = messages.recv() => self.on_message(message),
            }
        }
    }

    fn on_message(&mut self, message: HoleMsg) {
        match message {
            HoleMsg::Offer { slot, epoch, record } => {
                if let Some(hole) = self.holes.get_mut(&slot).filter(|h| h.epoch == epoch) {
                    hole.offer = Some(record);
                }
            }
            HoleMsg::Ended { slot, epoch, retry } => {
                if self.core.on_ended(slot, epoch, retry) {
                    self.holes.remove(&slot);
                }
            }
        }
    }

    /// Запрос клиента по дыре k: новый номер — заводим дыру; в ответ — запись дыры, когда готова.
    async fn knock(&mut self, knock: Knock) {
        let Some(request) = unwrap(&knock.packet, &mut self.recv, &self.pair) else { return };
        let slot = request.slot;
        let decision = self.core.knock(slot, request.session_id, request.registered_at_unix_ms);
        if decision.restarted && !self.holes.is_empty() {
            // Клиент перезапущен: его прошлые дыры мертвы, не ждём тайм-аута потери.
            log::info!("клиент перезапущен: закрываем его прошлые дыры ({})", self.holes.len());
            self.holes.clear();
        }
        match decision.action {
            KnockAction::Ignore => return,
            KnockAction::Existing => {}
            KnockAction::Spawn { epoch, replace } => {
                if replace {
                    // Тот же номер с новой сессией: так делает прошлая версия клиента (фиксированные
                    // слоты 0..9 перерегистрируются после потери дыры). Прошлая дыра брошена.
                    log::debug!("знакомство: дыра {slot} перерегистрирована клиентом {}, новая сессия {}", knock.from, request.session_id);
                    if let Some(mut old) = self.holes.remove(&slot) {
                        // Дожидаемся конца старой задачи: её уборка (реестр, команды, фазы) не должна
                        // задеть новую дыру с тем же номером.
                        old._task.0.abort();
                        let _ = (&mut old._task.0).await;
                    }
                } else {
                    log::debug!("знакомство: дыра {slot}, клиент {}, сессия {}", knock.from, request.session_id);
                }
                self.spawn_hole(slot, epoch, request.session_id);
            }
        }
        if let Some(offer) = self.holes.get(&slot).and_then(|h| h.offer.clone()) {
            let _ = self.socket.send_to(&codec::encode(&wrap(offer), &self.send), knock.from).await;
        }
    }

    fn spawn_hole(&mut self, slot: SlotId, epoch: u32, client_session: Uuid) {
        let hole = ServerHole {
            public_ip: self.public_ip,
            ports: self.ports.clone(),
            pair: self.pair.clone(),
            messages: self.msg_tx.clone(),
            epoch,
            base: self.factory.make(slot, true, true),
        };
        let task = AbortOnDrop(tokio::spawn(hole.run(client_session)));
        self.holes.insert(slot, ServerHoleRec { epoch, offer: None, _task: task });
    }
}

/// Рабочая задача одной дыры сервера (один проход, потом дыра не возвращается).
struct ServerHole {
    public_ip: IpAddr,
    ports: Arc<PortPool>,
    pair: Pair,
    messages: mpsc::UnboundedSender<HoleMsg>,
    epoch: u32,
    base: SlotBase,
}

impl ServerHole {
    async fn run(mut self, client: Uuid) {
        let (slot, epoch) = (self.base.slot, self.epoch);
        let retry = self.serve(client).await;
        let _ = self.messages.send(HoleMsg::Ended { slot, epoch, retry });
    }

    /// Возвращает `true`, если дыру не удалось завести по нашей вине (нет порта).
    async fn serve(&mut self, client: Uuid) -> bool {
        let (slot, label) = (self.base.slot, self.base.label.clone());
        self.base.phase(SlotPhase::Rendezvous);
        // Порт остаётся в банке, пока жива дыра: вернётся, когда задача закончится.
        let (port_lease, socket) = match lease_and_bind(&self.ports).await {
            Ok((lease, socket)) => (lease, Arc::new(socket)),
            Err(e) => {
                log::warn!("{label}дыра {slot}: не удалось занять порт: {e:#}");
                return true;
            }
        };
        let port = port_lease.port();
        let my_session = Uuid::new_v4();
        let record = self.pair.record(slot, my_session, SocketAddr::new(self.public_ip, port), 0);
        let _ = self.messages.send(HoleMsg::Offer { slot, epoch: self.epoch, record });
        log::info!("{label}дыра {slot}: ждём клиента на порту {port}");
        self.base.phase(SlotPhase::Punching);
        let attempt = punch::establish(
            socket.clone(),
            port,
            Vec::new(), // пассивно: отвечаем туда, откуда придёт клиент
            self.pair.identity(slot, my_session, client),
            PunchConfig::default(),
            self.base.events.clone(),
        );
        let link = tokio::select! {
            result = tokio::time::timeout(PUNCH_WINDOW, attempt) => match result {
                Ok(Ok(link)) => link,
                Ok(Err(e)) => {
                    log::warn!("{label}дыра {slot}: приём не удался: {e}");
                    return false;
                }
                Err(_) => {
                    log::info!("{label}дыра {slot}: клиент не пришёл за {PUNCH_WINDOW:?}, закрываем");
                    return false;
                }
            },
            _ = self.base.cmd_rx.recv() => return false,
        };
        self.base.hold(link, std::future::pending()).await;
        drop(port_lease);
        false
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
    pub pool: PoolPolicy,
    pub hole_age: (Duration, Duration),
}

/// Сокет знакомства клиента: запросы серверу и разбор ответов по дырам.
struct ClientBootstrap {
    socket: UdpSocket,
    server: SocketAddr,
    send: SendKeys,
    name: String,
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
                let mut packet = this.name.clone().into_bytes();
                packet.extend_from_slice(&codec::encode(&wrap(record.clone()), &this.send));
                let _ = this.socket.send_to(&packet, this.server).await;
            }
        }))
    }
}

/// Ответы сервера по номерам дыр: последний ответ дыры — в её `watch`.
type Offers = Arc<Mutex<HashMap<SlotId, watch::Sender<Option<PeerSession>>>>>;

/// Принимает ответы сервера и раскладывает их по дырам (ответ на уже закрытую дыру отбрасывается).
async fn receive_offers(boot: Arc<ClientBootstrap>, mut recv: RecvKeys, pair: Pair, offers: Offers) {
    let mut buf = [0u8; 1500];
    loop {
        let Ok((n, _)) = boot.socket.recv_from(&mut buf).await else { continue };
        let Some(offer) = unwrap(&buf[..n], &mut recv, &pair) else { continue };
        let Some(tx) = offers.lock().unwrap().get(&offer.slot).cloned() else { continue };
        tx.send_if_modified(|current| {
            let changed = current.as_ref() != Some(&offer);
            if changed {
                log::debug!("знакомство: дыра {}, сервер предлагает {} (сессия {})", offer.slot, offer.addr, offer.session_id);
                *current = Some(offer.clone());
            }
            changed
        });
    }
}

/// Сокет знакомства и менеджер набора дыр клиента.
pub(crate) async fn start_client(label: &crate::label::Label, config: ClientConfig, factory: HoleFactory) -> Result<Vec<AbortOnDrop>> {
    let socket = crate::bind::udp(config.bind_ip, 0, config.bind_ifindex).await.context("сокет знакомства")?;
    let (send, recv) = config.pair.secret.bootstrap_keys();
    let name = peer_name(&config.pair.my_peer_id);
    let boot = Arc::new(ClientBootstrap { socket, server: config.server, send, name });
    log::info!("{label}VPS-клиент: сервер {}", config.server);

    let offers = Offers::default();
    let (ended_tx, ended_rx) = mpsc::unbounded_channel();
    let manager = Manager {
        boot: boot.clone(),
        pair: config.pair.clone(),
        bind_ip: config.bind_ip,
        bind_ifindex: config.bind_ifindex,
        factory,
        offers: offers.clone(),
        pool: config.pool,
        hole_age: config.hole_age,
        holes: HashMap::new(),
        // Номера монотонные и не переиспользуются; старт случайный (до миллиона — чтобы в статусе
        // номера были короткими), чтобы перезапущенный клиент не пересёкся с номерами прошлого запуска.
        next_id: u32::from_le_bytes(Uuid::new_v4().into_bytes()[..4].try_into().expect("4 байта")) % 1_000_000,
        last_open: Instant::now() - config.pool.add_interval,
        ended_tx,
        started_ms: SystemTime::now().duration_since(UNIX_EPOCH).map_or(1, |d| d.as_millis() as u64).max(1),
    };
    Ok(vec![
        AbortOnDrop(tokio::spawn(receive_offers(boot, recv, config.pair, offers))),
        AbortOnDrop(tokio::spawn(manager.run(ended_rx))),
    ])
}

/// Дыра, которую ведёт менеджер.
struct HoleRec {
    opened: Instant,
    max_age: Duration,
    _task: AbortOnDrop,
}

/// Менеджер набора дыр клиента: раз в секунду оценивает набор (`holes::plan`) и открывает новые
/// дыры или сливает старые и плохие. Номера назначает он.
struct Manager {
    boot: Arc<ClientBootstrap>,
    pair: Pair,
    bind_ip: IpAddr,
    bind_ifindex: Option<u32>,
    factory: HoleFactory,
    offers: Offers,
    pool: PoolPolicy,
    hole_age: (Duration, Duration),
    holes: HashMap<SlotId, HoleRec>,
    next_id: SlotId,
    last_open: Instant,
    ended_tx: mpsc::UnboundedSender<SlotId>,
    /// Время запуска процесса (мс Unix), во всех запросах знакомства: так сервер узнаёт о перезапуске.
    started_ms: u64,
}

impl Manager {
    async fn run(mut self, mut ended: mpsc::UnboundedReceiver<SlotId>) {
        let mut ticker = tokio::time::interval(MANAGE_INTERVAL);
        loop {
            tokio::select! {
                _ = ticker.tick() => self.evaluate(),
                Some(slot) = ended.recv() => {
                    self.holes.remove(&slot);
                    // Не ждём очередного тика: замена нужна сразу, если дыр не хватает.
                    self.evaluate();
                }
            }
        }
    }

    fn evaluate(&mut self) {
        let now = Instant::now();
        let infos = {
            let registry = self.factory.registry.lock().unwrap();
            crate::holes::snapshot(&registry, self.holes.iter().map(|(&id, rec)| (id, rec.opened, rec.max_age)), now)
        };
        for action in plan(&infos, now.duration_since(self.last_open), &self.pool) {
            match action {
                Action::Open => {
                    if self.holes.len() < self.pool.max_total * 2 {
                        self.open(now);
                    }
                }
                Action::Retire(id) => {
                    log::debug!("{}политика набора: сливаем дыру {id}", self.factory.label);
                    send_command(&self.factory.ctl, id, HoleCmd::Retire);
                }
            }
        }
    }

    fn open(&mut self, now: Instant) {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.last_open = now;
        let max_age = jittered(self.hole_age.0, self.hole_age.1);
        let (offer_tx, offer_rx) = watch::channel(None);
        self.offers.lock().unwrap().insert(id, offer_tx);
        let hole = ClientHole {
            boot: self.boot.clone(),
            pair: self.pair.clone(),
            bind_ip: self.bind_ip,
            bind_ifindex: self.bind_ifindex,
            offer_rx,
            base: self.factory.make(id, true, false),
            offers: self.offers.clone(),
            ended: self.ended_tx.clone(),
            started_ms: self.started_ms,
        };
        let task = AbortOnDrop(tokio::spawn(hole.run()));
        self.holes.insert(id, HoleRec { opened: now, max_age, _task: task });
    }
}

/// Рабочая задача одной дыры клиента (один проход, потом дыра не возвращается).
struct ClientHole {
    boot: Arc<ClientBootstrap>,
    pair: Pair,
    bind_ip: IpAddr,
    bind_ifindex: Option<u32>,
    offer_rx: watch::Receiver<Option<PeerSession>>,
    base: SlotBase,
    offers: Offers,
    ended: mpsc::UnboundedSender<SlotId>,
    started_ms: u64,
}

impl ClientHole {
    async fn run(mut self) {
        let slot = self.base.slot;
        self.serve().await;
        self.offers.lock().unwrap().remove(&slot);
        let _ = self.ended.send(slot);
    }

    async fn serve(&mut self) {
        let (slot, label) = (self.base.slot, self.base.label.clone());
        // Asking: свой сокет (новый локальный адрес на каждую дыру) и сессия; спрашиваем, пока не
        // залинкуемся.
        self.base.phase(SlotPhase::Rendezvous);
        let socket = match crate::bind::udp(self.bind_ip, 0, self.bind_ifindex).await {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                log::warn!("{label}дыра {slot}: сокет дыры: {e}");
                return;
            }
        };
        let port = socket.local_addr().map(|a| a.port()).unwrap_or(0);
        let my_session = Uuid::new_v4();
        let asking = self.boot.ask(self.pair.record(slot, my_session, SocketAddr::from(([0, 0, 0, 0], port)), self.started_ms));
        let deadline = tokio::time::sleep(OPEN_TIMEOUT);
        tokio::pin!(deadline);

        // Connecting: идём на порт из последнего ответа; ответ сменился — на новый.
        let link = 'connect: loop {
            let offer = tokio::select! {
                offer = self.offer_rx.wait_for(Option::is_some) => match offer {
                    Ok(offer) => offer.clone().expect("проверено"),
                    Err(_) => return,
                },
                _ = self.base.cmd_rx.recv() => return,
                _ = &mut deadline => {
                    log::info!("{label}дыра {slot}: сервер не ответил за {OPEN_TIMEOUT:?}, закрываем");
                    return;
                }
            };
            self.offer_rx.mark_unchanged();
            log::info!("{label}дыра {slot}: идём на порт сервера {}", offer.addr);
            self.base.phase(SlotPhase::Punching);
            // Сокет говорит только с этим портом сервера: ядру не искать маршрут на каждый пакет,
            // чужие адреса отсекаются в ядре.
            if let Err(e) = socket.connect(offer.addr).await {
                log::warn!("{label}дыра {slot}: connect к {}: {e}", offer.addr);
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
                    Ok(link) => break 'connect link,
                    Err(e) => {
                        log::warn!("{label}дыра {slot}: подключение не удалось: {e}");
                        return;
                    }
                },
                changed = self.offer_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    continue 'connect;
                }
                _ = self.base.cmd_rx.recv() => return,
                _ = &mut deadline => {
                    log::info!("{label}дыра {slot}: дыра не открылась за {OPEN_TIMEOUT:?}, закрываем");
                    return;
                }
            }
        };
        drop(asking);
        self.base.hold(link, std::future::pending()).await;
    }
}

#[cfg(test)]
mod integration;

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
    fn only_signed_records_of_the_expected_peer_are_accepted() {
        let (me, peer) = (Uuid::new_v4(), Uuid::new_v4());
        let mine = pair(me, peer);
        let theirs = pair(peer, me);
        let (send, mut recv) = theirs.secret.bootstrap_keys();
        let endpoint = SocketAddr::from(([203, 0, 113, 10], 41234));
        let packet = |p: &Pair, slot: crate::multilink::SlotId| codec::encode(&wrap(p.record(slot, Uuid::new_v4(), endpoint, 0)), &send);

        let ok = unwrap(&packet(&theirs, 3), &mut recv, &mine).expect("запись пира");
        assert_eq!((ok.slot, ok.addr), (3, endpoint));
        let big = unwrap(&packet(&theirs, 4_000_000_000), &mut recv, &mine).expect("номера динамических дыр велики");
        assert_eq!(big.slot, 4_000_000_000);
        assert!(unwrap(&packet(&mine, 0), &mut recv, &mine).is_none(), "своя запись (эхо)");

        let stranger = pair(peer, Uuid::new_v4());
        assert!(unwrap(&packet(&stranger, 0), &mut recv, &mine).is_none(), "запись подписана чужим");
        let (foreign_send, _) = stranger.secret.bootstrap_keys();
        let foreign = codec::encode(&wrap(theirs.record(0, Uuid::new_v4(), endpoint, 0)), &foreign_send);
        assert!(unwrap(&foreign, &mut recv, &mine).is_none(), "пакет чужих ключей знакомства");
    }

    #[tokio::test]
    async fn listener_passes_only_the_expected_name() {
        let server = Arc::new(UdpSocket::bind(("127.0.0.1", 0)).await.unwrap());
        let addr = server.local_addr().unwrap();
        let (commands_tx, commands_rx) = mpsc::channel(8);
        tokio::spawn(listen(server, commands_rx));
        let (knocks_tx, mut knocks) = mpsc::channel(8);
        commands_tx.send(Command::Register { name: "0a1b2c3d".into(), token: Uuid::new_v4(), knocks: knocks_tx }).await.unwrap();
        let client = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        client.send_to(b"ffffffffpayload", addr).await.unwrap();
        client.send_to(b"0a1b2c3dpayload", addr).await.unwrap();
        let knock = tokio::time::timeout(Duration::from_secs(1), knocks.recv()).await.unwrap().unwrap();
        assert_eq!(knock.packet, b"payload");
        assert!(knocks.try_recv().is_err(), "чужое имя должно отбрасываться молча");
    }

    /// Прошлая версия клиента держит фиксированные слоты 0..9 и после потери дыры перерегистрирует
    /// тот же номер с новой сессией: сервер обязан завести дыру заново, а не молчать.
    #[tokio::test]
    async fn server_follows_a_legacy_client_that_reregisters_the_same_slot() {
        use crate::discovery::Discovery;
        use crate::multilink::{MultiLink, MultiLinkOptions};
        let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
        let boot_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let bootstrap = Bootstrap::bind(boot_port, 48200..=48299).await.unwrap();
        let (_server, _rx) = MultiLink::start_discovery(
            "",
            Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap },
            server_id,
            client_id,
            MultiLinkOptions::default(),
        )
        .await
        .unwrap();

        let client = pair(client_id, server_id);
        let (send, mut recv) = client.secret.bootstrap_keys();
        let socket = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let server_addr = SocketAddr::from(([127, 0, 0, 1], boot_port));
        let mut ask = async |slot: SlotId, session: Uuid| -> PeerSession {
            for _ in 0..20 {
                let record = client.record(slot, session, SocketAddr::from(([127, 0, 0, 1], 50000)), 0);
                let mut packet = peer_name(&client_id).into_bytes();
                packet.extend_from_slice(&codec::encode(&wrap(record), &send));
                socket.send_to(&packet, server_addr).await.unwrap();
                let mut buf = [0u8; 1500];
                if let Ok(Ok((n, _))) = tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut buf)).await
                    && let Some(offer) = unwrap(&buf[..n], &mut recv, &client)
                {
                    return offer;
                }
            }
            panic!("сервер не ответил на знакомство");
        };

        let first = ask(3, Uuid::new_v4()).await;
        assert_eq!(first.slot, 3);
        let second = ask(3, Uuid::new_v4()).await;
        assert_eq!(second.slot, 3);
        assert_ne!(first.session_id, second.session_id, "новая сессия клиента — новая дыра сервера");
        assert_ne!(first.addr, second.addr, "новая дыра занимает другой порт");
    }

    fn knock_core() -> KnockCore {
        KnockCore::new(4)
    }

    #[test]
    fn a_new_number_gets_a_hole_and_a_repeated_knock_finds_it() {
        let (mut core, session) = (knock_core(), Uuid::new_v4());
        assert!(matches!(core.knock(7, session, 100).action, KnockAction::Spawn { replace: false, .. }));
        assert_eq!(core.knock(7, session, 100).action, KnockAction::Existing, "повторный запрос раз в секунду дыры не множит");
    }

    #[test]
    fn the_same_number_with_a_new_session_replaces_the_hole_of_a_legacy_client() {
        let mut core = knock_core();
        let first = match core.knock(3, Uuid::new_v4(), 0).action {
            KnockAction::Spawn { epoch, .. } => epoch,
            other => panic!("{other:?}"),
        };
        let second = match core.knock(3, Uuid::new_v4(), 0).action {
            KnockAction::Spawn { epoch, replace: true } => epoch,
            other => panic!("{other:?}"),
        };
        assert_ne!(first, second);
        // Конец вытесненной дыры текущую не трогает.
        assert!(!core.on_ended(3, first, false));
        assert!(core.on_ended(3, second, false));
    }

    #[test]
    fn a_late_knock_of_a_closed_hole_does_not_revive_it_but_a_retry_does() {
        let (mut core, session) = (knock_core(), Uuid::new_v4());
        let KnockAction::Spawn { epoch, .. } = core.knock(9, session, 0).action else { panic!() };
        assert!(core.on_ended(9, epoch, false));
        assert_eq!(core.knock(9, session, 0).action, KnockAction::Ignore);
        // Не нашёлся порт — номер не закрыт, клиент попробует снова.
        let other = Uuid::new_v4();
        let KnockAction::Spawn { epoch, .. } = core.knock(10, other, 0).action else { panic!() };
        assert!(core.on_ended(10, epoch, true));
        assert!(matches!(core.knock(10, other, 0).action, KnockAction::Spawn { .. }));
    }

    #[test]
    fn a_restarted_client_closes_its_old_holes_and_old_knocks_are_ignored() {
        let mut core = knock_core();
        let old = Uuid::new_v4();
        core.knock(1, old, 1_000);
        core.knock(2, old, 1_000);
        let decision = core.knock(500, Uuid::new_v4(), 2_000);
        assert!(decision.restarted, "время запуска выросло — клиент перезапущен");
        assert!(matches!(decision.action, KnockAction::Spawn { .. }));
        assert_eq!(core.holes.len(), 1, "прошлые дыры закрыты");
        assert_eq!(core.knock(1, old, 1_000).action, KnockAction::Ignore, "запоздалый запрос прошлого экземпляра");
        // Прошлая версия клиента время запуска не шлёт (0) — на решение это не влияет.
        assert!(matches!(core.knock(5, Uuid::new_v4(), 0).action, KnockAction::Spawn { .. }));
        // Первый запрос вообще — не «перезапуск».
        assert!(!knock_core().knock(1, Uuid::new_v4(), 777).restarted);
    }

    #[test]
    fn a_client_cannot_hold_more_holes_than_the_limit() {
        let mut core = knock_core();
        for slot in 0..4 {
            assert!(matches!(core.knock(slot, Uuid::new_v4(), 0).action, KnockAction::Spawn { .. }));
        }
        assert_eq!(core.knock(4, Uuid::new_v4(), 0).action, KnockAction::Ignore, "предел дыр на клиента");
        // Существующим и перерегистрации номеру предел не мешает.
        let session = Uuid::new_v4();
        let KnockAction::Spawn { epoch, .. } = core.knock(2, session, 0).action else { panic!() };
        assert!(core.on_ended(2, epoch, false));
        assert!(matches!(core.knock(4, Uuid::new_v4(), 0).action, KnockAction::Spawn { .. }), "место освободилось");
    }
}
