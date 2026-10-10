//! `hp-router`: роутер OpenWrt с двумя ролями в одном процессе (один бинарник — меньше места на
//! флеше).
//!
//! 1. Шлюз дома: TUN (`hp0`) ↔ 10 дыр к VPS с белым IP (как `vps-client`). Весь трафик LAN и
//!    Wi-Fi уходит к VPS как есть, без WireGuard; TCP с номером в потоке, VPS возвращает порядок.
//! 2. Пир для телефонов: на каждый телефон свой набор из 10 P2P-дыр (STUN + MQTT, как у
//!    `hp-server`). Пакеты телефона роутер не разбирает и порядок им не восстанавливает — только
//!    перекладывает в дыры VPS как `WrappedData { client_id }` с номерами телефона; ответы VPS для
//!    этого `client_id` — обратно телефону. Порядок возвращает конечный получатель (VPS или
//!    телефон), TUN и стек ядра роутера пакеты телефонов не проходят.
//!
//! Телефоны задаются в настройках (`PHONE_<n>_*`) или добавляются из трея по протоколу
//! управления (`hp-control`): роутер сам заводит пару GUID и номер телефона, после первого
//! подключения телефон сохраняется в `phones.state`.
//!
//! Запуск: `hp-router [--config router.env] [--connection-string | --new-connection-string]`
//! (без `--config` — `router.env` рядом с бинарником, если есть, иначе только окружение).
//! `--connection-string` печатает строку подключения трея, `--new-connection-string` меняет ключ
//! (прежние строки перестают работать сразу) — это вызывает LuCI. Настройки:
//!   VPS_SERVER — адрес VPS `ip[:порт знакомства]` (порт по умолчанию 40000);
//!   VPS_MY_ID / VPS_PEER_ID — GUID роутера и VPS-сервера;
//!   TUN_NAME (`hp0`), TUN_MTU (1400) — адрес в туннеле роутеру выдаёт VPS (и телефонам тоже:
//!   их запросы роутер пересылает на VPS со своим номером телефона);
//!   STUN_ADDR (один или несколько через запятую), MQTT_ADDR, MQTT_CA — для телефонов (без них
//!   телефонов нет и сопряжение недоступно);
//!   PHONE_<n>_MY_ID / PHONE_<n>_PEER_ID — GUID роутера и телефона n, n = 1, 2, 3 … подряд
//!   (до 255); `n` и есть `client_id`. У каждого набора свой GUID роутера: брокер различает
//!   клиентов по нему;
//!   CONTROL_ADDR — адрес протокола управления в LAN (`192.168.1.1:47001`; по умолчанию выключен:
//!   не `0.0.0.0` и не WAN); CONTROL_KEY_FILE (`control.key`), PHONES_FILE (`phones.state`) —
//!   рядом с настройками;
//!   REORDER_WAIT_MS, DATA_HOLES — как у `vps-client`; RUNTIME=multi — многопоточный tokio
//!   (по умолчанию однопоточный: на одноядерном роутере так быстрее);
//!   ROUTES — `auto` (по умолчанию): служба сама ставит маршруты — всё в `hp0`, кроме VPS, STUN
//!   и MQTT, сокеты дыр привязаны к аплинку (`routes.rs`); `off` — маршруты вручную;
//!   ON_TUN_UP (`on-tun-up.sh`), ON_TUN_DOWN (`on-tun-down.sh`) — скрипты после подъёма туннеля
//!   и при остановке службы, рядом с настройками (`routes::Hooks`);
//!   RUST_LOG, LOG_FILE — логи.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use connection::auth::peer_name;
use connection::discovery::Discovery;
use connection::multilink::{ConnState, Control, Incoming, LinkStatus, MultiLink, MultiLinkOptions, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use connection::proto::AddressKind;
use hp_control::proto;
use hp_server::service::{now_unix, pairing, PAIRING_TTL};
use hp_server::settings::Settings;
use hp_server::Peer;
use tokio::sync::mpsc;
use uuid::Uuid;

use hp_tun::routes;

const STATUS_INTERVAL: Duration = Duration::from_secs(30);
/// Как часто сверять маршруты и аплинк (`routes.rs`).
const ROUTES_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Phone {
    /// Номер телефона (`n` из `PHONE_<n>_*`), он же `client_id` в `WrappedData`.
    client_id: u8,
    my_id: Uuid,
    peer_id: Uuid,
}

/// Общее для наборов дыр к телефонам.
#[derive(Clone)]
struct PhoneDiscovery {
    stun_addrs: Vec<SocketAddr>,
    mqtt_addr: SocketAddr,
    mqtt_ca_pem: Vec<u8>,
}

struct Config {
    vps_server: SocketAddr,
    vps_my_id: Uuid,
    vps_peer_id: Uuid,
    tun_name: String,
    tun_mtu: u16,
    reorder_wait: Duration,
    data_holes: u8,
    /// Динамический набор дыр (`HOLES_MIN`, `HOLES_MAX`, `HOLE_AGE`) — к VPS и к телефонам.
    pool: connection::holes::PoolPolicy,
    hole_age: (Duration, Duration),
    phones: Vec<Phone>,
    /// `None` — STUN/MQTT не заданы: телефонов нет, сопряжение недоступно.
    phone_discovery: Option<PhoneDiscovery>,
    control: Option<SocketAddr>,
    key_file: PathBuf,
    phones_file: PathBuf,
    /// `ROUTES=auto`: маршруты и привязку сокетов к аплинку ставит служба.
    routes: bool,
    on_tun_up: PathBuf,
    on_tun_down: PathBuf,
}

impl Config {
    fn from_settings(settings: &Settings) -> Result<Self> {
        let get = |name: &str| settings.get(name);
        let need = |name: &str| get(name).with_context(|| format!("не задана переменная {name}"));
        let parse_id = |name: &str| -> Result<Uuid> { need(name)?.trim().parse().with_context(|| format!("{name}: некорректный GUID")) };
        let number = |name: &str, default: u64| -> Result<u64> {
            get(name).map_or(Ok(default), |v| v.trim().parse().with_context(|| format!("{name}: ожидается число")))
        };

        let phones = parse_phones(&get)?;
        let phone_discovery = if phones.is_empty() && get("STUN_ADDR").is_none() {
            None
        } else {
            let mqtt_ca = settings.resolve(need("MQTT_CA")?.trim());
            Some(PhoneDiscovery {
                stun_addrs: connection::stun::parse_servers(&need("STUN_ADDR")?).context("STUN_ADDR")?,
                mqtt_addr: need("MQTT_ADDR")?.trim().parse().context("MQTT_ADDR: ожидается ip:порт")?,
                mqtt_ca_pem: std::fs::read(&mqtt_ca).with_context(|| format!("не удалось прочитать CA-сертификат {}", mqtt_ca.display()))?,
            })
        };
        let config = Self {
            vps_server: connection::vps::parse_server(&need("VPS_SERVER")?).context("VPS_SERVER: ожидается ip или ip:порт")?,
            vps_my_id: parse_id("VPS_MY_ID")?,
            vps_peer_id: parse_id("VPS_PEER_ID")?,
            tun_name: get("TUN_NAME").unwrap_or_else(|| "hp0".into()),
            tun_mtu: u16::try_from(number("TUN_MTU", 1400)?).context("TUN_MTU")?,
            reorder_wait: Duration::from_millis(number("REORDER_WAIT_MS", DEFAULT_REORDER_WAIT.as_millis() as u64)?),
            data_holes: u8::try_from(number("DATA_HOLES", 0)?).context("DATA_HOLES")?,
            pool: hp_server::holes_settings(&get)?.0,
            hole_age: hp_server::holes_settings(&get)?.1,
            phones,
            phone_discovery,
            control: control_addr(&get)?,
            key_file: settings.resolve(get("CONTROL_KEY_FILE").as_deref().unwrap_or(hp_control::KEY_FILE).trim()),
            phones_file: settings.resolve(get("PHONES_FILE").as_deref().unwrap_or("phones.state").trim()),
            routes: match get("ROUTES").as_deref().map(str::trim) {
                None | Some("") | Some("auto") => true,
                Some("off") => false,
                Some(other) => anyhow::bail!("ROUTES: ожидается auto или off, а не {other}"),
            },
            on_tun_up: settings.resolve(get("ON_TUN_UP").as_deref().unwrap_or("on-tun-up.sh").trim()),
            on_tun_down: settings.resolve(get("ON_TUN_DOWN").as_deref().unwrap_or("on-tun-down.sh").trim()),
        };

        let mut router_ids = HashSet::from([config.vps_my_id]);
        for phone in &config.phones {
            anyhow::ensure!(
                router_ids.insert(phone.my_id),
                "GUID роутера {} встречается дважды: у каждого набора дыр он должен быть свой",
                peer_name(&phone.my_id)
            );
        }
        Ok(config)
    }
}

/// Туннель снимается: маршруты /1 (`ROUTES=auto`), затем `on-tun-down.sh`; `result` — с чем
/// выйти (ошибка — procd перезапустит службу).
async fn shutdown_tunnel(routes: Option<&mut routes::Routes>, hooks: &routes::Hooks, result: Result<()>) -> Result<()> {
    if let Some(routes) = routes {
        routes.tunnel_down().await;
    }
    hooks.down().await;
    result
}

/// Переменные окружения хуков (описание — `hp_tun::routes::hook_env`).
fn hook_env_for(config: &Config, tun: &str, assigned: &hp_tun::bridge::Assigned, routes: Option<&routes::Routes>) -> Vec<(&'static str, String)> {
    let std::net::IpAddr::V4(vps_ip) = config.vps_server.ip() else { return Vec::new() };
    routes::hook_env(tun, (assigned.address, assigned.prefix), &assigned.dns, vps_ip, &bypass_hosts(config), routes)
}

/// Адреса, которые идут мимо туннеля: VPS, STUN, MQTT (IPv4).
fn bypass_hosts(config: &Config) -> Vec<Ipv4Addr> {
    let mut addrs = vec![config.vps_server];
    if let Some(d) = &config.phone_discovery {
        addrs.extend(d.stun_addrs.iter().copied());
        addrs.push(d.mqtt_addr);
    }
    addrs
        .into_iter()
        .filter_map(|a| match a.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            std::net::IpAddr::V6(_) => None,
        })
        .fold(Vec::new(), |mut unique, ip| {
            // STUN и MQTT часто на одном хосте.
            if !unique.contains(&ip) {
                unique.push(ip);
            }
            unique
        })
}

fn vps_probe(config: &Config) -> Result<Ipv4Addr> {
    match config.vps_server.ip() {
        std::net::IpAddr::V4(ip) => Ok(ip),
        std::net::IpAddr::V6(_) => anyhow::bail!("ROUTES=auto: VPS_SERVER должен быть IPv4 (или ROUTES=off)"),
    }
}

/// `CONTROL_ADDR`: выключено, если не задан или `off`; `0.0.0.0` не принимаем — только адрес LAN.
fn control_addr(get: &impl Fn(&str) -> Option<String>) -> Result<Option<SocketAddr>> {
    match get("CONTROL_ADDR").as_deref().map(str::trim) {
        None | Some("") | Some("off") => Ok(None),
        Some(text) => {
            let addr: SocketAddr = text.parse().context("CONTROL_ADDR: ожидается ip:порт адреса LAN")?;
            anyhow::ensure!(!addr.ip().is_unspecified(), "CONTROL_ADDR: не 0.0.0.0 — только адрес LAN (иначе управление видно из WAN)");
            Ok(Some(addr))
        }
    }
}

/// `PHONE_<n>_MY_ID` / `PHONE_<n>_PEER_ID`, n = 1, 2, 3 … подряд.
fn parse_phones(get: &impl Fn(&str) -> Option<String>) -> Result<Vec<Phone>> {
    let mut phones = Vec::new();
    let mut phone_ids = HashSet::new();
    for client_id in 1..=u8::MAX {
        let my_name = format!("PHONE_{client_id}_MY_ID");
        let peer_name = format!("PHONE_{client_id}_PEER_ID");
        let (my, peer) = match (get(&my_name), get(&peer_name)) {
            (None, None) => break,
            (Some(my), Some(peer)) => (my, peer),
            _ => anyhow::bail!("телефон {client_id}: нужны обе переменные {my_name} и {peer_name}"),
        };
        let phone = Phone {
            client_id,
            my_id: my.trim().parse().with_context(|| format!("{my_name}: некорректный GUID"))?,
            peer_id: peer.trim().parse().with_context(|| format!("{peer_name}: некорректный GUID"))?,
        };
        anyhow::ensure!(phone_ids.insert(phone.peer_id), "GUID телефона {} указан дважды", connection::auth::peer_name(&phone.peer_id));
        phones.push(phone);
    }
    Ok(phones)
}

/// Что сделать вместо запуска.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Run,
    ConnectionString,
    NewConnectionString,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<(Option<PathBuf>, Action)> {
    let (mut config, mut action) = (None, Action::Run);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = Some(args.next().context("--config: нужен путь")?.into()),
            "--connection-string" => action = Action::ConnectionString,
            "--new-connection-string" => action = Action::NewConnectionString,
            _ => anyhow::bail!("использование: hp-router [--config router.env] [--connection-string | --new-connection-string]"),
        }
    }
    if config.is_none() {
        let default = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("router.env")));
        config = default.filter(|p| p.is_file());
    }
    Ok((config, action))
}

fn main() -> Result<()> {
    let (config_path, action) = parse_args(std::env::args().skip(1))?;
    let settings = Settings::load(config_path.as_deref())?;
    if action != Action::Run {
        let get = |name: &str| settings.get(name);
        let addr = control_addr(&get)?.context("управление выключено: задайте CONTROL_ADDR=<адрес LAN>:47001")?;
        let key_file = settings.resolve(get("CONTROL_KEY_FILE").as_deref().unwrap_or(hp_control::KEY_FILE).trim());
        let key = if action == Action::NewConnectionString { hp_control::replace_key(&key_file)? } else { hp_control::load_or_create_key(&key_file)? };
        println!("{}", hp_control::connection_string(addr, &key));
        return Ok(());
    }
    hp_server::init_logging(&settings)?;
    let config = Config::from_settings(&settings)?;
    let runtime = if settings.get("RUNTIME").as_deref() == Some("multi") {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    } else {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?
    };
    runtime.block_on(run(config))
}

/// Счётчики одного направления пересылки (32 бита: на MIPS32 64-битных атомиков нет).
#[derive(Default)]
struct DirectionStats {
    forwarded: std::sync::atomic::AtomicU32,
    dropped: std::sync::atomic::AtomicU32,
}

impl DirectionStats {
    fn snapshot(&self) -> (u32, u32) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.forwarded.load(Relaxed), self.dropped.load(Relaxed))
    }
}

/// Счётчики пересылки телефонов.
#[derive(Default)]
struct RelayStats {
    to_vps: DirectionStats,
    to_phones: DirectionStats,
}

/// Откуда телефон.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// `PHONE_<n>_*` в настройках: удалить можно только там.
    Settings,
    /// Сопряжён из трея, хранится в `phones.state`.
    Paired,
    /// Ждёт первого подключения до этого момента (unix-время).
    Pending(u64),
}

struct PhoneEntry {
    phone: Phone,
    link: Arc<MultiLink>,
    origin: Origin,
    /// Адрес в туннеле, который VPS выдал телефону.
    address: Option<Ipv4Addr>,
    _tasks: [AbortOnDrop; 2],
}

/// Телефоны по `client_id`. Пакеты VPS → телефон берут блокировку на чтение (без записи в
/// горячем пути); запись — только при добавлении, удалении и выдаче адреса.
type Phones = Arc<RwLock<HashMap<u8, PhoneEntry>>>;

fn link_of(phones: &Phones, client_id: u8) -> Option<Arc<MultiLink>> {
    phones.read().unwrap().get(&client_id).map(|e| e.link.clone())
}

struct Router {
    vps: Arc<MultiLink>,
    vps_address: Ipv4Addr,
    phones: Phones,
    discovery: Option<PhoneDiscovery>,
    phone_options: MultiLinkOptions,
    stats: Arc<RelayStats>,
    bridge: hp_tun::bridge::Bridge,
    phones_file: PathBuf,
    started: Instant,
    /// Сопряжения по одному: номер телефона и ожидающий набор выбираются без гонок.
    pairing_lock: tokio::sync::Mutex<()>,
}

impl Router {
    async fn add_phone(&self, phone: Phone, origin: Origin) -> Result<()> {
        let discovery = self.discovery.as_ref().context("для телефонов нужны STUN_ADDR, MQTT_ADDR и MQTT_CA")?;
        let (link, rx) = MultiLink::start_with(
            &peer_name(&phone.peer_id),
            discovery.stun_addrs.clone(),
            discovery.mqtt_addr,
            discovery.mqtt_ca_pem.clone(),
            phone.my_id,
            phone.peer_id,
            self.phone_options,
        )
        .await
        .with_context(|| format!("телефон {}", phone.client_id))?;
        let link = Arc::new(link);
        let control = link.take_control().expect("приёмник служебных сообщений забираем один раз");
        let tasks = [
            AbortOnDrop(tokio::spawn(phone_to_vps(phone.client_id, rx, self.vps.clone(), self.stats.clone()))),
            AbortOnDrop(tokio::spawn(phone_requests_to_vps(phone.client_id, control, self.vps.clone()))),
        ];
        log::info!("телефон {} ({}): ищем", phone.client_id, peer_name(&phone.peer_id));
        self.phones.write().unwrap().insert(phone.client_id, PhoneEntry { phone, link, origin, address: None, _tasks: tasks });
        Ok(())
    }

    /// Сопряжённые раньше телефоны из `phones.state`: `client_id GUID-роутера GUID-телефона`.
    async fn add_paired_from_file(&self) -> Result<()> {
        let Ok(text) = std::fs::read_to_string(&self.phones_file) else { return Ok(()) };
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let parsed = (parts.next().map(str::parse::<u8>), parts.next().map(str::parse::<Uuid>), parts.next().map(str::parse::<Uuid>));
            let (Some(Ok(client_id)), Some(Ok(my_id)), Some(Ok(peer_id))) = parsed else {
                log::warn!("{}:{}: ожидается «номер GUID-роутера GUID-телефона», строка пропущена", self.phones_file.display(), n + 1);
                continue;
            };
            if client_id == 0 || self.phones.read().unwrap().contains_key(&client_id) {
                log::warn!("{}:{}: номер телефона {client_id} уже занят, строка пропущена", self.phones_file.display(), n + 1);
                continue;
            }
            self.add_phone(Phone { client_id, my_id, peer_id }, Origin::Paired).await?;
        }
        Ok(())
    }

    fn save_paired(&self) -> Result<()> {
        let mut text = String::from("# Сопряжённые телефоны: номер, GUID роутера, GUID телефона. Секрет пары — не публиковать.\n");
        let phones = self.phones.read().unwrap();
        let mut paired: Vec<&PhoneEntry> = phones.values().filter(|e| e.origin == Origin::Paired).collect();
        paired.sort_by_key(|e| e.phone.client_id);
        for e in paired {
            text.push_str(&format!("{} {} {}\n", e.phone.client_id, e.phone.my_id, e.phone.peer_id));
        }
        hp_control::write_private(&self.phones_file, &text).with_context(|| format!("запись {}", self.phones_file.display()))
    }

    /// Раз в секунду: ожидающий телефон подключился — сохранить; истёк — остановить.
    fn housekeeping(&self) {
        let now = now_unix();
        let mut promoted = false;
        self.phones.write().unwrap().retain(|_, e| {
            let Origin::Pending(expires) = e.origin else { return true };
            if e.link.live_count() > 0 {
                log::info!("сопряжение: телефон {} подключился", e.phone.client_id);
                e.origin = Origin::Paired;
                promoted = true;
                true
            } else if now > expires {
                log::info!("сопряжение: телефон {} не подключился, пакет истёк", e.phone.client_id);
                false
            } else {
                true
            }
        });
        if promoted && let Err(e) = self.save_paired() {
            log::warn!("не удалось сохранить сопряжённые телефоны: {e:#}");
        }
    }

    fn live_counts(&self) -> Vec<(u8, usize)> {
        let mut v: Vec<_> = self.phones.read().unwrap().iter().map(|(id, e)| (*id, e.link.live_count())).collect();
        v.sort_unstable();
        v
    }
}

fn peer_status(name: String, kind: &str, status: LinkStatus) -> proto::PeerStatus {
    proto::PeerStatus {
        name,
        state: match status.state {
            ConnState::Rendezvous => "rendezvous",
            ConnState::Punching => "punching",
            ConnState::Connected(_) => "connected",
        }
        .into(),
        live: status.in_work() as u32,
        target: TARGET_LINKS,
        holes: status
            .holes
            .iter()
            .map(|h| proto::HoleStatus {
                slot: h.slot,
                peer_addr: h.peer_addr.map(|a| a.to_string()).unwrap_or_default(),
                sent: h.sent,
                received: h.received,
                loss_out: h.loss_out.unwrap_or(-1.0),
                loss_in: h.loss_in.unwrap_or(-1.0),
                age_secs: h.age.as_secs().min(u64::from(u32::MAX)) as u32,
                draining: h.state == connection::holes::HoleState::Draining,
            })
            .collect(),
        kind: kind.into(),
        reorder_wait_ms: status.reorder_wait_ms,
        registered_addr: status.peer_registration.map(|r| r.addr.to_string()).unwrap_or_default(),
        registered_at_unix_ms: status.peer_registration.map_or(0, |r| r.registered_at_unix_ms),
        ..Default::default()
    }
}

impl hp_control::server::Controlled for Router {
    fn service(&self) -> &'static str {
        "hp-router"
    }

    fn status(&self) -> proto::Status {
        let mut peers = vec![proto::PeerStatus {
            addresses: vec![self.vps_address.to_string()],
            ..peer_status("VPS".into(), "vps", self.vps.status())
        }];
        let phones = self.phones.read().unwrap();
        let mut list: Vec<&PhoneEntry> = phones.values().collect();
        list.sort_by_key(|e| e.phone.client_id);
        for e in list {
            peers.push(proto::PeerStatus {
                addresses: e.address.iter().map(|a| a.to_string()).collect(),
                pending: matches!(e.origin, Origin::Pending(_)),
                pending_expires_unix: if let Origin::Pending(t) = e.origin { t } else { 0 },
                removable: e.origin != Origin::Settings,
                ..peer_status(peer_name(&e.phone.peer_id), "phone", e.link.status())
            });
        }
        let (lan_to_vps, ordered, vps_to_lan, lan_dropped) = self.bridge.stats().snapshot();
        let (to_vps, to_vps_dropped) = self.stats.to_vps.snapshot();
        let (to_phones, to_phones_dropped) = self.stats.to_phones.snapshot();
        proto::Status {
            service: "hp-router".into(),
            uptime_s: self.started.elapsed().as_secs(),
            mode: "router".into(),
            peers,
            traffic: Some(proto::Traffic {
                to_peers: to_phones.into(),
                ordered: ordered.into(),
                from_peers: to_vps.into(),
                dropped: u64::from(lan_dropped) + u64::from(to_vps_dropped) + u64::from(to_phones_dropped),
                lan_to_vps: lan_to_vps.into(),
                vps_to_lan: vps_to_lan.into(),
            }),
            pairing_supported: self.discovery.is_some(),
            ..Default::default()
        }
    }

    async fn create_pairing(&self) -> Result<proto::Pairing> {
        let discovery = self.discovery.clone().context("сопряжение недоступно: не заданы STUN_ADDR, MQTT_ADDR, MQTT_CA")?;
        let _guard = self.pairing_lock.lock().await;
        let expires = now_unix() + PAIRING_TTL.as_secs();
        let reused = self.phones.write().unwrap().values_mut().find(|e| matches!(e.origin, Origin::Pending(_))).map(|e| {
            e.origin = Origin::Pending(expires);
            e.phone
        });
        let phone = match reused {
            Some(phone) => phone,
            None => {
                let client_id = {
                    let phones = self.phones.read().unwrap();
                    (1..=u8::MAX).find(|id| !phones.contains_key(id)).context("все 255 номеров телефонов заняты")?
                };
                let phone = Phone { client_id, my_id: Uuid::new_v4(), peer_id: Uuid::new_v4() };
                self.add_phone(phone, Origin::Pending(expires)).await?;
                phone
            }
        };
        log::info!("сопряжение: ждём телефон {} ({}) до {expires} (unix)", phone.client_id, peer_name(&phone.peer_id));
        let peer = Peer { my_id: phone.my_id, peer_id: phone.peer_id };
        Ok(pairing(peer, &discovery.stun_addrs, discovery.mqtt_addr, discovery.mqtt_ca_pem, expires))
    }

    fn remove_peer(&self, name: &str) -> Result<()> {
        let removed = {
            let mut phones = self.phones.write().unwrap();
            let client_id = phones.iter().find(|(_, e)| peer_name(&e.phone.peer_id) == name).map(|(id, _)| *id).with_context(|| format!("нет телефона {name}"))?;
            anyhow::ensure!(phones[&client_id].origin != Origin::Settings, "телефон {name} задан в настройках (PHONE_<n>_*) — удалите его там");
            phones.remove(&client_id)
        };
        drop(removed);
        log::info!("телефон {name} удалён");
        self.save_paired()
    }
}

async fn run(config: Config) -> Result<()> {
    log::info!("hp-router: VPS {} (я {}), телефонов в настройках {}", config.vps_server, peer_name(&config.vps_my_id), config.phones.len());
    let mut routes = match config.routes {
        true => Some(routes::Routes::start(&config.tun_name, bypass_hosts(&config), vps_probe(&config)?).await?),
        false => None,
    };
    let bind_ifindex = routes.as_ref().map(|r| r.uplink.ifindex);
    // Пакеты телефонов роутер перекладывает насквозь: порядок им вернёт VPS или сам телефон.
    let vps_options = MultiLinkOptions {
        reorder_wait: config.reorder_wait,
        data_holes: config.data_holes,
        reorder_clients: false,
        bind_ifindex,
        pool: config.pool,
        hole_age: config.hole_age,
        ..MultiLinkOptions::default()
    };
    let (vps, mut vps_rx) = MultiLink::start_discovery(
        "vps",
        Discovery::VpsClient { server: config.vps_server },
        config.vps_my_id,
        config.vps_peer_id,
        vps_options,
    )
    .await?;
    let vps = Arc::new(vps);

    // Адрес в туннеле выдаёт VPS; TUN поднимаем, когда он получен.
    let control = vps.take_control().expect("приёмник служебных сообщений забираем один раз");
    let (assigned, vps_control) = hp_tun::bridge::request_address(&vps, control, &mut vps_rx, AddressKind::Host).await;
    let tun_config = hp_tun::TunConfig {
        name: config.tun_name.clone(),
        address: Some((assigned.address, assigned.prefix)),
        mtu: Some(config.tun_mtu),
        up: true,
    };
    let tun = hp_tun::Tun::create(&tun_config).context("создание TUN (нужны права root)")?;
    log::info!("hp-router: TUN {} {}/{}", tun.name(), assigned.address, assigned.prefix);
    if let Some(routes) = &mut routes {
        routes.tunnel_up().await?;
    }
    let hooks = routes::Hooks { up: config.on_tun_up.clone(), down: config.on_tun_down.clone(), env: hook_env_for(&config, tun.name(), &assigned, routes.as_ref()) };
    let _refresh = AbortOnDrop(hp_tun::bridge::spawn_address_refresh(vps.clone(), AddressKind::Host, None));
    let (to_phones_tx, to_phones_rx) = mpsc::channel::<Incoming>(256);
    let bridge = hp_tun::bridge::Bridge::start_relay(tun, vps.clone(), vps_rx, to_phones_tx);
    hooks.up().await;

    let router = Arc::new(Router {
        vps: vps.clone(),
        vps_address: assigned.address,
        phones: Arc::default(),
        discovery: config.phone_discovery.clone(),
        phone_options: MultiLinkOptions {
            reorder_wait: Duration::ZERO,
            data_holes: config.data_holes,
            bind_ifindex,
            pool: config.pool,
            hole_age: config.hole_age,
            ..MultiLinkOptions::default()
        },
        stats: Arc::new(RelayStats::default()),
        bridge,
        phones_file: config.phones_file.clone(),
        started: Instant::now(),
        pairing_lock: tokio::sync::Mutex::new(()),
    });
    for phone in &config.phones {
        router.add_phone(*phone, Origin::Settings).await?;
    }
    router.add_paired_from_file().await?;
    tokio::spawn(vps_to_phones(to_phones_rx, router.phones.clone(), router.stats.clone()));
    tokio::spawn(vps_answers_to_phones(vps_control, router.phones.clone()));

    if let Some(addr) = config.control {
        hp_control::load_or_create_key(&config.key_file)?;
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("управление: не удалось занять {addr}"))?;
        log::info!("управление: {addr}; строка подключения — hp-router --connection-string (или LuCI)");
        tokio::spawn(hp_control::server::serve(listener, config.key_file.clone(), router.clone()));
    }

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_log = Instant::now();
    let mut last_routes = Instant::now();
    let mut last = None;
    // procd останавливает службу SIGTERM: сначала снимаем туннель и зовём on-tun-down.sh.
    let mut shutdown = hp_tun::platform::NativeShutdown::new()?;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            signal = shutdown.wait() => {
                log::info!("hp-router: остановка ({signal})");
                return shutdown_tunnel(routes.as_mut(), &hooks, Ok(())).await;
            }
        }
        router.housekeeping();
        if let Some(routes) = &mut routes
            && last_routes.elapsed() >= ROUTES_INTERVAL
        {
            last_routes = Instant::now();
            if let Err(e) = routes.check().await {
                return shutdown_tunnel(Some(routes), &hooks, Err(e)).await;
            }
        }
        if last_log.elapsed() < STATUS_INTERVAL {
            continue;
        }
        last_log = Instant::now();
        let now = (vps.live_count(), router.bridge.stats().snapshot(), router.stats.to_vps.snapshot(), router.stats.to_phones.snapshot(), router.live_counts());
        if Some(&now) != last.as_ref() {
            let (to, ordered, from, dropped) = now.1;
            let holes: Vec<String> = now.4.iter().map(|(id, n)| format!("{id}:{n}")).collect();
            log::info!(
                "VPS: дыры {}/{TARGET_LINKS}, из TUN {to} (TCP с номером {ordered}), в TUN {from}, потеряно {dropped}; \
                 телефоны (дыры {}): к VPS {}/{} потеряно, к телефонам {}/{} потеряно",
                now.0,
                if holes.is_empty() { "нет".to_string() } else { holes.join(" ") },
                now.2 .0,
                now.2 .1,
                now.3 .0,
                now.3 .1,
            );
            last = Some(now);
        }
    }
}

/// Задача, которая останавливается вместе с хэндлом.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Запрос адреса от телефона — на VPS с номером телефона (адреса раздаёт VPS, роутер — нет).
async fn phone_requests_to_vps(client_id: u8, mut control: mpsc::Receiver<Control>, vps: Arc<MultiLink>) {
    while let Some(message) = control.recv().await {
        if let Control::AddressRequest(mut request) = message {
            request.client_id = Some(u32::from(client_id));
            let _ = vps.send_control(Control::AddressRequest(request)).await;
        }
    }
}

/// Ответы VPS на адреса телефонов — телефону по `client_id` (без номера: телефон его не знает).
async fn vps_answers_to_phones(mut control: mpsc::Receiver<Control>, phones: Phones) {
    while let Some(message) = control.recv().await {
        let Control::AddressAssign(mut assign) = message else { continue };
        let Some(client_id) = assign.client_id.take().and_then(|c| u8::try_from(c).ok()) else { continue };
        let link = {
            let mut phones = phones.write().unwrap();
            phones.get_mut(&client_id).map(|entry| {
                if let Ok(octets) = <[u8; 4]>::try_from(assign.address.as_slice()) {
                    entry.address = Some(Ipv4Addr::from(octets));
                }
                entry.link.clone()
            })
        };
        match link {
            Some(phone) => {
                let _ = phone.send_control(Control::AddressAssign(assign)).await;
            }
            None => log::debug!("VPS выдал адрес неизвестному телефону {client_id}"),
        }
    }
}

/// Телефон → VPS: пакет как пришёл, с номерами телефона (`Ordered`) или без (`Data`).
async fn phone_to_vps(client_id: u8, mut rx: mpsc::Receiver<Incoming>, vps: Arc<MultiLink>, stats: Arc<RelayStats>) {
    use std::sync::atomic::Ordering::Relaxed;
    while let Some(packet) = rx.recv().await {
        if packet.wrapped.is_some() {
            stats.to_vps.dropped.fetch_add(1, Relaxed);
            continue;
        }
        match vps.send_client(client_id, packet.order, &packet.payload).await {
            Ok(_) => stats.to_vps.forwarded.fetch_add(1, Relaxed),
            Err(e) => {
                log::trace!("телефон {client_id} -> VPS: {e:#}");
                stats.to_vps.dropped.fetch_add(1, Relaxed)
            }
        };
    }
    log::debug!("телефон {client_id}: канал входящих закрыт");
}

/// VPS → телефон по `client_id`: номера VPS сохраняются (`Ordered`), телефон вернёт порядок сам.
async fn vps_to_phones(mut rx: mpsc::Receiver<Incoming>, phones: Phones, stats: Arc<RelayStats>) {
    use std::sync::atomic::Ordering::Relaxed;
    while let Some(packet) = rx.recv().await {
        let Some(client_id) = packet.wrapped.map(|w| w.client_id) else { continue };
        let Some(phone) = link_of(&phones, client_id) else {
            log::debug!("VPS прислал пакет неизвестному телефону {client_id}");
            stats.to_phones.dropped.fetch_add(1, Relaxed);
            continue;
        };
        let sent = match packet.order {
            Some((flow, seq)) => phone.send_ordered(flow, seq, &packet.payload).await,
            None => phone.send_data(&packet.payload).await,
        };
        match sent {
            Ok(_) => stats.to_phones.forwarded.fetch_add(1, Relaxed),
            Err(e) => {
                log::trace!("VPS -> телефон {client_id}: {e:#}");
                stats.to_phones.dropped.fetch_add(1, Relaxed)
            }
        };
    }
    log::warn!("ретрансляция телефонам остановлена");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn getter(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn phones_are_numbered_consecutively() {
        let (a, b, c, d) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let phones = parse_phones(&getter(&[
            ("PHONE_1_MY_ID", &a.to_string()),
            ("PHONE_1_PEER_ID", &b.to_string()),
            ("PHONE_2_MY_ID", &c.to_string()),
            ("PHONE_2_PEER_ID", &d.to_string()),
            ("PHONE_4_MY_ID", &a.to_string()),
        ]))
        .unwrap();
        assert_eq!(
            phones,
            vec![Phone { client_id: 1, my_id: a, peer_id: b }, Phone { client_id: 2, my_id: c, peer_id: d }],
            "после пропуска номера (3) дальше не читаем"
        );
        assert!(parse_phones(&getter(&[])).unwrap().is_empty(), "без телефонов — только шлюз");
    }

    #[test]
    fn control_only_on_a_lan_address() {
        assert_eq!(control_addr(&getter(&[])).unwrap(), None, "по умолчанию выключено");
        assert_eq!(control_addr(&getter(&[("CONTROL_ADDR", "off")])).unwrap(), None);
        assert_eq!(control_addr(&getter(&[("CONTROL_ADDR", "192.168.1.1:47001")])).unwrap(), Some("192.168.1.1:47001".parse().unwrap()));
        assert!(control_addr(&getter(&[("CONTROL_ADDR", "0.0.0.0:47001")])).is_err(), "не на всех интерфейсах");
    }

    #[test]
    fn connection_string_flags() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(args(&["--config", "/etc/hp-router/router.env", "--connection-string"])).unwrap(), (Some("/etc/hp-router/router.env".into()), Action::ConnectionString));
        assert_eq!(parse_args(args(&["--config", "r.env", "--new-connection-string"])).unwrap().1, Action::NewConnectionString);
        assert!(parse_args(args(&["--bogus"])).is_err());
    }

    #[test]
    fn half_configured_or_repeated_phones_are_rejected() {
        let (a, b) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
        assert!(parse_phones(&getter(&[("PHONE_1_MY_ID", &a)])).is_err());
        let repeated = getter(&[("PHONE_1_MY_ID", &a), ("PHONE_1_PEER_ID", &b), ("PHONE_2_MY_ID", &a), ("PHONE_2_PEER_ID", &b)]);
        assert!(parse_phones(&repeated).is_err(), "один телефон дважды");
    }
}
