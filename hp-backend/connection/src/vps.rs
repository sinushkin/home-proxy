//! VPS-режим: у сервера белый IP, пробивать ничего не нужно.
//!
//! Клиент заранее знает `ip:порт знакомства` сервера. Никаких STUN, MQTT и окон пробива: клиент
//! спрашивает, сервер отвечает «твой слот k — мой порт P, сессия S», клиент идёт на этот порт.
//!
//! **Порт знакомства**: запрос начинается с открытого имени клиента (`auth::peer_name`); чужое имя
//! сервер молча отбрасывает. На подписанный запрос клиента по слоту k (его запись `Rendezvous` с
//! сессией) сервер отвечает текущей записью своего слота k. Запись у слота есть всегда — с момента,
//! как он занял порт, — и не снимается никогда, только заменяется новой. Состояние знакомства
//! (записи и сессии по слотам) держит актор клиента `ClientActor`; слоты обновляют его через канал.
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

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::auth::{peer_name, PairSecret, RecvKeys, SendKeys};
use crate::codec;
use crate::multilink::{AbortOnDrop, SlotBase, SlotId, SlotPhase, TARGET_LINKS};
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
    fn record(&self, slot: crate::multilink::SlotId, session: Uuid, endpoint: SocketAddr) -> Rendezvous {
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
}

/// Запрос знакомства, уже отделённый от открытого имени клиента.
struct Knock {
    packet: Vec<u8>,
    from: SocketAddr,
}

/// Новая запись слота сервера: слот сообщает актору клиента, что отвечать на знакомство.
struct OfferUpdate {
    slot: SlotId,
    record: Rendezvous,
}

/// Регистрирует клиента на общем порту знакомства и запускает его слоты и актор.
pub(crate) async fn start_server(label: &crate::label::Label, config: ServerConfig, bases: Vec<SlotBase>) -> Result<Vec<AbortOnDrop>> {
    let name = peer_name(&config.pair.peer_id);
    let Registration { socket, knocks, guard } = config.bootstrap.register(name.clone()).await;
    log::info!("{label}VPS-сервер: клиент {name} на порту знакомства {}", config.bootstrap.port());

    let (offer_tx, offer_rx) = mpsc::channel(TARGET_LINKS as usize);
    let mut sessions = Vec::new();
    let mut tasks = Vec::new();
    for base in bases {
        // Последняя сессия клиента для этого слота (с порта знакомства).
        let (client_tx, client_rx) = watch::channel(None);
        sessions.push(client_tx);
        let slot = ServerSlot {
            public_ip: config.public_ip,
            ports: config.bootstrap.ports(),
            pair: config.pair.clone(),
            offers: offer_tx.clone(),
            client_rx,
            base,
        };
        tasks.push(AbortOnDrop(tokio::spawn(slot.run())));
    }
    let (send, recv) = config.pair.secret.bootstrap_keys();
    let actor = ClientActor {
        socket,
        pair: config.pair.clone(),
        send,
        recv,
        offers: vec![None; TARGET_LINKS as usize],
        sessions,
    };
    tasks.push(AbortOnDrop(tokio::spawn(actor.run(knocks, offer_rx, guard))));
    Ok(tasks)
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

/// Владелец состояния клиента на сервере: записи слотов для знакомства и сессии клиента по слотам.
/// Блокировок нет: состояние меняет только эта задача, слоты пишут через канал.
struct ClientActor {
    socket: Arc<UdpSocket>,
    pair: Pair,
    send: SendKeys,
    recv: RecvKeys,
    offers: Vec<Option<Rendezvous>>,
    sessions: Vec<watch::Sender<Option<Uuid>>>,
}

impl ClientActor {
    async fn run(mut self, mut knocks: mpsc::Receiver<Knock>, mut updates: mpsc::Receiver<OfferUpdate>, _registration: Unregistration) {
        loop {
            tokio::select! {
                Some(knock) = knocks.recv() => self.knock(knock).await,
                Some(update) = updates.recv() => self.offers[update.slot as usize] = Some(update.record),
                else => return,
            }
        }
    }

    /// Запрос клиента по слоту k → его сессия слоту k, в ответ — запись слота k.
    async fn knock(&mut self, knock: Knock) {
        let Some(request) = unwrap(&knock.packet, &mut self.recv, &self.pair) else { return };
        let slot = request.slot as usize;
        self.sessions[slot].send_if_modified(|current| {
            let changed = *current != Some(request.session_id);
            if changed {
                log::debug!("знакомство: слот {slot}, клиент {}, сессия {}", knock.from, request.session_id);
                *current = Some(request.session_id);
            }
            changed
        });
        if let Some(offer) = self.offers[slot].clone() {
            let _ = self.socket.send_to(&codec::encode(&wrap(offer), &self.send), knock.from).await;
        }
    }
}

/// Рабочая задача одного слота сервера.
struct ServerSlot {
    public_ip: IpAddr,
    ports: Arc<PortPool>,
    pair: Pair,
    offers: mpsc::Sender<OfferUpdate>,
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
            // Порт остаётся в банке, пока жива эта дыра (`port_lease`): вернётся, когда слот начнёт заново.
            let (port_lease, socket) = match lease_and_bind(&self.ports).await {
                Ok((lease, socket)) => (lease, Arc::new(socket)),
                Err(e) => {
                    log::warn!("{label}слот {slot}: не удалось занять порт: {e:#}; повтор");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            let port = port_lease.port();
            let my_session = Uuid::new_v4();
            let record = self.pair.record(slot, my_session, SocketAddr::new(self.public_ip, port));
            let _ = self.offers.send(OfferUpdate { slot, record }).await;
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
    let name = peer_name(&config.pair.my_peer_id);
    let boot = Arc::new(ClientBootstrap { socket, server: config.server, send, name });
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
        let packet = |p: &Pair, slot: crate::multilink::SlotId| codec::encode(&wrap(p.record(slot, Uuid::new_v4(), endpoint)), &send);

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
