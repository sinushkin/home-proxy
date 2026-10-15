//! Релеи VPS-сервера (`vps-relay`): для клиента релей — ещё один такой же сервер (знакомство на
//! его порт, дыра на выданный им порт), а сам релей лишь перекладывает пакеты на настоящий сервер.
//!
//! Релей приходит к серверу сам — как VPS-клиент по своему GUID (`relays.txt` сервера), с белого
//! или серого адреса: сервер никуда не ходит. По дырам релея к серверу идут служебные сообщения
//! (`Control::Relay*`):
//!
//! ```text
//!   релей → сервер   RelayHello { ip, порт знакомства }        раз в 30 с; пустой ip — адрес, с которого пришёл
//!   клиент → релей   запрос знакомства (как серверу)
//!   релей → сервер   RelayKnock { пакет, откуда }              ключей клиентов релей не знает
//!   сервер → релей   RelayOpen { token, порт дыры сервера P }  сервер завёл дыру
//!   релей → сервер   RelayReady { token, порт релея Q }        сессия Q ↔ сервер:P
//!   сервер → релей   RelayReply { ответ, кому }                «твоя дыра — релей:Q», с порта знакомства релея
//!   клиент ↔ релей:Q ↔ сервер:P                                пакеты как есть, подпись сквозная
//! ```
//!
//! Раз в минуту сервер шлёт каждому клиенту `RelayOffer`: порты знакомства подключённых релеев и
//! долю новых дыр через них (`announce`).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

use super::{lease_and_bind, Bootstrap, NAME_LEN};
use crate::auth::peer_name;
use crate::discovery::Discovery;
use crate::multilink::{AbortOnDrop, Control, LinkRegistry, MultiLink, MultiLinkOptions};
use crate::port_pool::{PortLease, PortPool};
use crate::proto::{RelayHello, RelayKnock, RelayOffer, RelayOpen, RelayReady, RelayReply};

/// Доля новых дыр клиентов через релеи по умолчанию, %.
pub const DEFAULT_SHARE: u8 = 50;

/// Как часто сервер напоминает клиенту разрешённые релеи и сколько разрешение действует у клиента
/// (три напоминания: одно-два потерянных сообщения его не отменяют).
const ANNOUNCE: Duration = Duration::from_secs(60);
const VALID: Duration = Duration::from_secs(180);
/// Как часто сервер проверяет, пора ли объявить релеи клиенту (первый раз — как только есть что).
const ANNOUNCE_CHECK: Duration = Duration::from_secs(5);
/// По скольким живым дырам шлём объявление (на случай потери пакета).
const ANNOUNCE_COPIES: usize = 3;
/// Сколько сервер ждёт, пока релей заведёт сессию.
const OPEN_WAIT: Duration = Duration::from_secs(5);
/// Keep-alive релея к серверу: `RelayHello` по случайной дыре раз в столько, если за это время
/// релей и так ничего серверу не отправил (заодно напоминает порт знакомства).
const KEEPALIVE: Duration = Duration::from_secs(5);
/// `RelayHello` (порт знакомства) уходит не реже этого, даже если обмен и так идёт.
const HELLO_EVERY: Duration = Duration::from_secs(30);
/// Сервер: от релея нет пакетов дольше этого — релей выбывает, клиентам сразу новый список.
const SILENCE: Duration = Duration::from_secs(10);
/// Как часто сервер проверяет, жив ли релей.
const LIVENESS_CHECK: Duration = Duration::from_secs(1);
/// Сессия релея без пакетов в обе стороны дольше этого закрывается (keep-alive дыр — 2–10 с).
const SESSION_IDLE: Duration = Duration::from_secs(120);
/// Буфер под пакет: пакет в дыре не больше 1500 байт.
const BUF: usize = 2048;

// ---------------------------------------------------------------------------------------------
// Сервер
// ---------------------------------------------------------------------------------------------

/// Релеи процесса сервера (общие для всех клиентов, живут в `Bootstrap`).
pub(crate) struct Relays {
    inner: Mutex<Inner>,
    /// Поколение списка релеев: растёт, когда релей появился, выбыл или сменил адрес — задачи
    /// объявления клиентам шлют новый список сразу.
    changes: watch::Sender<u64>,
}

impl Default for Relays {
    fn default() -> Self {
        Self { inner: Mutex::default(), changes: watch::channel(0).0 }
    }
}

#[derive(Default)]
struct Inner {
    /// Доля новых дыр через релеи, % (`None` — по умолчанию).
    share: Option<u8>,
    relays: HashMap<String, Entry>,
    next_token: u32,
    /// Поручения сессий, ждущие `RelayReady`: token → порт релея.
    pending: HashMap<u32, oneshot::Sender<u16>>,
}

struct Entry {
    /// Связь с релеем (его дыры к серверу); `Weak` — реестр не держит `MultiLink` живым.
    link: Weak<MultiLink>,
    /// Порт знакомства релея для клиентов (из `RelayHello`).
    public: Option<SocketAddr>,
    /// От релея недавно были пакеты (`SILENCE`): только такие — в списке для клиентов.
    alive: bool,
}

impl Relays {
    pub(crate) fn set_share(&self, share: u8) {
        self.inner.lock().unwrap().share = Some(share.min(100));
    }

    pub(crate) fn is_relay(&self, name: &str) -> bool {
        self.inner.lock().unwrap().relays.contains_key(name)
    }

    fn expect(&self, name: &str) {
        self.inner.lock().unwrap().relays.insert(name.to_string(), Entry { link: Weak::new(), public: None, alive: false });
    }

    fn attach(&self, name: &str, link: Weak<MultiLink>) {
        if let Some(entry) = self.inner.lock().unwrap().relays.get_mut(name) {
            entry.link = link;
        }
    }

    fn set_public(&self, name: &str, public: SocketAddr) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.relays.get_mut(name)
            && entry.public != Some(public)
        {
            log::info!("релей {name}: порт знакомства для клиентов {public}");
            entry.public = Some(public);
            drop(inner);
            self.changes.send_modify(|g| *g = g.wrapping_add(1));
        }
    }

    fn set_alive(&self, name: &str, alive: bool) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.relays.get_mut(name)
            && entry.alive != alive
        {
            if alive {
                log::info!("релей {name}: на связи");
            } else {
                log::warn!("релей {name}: нет пакетов дольше {SILENCE:?} — убираем из списка клиентов");
            }
            entry.alive = alive;
            drop(inner);
            self.changes.send_modify(|g| *g = g.wrapping_add(1));
        }
    }

    fn link(&self, name: &str) -> Option<Arc<MultiLink>> {
        self.inner.lock().unwrap().relays.get(name)?.link.upgrade()
    }

    /// Ответ клиенту на знакомство, пришедшее через релей `name`.
    pub(crate) async fn reply(&self, name: &str, to: SocketAddr, packet: Vec<u8>) {
        if let Some(link) = self.link(name) {
            let _ = link.send_control(Control::RelayReply(RelayReply { packet, to: to.to_string() })).await;
        }
    }

    /// Поручить релею `name` сессию на порт дыры сервера `server_port`; адрес сессии для клиента.
    pub(crate) async fn open(&self, name: &str, server_port: u16) -> Option<SocketAddr> {
        let (link, public, token, rx) = {
            let mut inner = self.inner.lock().unwrap();
            let entry = inner.relays.get(name)?;
            let (link, public) = (entry.link.upgrade()?, entry.public?);
            inner.next_token = inner.next_token.wrapping_add(1);
            let token = inner.next_token;
            let (tx, rx) = oneshot::channel();
            inner.pending.insert(token, tx);
            (link, public, token, rx)
        };
        let sent = link.send_control(Control::RelayOpen(RelayOpen { token, server_port: u32::from(server_port) })).await;
        let port = match sent {
            Ok(_) => tokio::time::timeout(OPEN_WAIT, rx).await.ok().and_then(Result::ok),
            Err(_) => None,
        };
        self.inner.lock().unwrap().pending.remove(&token);
        Some(SocketAddr::new(public.ip(), port?))
    }

    fn ready(&self, token: u32, port: u16) {
        if let Some(tx) = self.inner.lock().unwrap().pending.remove(&token) {
            let _ = tx.send(port);
        }
    }

    /// Разрешение для клиентов: живые релеи с известным портом знакомства (пустой список — релеев нет).
    fn offer(&self) -> RelayOffer {
        let inner = self.inner.lock().unwrap();
        let share = inner.share.unwrap_or(DEFAULT_SHARE);
        let mut relays: Vec<String> = if share == 0 {
            Vec::new()
        } else {
            inner.relays.values().filter(|e| e.alive).filter_map(|e| e.public.map(|a| a.to_string())).collect()
        };
        relays.sort();
        RelayOffer { relays, share: u32::from(share), valid_secs: VALID.as_secs() as u32 }
    }
}

/// Сообщает клиенту разрешённые релеи: как только они есть и у клиента есть живые дыры, затем раз в
/// минуту; список изменился (релей выбыл, появился) — сразу. Опустел — клиенту пустой список (он
/// перестаёт открывать дыры через релеи); релеев не было вовсе — молчим.
pub(crate) async fn announce(relays: Arc<Relays>, registry: Arc<Mutex<LinkRegistry>>) {
    let mut changes = relays.changes.subscribe();
    let mut last: Option<(Instant, Vec<String>)> = None;
    let mut ticker = tokio::time::interval(ANNOUNCE_CHECK);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            Ok(()) = changes.changed() => {}
        }
        let offer = relays.offer();
        let due = match &last {
            None => !offer.relays.is_empty(),
            Some((at, sent)) => *sent != offer.relays || (!offer.relays.is_empty() && at.elapsed() >= ANNOUNCE),
        };
        if !due {
            continue;
        }
        let links = registry.lock().unwrap().links();
        if links.is_empty() {
            continue;
        }
        for link in links.iter().take(ANNOUNCE_COPIES) {
            link.sender.send_relay_offer(offer.clone()).await;
        }
        last = Some((Instant::now(), offer.relays));
    }
}

/// Подключение релея к серверу (сторона сервера): его дыры и разбор его сообщений.
pub struct RelayLink {
    pub link: Arc<MultiLink>,
    _task: AbortOnDrop,
}

/// Принимает релей `relay_id` на порту знакомства сервера (как клиента) и подключает его к
/// реестру релеев: его клиентам можно вести дыры через него.
pub async fn accept(
    bootstrap: &Bootstrap,
    public_ip: IpAddr,
    server_id: Uuid,
    relay_id: Uuid,
    options: MultiLinkOptions,
) -> Result<RelayLink> {
    let name = peer_name(&relay_id);
    // До старта: дырам релея сервер не шлёт разрешение релеев.
    bootstrap.relays.expect(&name);
    let (link, _incoming) = MultiLink::start_discovery(
        &format!("релей {name}"),
        Discovery::VpsServer { public_ip, bootstrap: bootstrap.clone() },
        server_id,
        relay_id,
        options,
    )
    .await?;
    let link = Arc::new(link);
    bootstrap.relays.attach(&name, Arc::downgrade(&link));
    let mut control = link.take_control().expect("приёмник служебных сообщений забираем один раз");
    log::info!("VPS-сервер: ждём релей {name}");
    let task = tokio::spawn({
        let (bootstrap, link) = (bootstrap.clone(), Arc::downgrade(&link));
        async move {
            let _incoming = _incoming;
            // Живость: счётчик полученных пакетов по всем дырам релея (keep-alive, служебные).
            let (mut received, mut heard) = (0u64, Instant::now() - SILENCE);
            let mut check = tokio::time::interval(LIVENESS_CHECK);
            loop {
                let message = tokio::select! {
                    message = control.recv() => match message {
                        Some(message) => message,
                        None => return,
                    },
                    _ = check.tick() => {
                        let Some(relay) = link.upgrade() else { return };
                        let now = relay.packet_totals().1;
                        if now != received {
                            (received, heard) = (now, Instant::now());
                        }
                        bootstrap.relays.set_alive(&name, relay.live_count() > 0 && heard.elapsed() <= SILENCE);
                        continue;
                    }
                };
                match message {
                    Control::RelayHello(hello) => {
                        let Ok(port) = u16::try_from(hello.bootstrap_port) else { continue };
                        // Без адреса в сообщении — адрес, с которого релей пришёл к нам.
                        let ip = hello.public_ip.parse().ok().or_else(|| {
                            link.upgrade()?.live_links().into_iter().find_map(|(_, addr)| addr).map(|a| a.ip())
                        });
                        if let Some(ip) = ip {
                            bootstrap.relays.set_public(&name, SocketAddr::new(ip, port));
                        }
                    }
                    Control::RelayKnock(knock) => {
                        if let Ok(from) = knock.from.parse() {
                            bootstrap.inject(&knock.packet, from, name.clone()).await;
                        }
                    }
                    Control::RelayReady(ready) => {
                        if let Ok(port) = u16::try_from(ready.port) {
                            bootstrap.relays.ready(ready.token, port);
                        }
                    }
                    _ => {}
                }
            }
        }
    });
    Ok(RelayLink { link, _task: AbortOnDrop(task) })
}

// ---------------------------------------------------------------------------------------------
// Релей
// ---------------------------------------------------------------------------------------------

/// Настройки релея.
#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// Порт знакомства сервера, к которому релей приходит сам.
    pub server: SocketAddr,
    pub my_id: Uuid,
    pub server_id: Uuid,
    /// Белый адрес релея для клиентов; `None` — сервер возьмёт адрес, с которого пришёл релей.
    pub public_ip: Option<IpAddr>,
    /// Порт знакомства релея для клиентов и порты сессий.
    pub bootstrap_port: u16,
    pub ports: RangeInclusive<u16>,
    /// Настройки дыр релея к серверу.
    pub options: MultiLinkOptions,
}

/// Работающий релей: дроп останавливает его.
pub struct Relay {
    pub link: Arc<MultiLink>,
    _task: AbortOnDrop,
}

/// Запускает релей: дыры к серверу, порт знакомства для клиентов, сессии по поручению сервера.
pub async fn start(config: RelayConfig) -> Result<Relay> {
    anyhow::ensure!(!config.ports.contains(&config.bootstrap_port), "порт знакомства {} внутри диапазона сессий", config.bootstrap_port);
    let boot = Arc::new(
        UdpSocket::bind(("0.0.0.0", config.bootstrap_port))
            .await
            .with_context(|| format!("не удалось занять порт знакомства {}", config.bootstrap_port))?,
    );
    let pool = PortPool::new(config.ports.clone());
    let (link, incoming) =
        MultiLink::start_discovery("сервер", Discovery::VpsClient { server: config.server }, config.my_id, config.server_id, config.options)
            .await?;
    let link = Arc::new(link);
    let control = link.take_control().expect("приёмник служебных сообщений забираем один раз");
    log::info!(
        "vps-relay: сервер {}, порт знакомства {} ({}), сессии {}-{}",
        config.server,
        config.bootstrap_port,
        config.public_ip.map_or("адрес — как видит сервер".to_string(), |ip| ip.to_string()),
        config.ports.start(),
        config.ports.end()
    );
    let task = tokio::spawn(serve(config, boot, pool, Arc::downgrade(&link), control, incoming));
    Ok(Relay { link, _task: AbortOnDrop(task) })
}

async fn serve(
    config: RelayConfig,
    boot: Arc<UdpSocket>,
    pool: Arc<PortPool>,
    link: Weak<MultiLink>,
    mut control: tokio::sync::mpsc::Receiver<Control>,
    _incoming: tokio::sync::mpsc::Receiver<crate::multilink::Incoming>,
) {
    let hello = RelayHello {
        public_ip: config.public_ip.map(|ip| ip.to_string()).unwrap_or_default(),
        bootstrap_port: u32::from(config.bootstrap_port),
    };
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    let (mut sent, mut hello_at): (u64, Option<Instant>) = (0, None);
    let mut buf = [0u8; BUF];
    loop {
        let Some(server) = link.upgrade() else { return };
        tokio::select! {
            received = boot.recv_from(&mut buf) => {
                let Ok((n, from)) = received else { continue };
                if n > NAME_LEN && n <= 1500 {
                    let knock = RelayKnock { packet: buf[..n].to_vec(), from: from.to_string() };
                    match server.send_control(Control::RelayKnock(knock)).await {
                        Ok(slot) => log::debug!("знакомство от {from} ({n} байт) — серверу по дыре {slot}"),
                        Err(e) => log::warn!("знакомство от {from} серверу не передано: {e:#}"),
                    }
                }
            }
            message = control.recv() => match message {
                Some(Control::RelayReply(reply)) => {
                    if let Ok(to) = reply.to.parse::<SocketAddr>() {
                        let _ = boot.send_to(&reply.packet, to).await;
                    }
                }
                Some(Control::RelayOpen(open)) => {
                    let Ok(server_port) = u16::try_from(open.server_port) else { continue };
                    match open_session(&pool, SocketAddr::new(config.server.ip(), server_port)).await {
                        Ok(port) => {
                            log::info!("сессия {}: порт {port} ↔ сервер:{server_port}", open.token);
                            let ready = RelayReady { token: open.token, port: u32::from(port) };
                            let _ = server.send_control(Control::RelayReady(ready)).await;
                        }
                        Err(e) => log::warn!("сессия {}: {e:#}", open.token),
                    }
                }
                Some(_) => {}
                None => return,
            },
            _ = keepalive.tick() => {
                // Keep-alive не нужен, если за период и так что-то ушло серверу; порт знакомства
                // напоминаем всё равно — при подключении и раз в `HELLO_EVERY`.
                let idle = server.packet_totals().0 == sent;
                if (idle || hello_at.is_none_or(|t| t.elapsed() >= HELLO_EVERY))
                    && server.send_control(Control::RelayHello(hello.clone())).await.is_ok()
                {
                    hello_at = Some(Instant::now());
                }
                sent = server.packet_totals().0;
            }
        }
    }
}

/// Сессия: порт для клиента из банка и сокет к порту дыры сервера; пакеты — как есть.
async fn open_session(pool: &Arc<PortPool>, server: SocketAddr) -> Result<u16> {
    let (lease, front) = lease_and_bind(pool).await?;
    let back = UdpSocket::bind(("0.0.0.0", 0)).await.context("сокет к серверу")?;
    back.connect(server).await.context("connect к серверу")?;
    let port = lease.port();
    tokio::spawn(forward(front, back, lease));
    Ok(port)
}

async fn forward(front: UdpSocket, back: UdpSocket, _lease: PortLease) {
    let (mut from_client, mut from_server) = ([0u8; BUF], [0u8; BUF]);
    let mut client: Option<SocketAddr> = None;
    let idle = tokio::time::sleep(SESSION_IDLE);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            received = front.recv_from(&mut from_client) => {
                let Ok((n, from)) = received else { continue };
                // Адрес клиента — откуда пришёл последний пакет (у клиента может смениться маппинг).
                client = Some(from);
                let _ = back.send(&from_client[..n]).await;
                idle.as_mut().reset(tokio::time::Instant::now() + SESSION_IDLE);
            }
            received = back.recv(&mut from_server) => {
                let Ok(n) = received else { continue };
                if let Some(client) = client {
                    let _ = front.send_to(&from_server[..n], client).await;
                }
                idle.as_mut().reset(tokio::time::Instant::now() + SESSION_IDLE);
            }
            _ = &mut idle => {
                log::debug!("сессия на порту {}: простой, закрыта", front.local_addr().map_or(0, |a| a.port()));
                return;
            }
        }
    }
}
