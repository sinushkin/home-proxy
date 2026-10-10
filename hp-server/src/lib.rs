//! Сервер: принимает IP-пакеты клиентов по дырам и пишет их в TUN, ответы из TUN — обратно по
//! дырам (`hp_tun::hub`); в интернет пакеты выходят через NAT подсети туннеля на этой машине
//! (настраивается отдельно). Пиров может быть несколько (у каждого свой набор из 10 дыр): телефоны
//! напрямую (`Ordered`/`Data`) или роутер OpenWrt (пакеты его телефонов — `WrappedData` с
//! `client_id`). Адреса в туннеле раздаёт сервер (`addresses`): клиент просит адрес
//! (`AddressRequest`), сервер выдаёт постоянный для этого клиента.
//!
//! Настройки — переменные окружения и/или файл `KEY=VALUE` (`--config server.env`):
//!   STUN_ADDR, MQTT_ADDR, MQTT_CA — как у `peer` (STUN_ADDR — один или несколько
//!   серверов через запятую: `ip:порт,ip2:порт`);
//!   MY_ID / PEER_ID — GUID сервера и GUID пира (телефона или роутера); несколько пиров —
//!   PEER_<n>_MY_ID / PEER_<n>_PEER_ID, n = 1, 2, … подряд (GUID сервера у каждого свой: брокер
//!   различает соединения по нему); можно не задавать вовсе и добавлять телефоны из трея;
//!   ADDRESS_FILE — где хранить выданные адреса (`addresses.state` рядом с файлом настроек);
//!   PEERS_FILE — телефоны, сопряжённые через трей (`peers.state` рядом с настройками);
//!   CONTROL_ADDR — адрес протокола управления для трея (`127.0.0.1:47001`; `off` — выключить),
//!   ключ канала — в CONTROL_KEY_FILE (`control.key` рядом с настройками, создаётся сам);
//!   DNS — DNS для клиентов через запятую (по умолчанию — резолверы этой машины из resolv.conf;
//!   в конце всегда 8.8.8.8, 1.1.1.1);
//!   MODE — `tun` (интерфейс TUN + NAT подсети на машине; Linux, нужен root) или `netstack` (свой
//!   сетевой стек процесса: соединения телефона открывает сам `hp-server` обычными сокетами —
//!   без TUN, NAT и прав; на Windows только так); по умолчанию `tun`, на Windows `netstack`;
//!   TUN_ADDR — адрес сервера в туннеле и подсеть клиентов (`10.80.0.1/16`, по умолчанию так),
//!   TUN_NAME (`hp0`), TUN_MTU (1400);
//!   BIND_ADDR — к какому адаптеру привязать сокеты дыр и STUN, чтобы они шли мимо VPN этой
//!   машины, с настоящего домашнего адреса (соединения телефона — по-прежнему через VPN): `auto`
//!   (по умолчанию; сравнить STUN по маршруту по умолчанию и через каждый адаптер, `bypass`),
//!   IP-адрес адаптера или `off`;
//!   REORDER_WAIT_MS — сколько мс ждать недостающий TCP-пакет при восстановлении порядка (8;
//!   0 — выключить); DATA_HOLES — через сколько дыр слать данные (0 — через все живые);
//!   HOLES_MIN (4), HOLES_MAX (10), HOLE_AGE (`180-600`, секунды) — динамический набор дыр: не
//!   меньше стольких в работе, не больше стольких всего, дыры стареют и заменяются.
//!
//! Логи: `RUST_LOG` (по умолчанию `info`), `LOG_TARGET=syslog` — в syslog,
//! `LOG_FILE=путь` — в файл.
//!
//! Библиотечная часть (`settings`, `Common`, `serve`) переиспользуется `vps-server` и
//! `hp-router`. Запуск: `hp-server [--config server.env]` (без `--config` берётся
//! `server.env` рядом с бинарником, если есть, иначе только окружение). Linux и Windows.
//! `hp-server [--config …] --connection-string` печатает строку подключения для трея,
//! `--new-connection-string` — меняет ключ (прежние строки перестают работать) и печатает новую.

pub mod addresses;
pub mod bypass;
pub mod service;
pub mod settings;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use connection::holes::PoolPolicy;
use connection::discovery::Discovery;
use connection::multilink::{MultiLinkOptions, DEFAULT_HOLE_AGE, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use tokio::sync::mpsc;
use settings::Settings;
use uuid::Uuid;

use addresses::AddressBook;
use service::Service;

const STATUS_INTERVAL: Duration = Duration::from_secs(30);

/// Адрес сервера в туннеле по умолчанию: подсеть `/16` покрывает роутер (`10.80.0.2`) и
/// телефоны (`10.80.1.<n>`).
pub const DEFAULT_TUN_ADDR: &str = "10.80.0.1/16";

/// Пара GUID: сервер и пир (у каждого пира свой набор дыр).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    pub my_id: Uuid,
    pub peer_id: Uuid,
}

/// Что стоит за мостом: интерфейс TUN ядра или свой сетевой стек процесса.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Tun,
    Netstack,
}

impl Mode {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim) {
            None | Some("") => Ok(if cfg!(windows) { Mode::Netstack } else { Mode::Tun }),
            Some("tun") => Ok(Mode::Tun),
            Some("netstack") => Ok(Mode::Netstack),
            Some(other) => anyhow::bail!("MODE: «{other}» — ожидается tun или netstack"),
        }
    }
}

/// Настройки, общие для `hp-server` и `vps-server`.
pub struct Common {
    pub peers: Vec<Peer>,
    pub mode: Mode,
    /// К какому адаптеру привязать сокеты дыр и STUN (мимо VPN этой машины).
    pub bind: bypass::BindSetting,
    pub tun: hp_tun::TunConfig,
    /// Где хранить выданные адреса (`None` — только в памяти).
    pub address_file: Option<PathBuf>,
    /// DNS, которые сервер отдаёт клиентам вместе с адресом.
    pub dns: Vec<std::net::Ipv4Addr>,
    pub reorder_wait: Duration,
    pub data_holes: u8,
    /// Динамический набор дыр: сколько держать и как долго живёт дыра (`HOLES_MIN`, `HOLES_MAX`, `HOLE_AGE`).
    pub pool: PoolPolicy,
    pub hole_age: (Duration, Duration),
    /// Адрес протокола управления (трей); `None` — выключен.
    pub control: Option<std::net::SocketAddr>,
    pub key_file: PathBuf,
    /// Где хранить сопряжённые через трей телефоны.
    pub peers_file: Option<PathBuf>,
    /// Способ встречи для телефонов из `peers.state` и новых сопряжений (STUN + MQTT); `None` —
    /// сопряжение не поддерживается (VPS).
    pub pairing: Option<Discovery>,
    /// hp-stats (фича `stats`, PLAN-ML.md): настройки сбора статистики пакетов вниз; `None` —
    /// `STATS_FILE` не задан, сбор выключен (даже если бинарь собран с фичей).
    #[cfg(feature = "stats")]
    pub stats: Option<StatsSettings>,
}

/// Настройки сбора статистики пакетов (hp-stats, `STATS_*`), см. PLAN-ML.md §5.
#[cfg(feature = "stats")]
#[derive(Debug, Clone)]
pub struct StatsSettings {
    pub out_dir: PathBuf,
    pub loss_timeout: Duration,
    pub max_file_bytes: u64,
    pub channel_capacity: usize,
}

impl Common {
    pub fn from_settings(settings: &Settings) -> Result<Self> {
        Self::build(settings, true)
    }

    /// Настройки без пиров из `MY_ID`/`PEER_ID`/`PEER_<n>_*`: пиры придут из другого источника
    /// (файл клиентов `vps-server`).
    pub fn from_settings_without_peers(settings: &Settings) -> Result<Self> {
        Self::build(settings, false)
    }

    fn build(settings: &Settings, with_peers: bool) -> Result<Self> {
        let get = |name: &str| settings.get(name);
        let tun_addr = get("TUN_ADDR").unwrap_or_else(|| DEFAULT_TUN_ADDR.to_string());
        Ok(Self {
            peers: if with_peers { parse_peers(&get)? } else { Vec::new() },
            mode: Mode::parse(get("MODE").as_deref())?,
            bind: bypass::BindSetting::parse(get("BIND_ADDR").as_deref())?,
            address_file: Some(settings.resolve(get("ADDRESS_FILE").as_deref().unwrap_or("addresses.state").trim())),
            dns: addresses::client_dns(get("DNS").as_deref())?,
            tun: hp_tun::TunConfig {
                name: get("TUN_NAME").unwrap_or_else(|| "hp0".into()),
                address: Some(hp_tun::parse_cidr(&tun_addr).context("TUN_ADDR: ожидается ip/префикс")?),
                mtu: Some(match get("TUN_MTU") {
                    Some(value) => value.trim().parse().context("TUN_MTU: ожидается число")?,
                    None => 1400,
                }),
                up: true,
            },
            reorder_wait: match get("REORDER_WAIT_MS") {
                Some(value) => Duration::from_millis(value.trim().parse().context("REORDER_WAIT_MS: ожидается число миллисекунд")?),
                None => DEFAULT_REORDER_WAIT,
            },
            data_holes: match get("DATA_HOLES") {
                Some(value) => value.trim().parse().context("DATA_HOLES: ожидается число дыр 0..=255")?,
                None => 0,
            },
            pool: holes_settings(&get)?.0,
            hole_age: holes_settings(&get)?.1,
            control: match get("CONTROL_ADDR").as_deref().map(str::trim) {
                Some("off") | Some("") => None,
                Some(addr) => Some(addr.parse().context("CONTROL_ADDR: ожидается ip:порт или off")?),
                None => Some(hp_control::DEFAULT_ADDR.parse().expect("адрес по умолчанию")),
            },
            key_file: settings.resolve(get("CONTROL_KEY_FILE").as_deref().unwrap_or(hp_control::KEY_FILE).trim()),
            peers_file: Some(settings.resolve(get("PEERS_FILE").as_deref().unwrap_or("peers.state").trim())),
            pairing: None,
            #[cfg(feature = "stats")]
            stats: match get("STATS_FILE").as_deref().map(str::trim) {
                None | Some("") => None,
                Some(dir) => Some(StatsSettings {
                    out_dir: settings.resolve(dir),
                    loss_timeout: match get("STATS_LOSS_MS") {
                        Some(v) => Duration::from_millis(v.trim().parse().context("STATS_LOSS_MS: ожидается число миллисекунд")?),
                        None => Duration::from_secs(3),
                    },
                    max_file_bytes: match get("STATS_MAX_MB") {
                        Some(v) => v.trim().parse::<u64>().context("STATS_MAX_MB: ожидается число мегабайт")? * 1024 * 1024,
                        None => 64 * 1024 * 1024,
                    },
                    channel_capacity: match get("STATS_CHAN_CAP") {
                        Some(v) => v.trim().parse().context("STATS_CHAN_CAP: ожидается число")?,
                        None => 4096,
                    },
                }),
            },
        })
    }
}

/// Динамический набор дыр из настроек: `HOLES_MIN` (4 — не меньше стольких в работе), `HOLES_MAX`
/// (10), `HOLE_AGE` (`180-600` по умолчанию — срок жизни дыры в секундах, случайный в этих пределах).
pub fn holes_settings(get: &impl Fn(&str) -> Option<String>) -> Result<(PoolPolicy, (Duration, Duration))> {
    let number = |name: &str, default: usize| -> Result<usize> {
        get(name).map_or(Ok(default), |v| v.trim().parse().with_context(|| format!("{name}: ожидается число")))
    };
    let defaults = PoolPolicy::default();
    let pool = PoolPolicy { min_active: number("HOLES_MIN", defaults.min_active)?, max_total: number("HOLES_MAX", defaults.max_total)?, ..defaults };
    anyhow::ensure!(
        pool.min_active >= 1 && pool.min_active <= pool.max_total && pool.max_total <= TARGET_LINKS as usize,
        "HOLES_MIN должно быть от 1 до HOLES_MAX, а HOLES_MAX — не больше {TARGET_LINKS}"
    );
    let age = match get("HOLE_AGE") {
        None => DEFAULT_HOLE_AGE,
        Some(value) => {
            let bad = || anyhow::anyhow!("HOLE_AGE: ожидается «мин-макс» в секундах, например 60-180");
            let value = value.trim();
            let (min, max) = value.split_once('-').unwrap_or((value, value));
            let (min, max): (u64, u64) = (min.trim().parse().map_err(|_| bad())?, max.trim().parse().map_err(|_| bad())?);
            anyhow::ensure!(min >= 1 && min <= max, bad());
            (Duration::from_secs(min), Duration::from_secs(max))
        }
    };
    Ok((pool, age))
}

/// Пиры: `PEER_<n>_MY_ID` / `PEER_<n>_PEER_ID` (n = 1, 2, … подряд) или один — `MY_ID` / `PEER_ID`.
fn parse_peers(get: &impl Fn(&str) -> Option<String>) -> Result<Vec<Peer>> {
    let parse = |name: &str, value: String| -> Result<Uuid> { value.trim().parse().with_context(|| format!("{name}: некорректный GUID")) };
    let mut peers = Vec::new();
    for n in 1.. {
        let (my, peer) = (format!("PEER_{n}_MY_ID"), format!("PEER_{n}_PEER_ID"));
        match (get(&my), get(&peer)) {
            (None, None) => break,
            (Some(a), Some(b)) => peers.push(Peer { my_id: parse(&my, a)?, peer_id: parse(&peer, b)? }),
            _ => anyhow::bail!("пир {n}: нужны обе переменные {my} и {peer}"),
        }
    }
    if peers.is_empty() {
        match (get("MY_ID"), get("PEER_ID")) {
            (Some(my), Some(peer)) => peers.push(Peer { my_id: parse("MY_ID", my)?, peer_id: parse("PEER_ID", peer)? }),
            // Пиров в настройках может не быть: телефоны добавляются из трея.
            (None, None) => {}
            _ => anyhow::bail!("нужны обе переменные MY_ID и PEER_ID"),
        }
    }
    let mut mine = std::collections::HashSet::new();
    for peer in &peers {
        anyhow::ensure!(mine.insert(peer.my_id), "GUID сервера {} повторяется: у каждого пира он должен быть свой", peer.my_id);
    }
    Ok(peers)
}

/// Логи по настройкам: `LOG_FILE` считается от каталога файла настроек.
pub fn init_logging(settings: &Settings) -> Result<()> {
    hp_logging::init_with(|name| match name {
        "LOG_FILE" => settings.get(name).map(|path| settings.resolve(&path).to_string_lossy().into_owned()),
        _ => settings.get(name),
    })?;
    Ok(())
}

/// Обычный (P2P) режим: встреча через STUN и MQTT.
fn p2p_discovery(settings: &Settings) -> Result<Discovery> {
    let need = |name: &str| settings.get(name).with_context(|| format!("не задана переменная {name}"));
    let mqtt_ca = settings.resolve(&need("MQTT_CA")?);
    Ok(Discovery::StunMqtt {
        stun_addrs: connection::stun::parse_servers(&need("STUN_ADDR")?).context("STUN_ADDR")?,
        mqtt_addr: need("MQTT_ADDR")?.trim().parse().context("MQTT_ADDR: ожидается ip:порт")?,
        mqtt_ca_pem: std::fs::read(&mqtt_ca)
            .with_context(|| format!("не удалось прочитать CA-сертификат {}", mqtt_ca.display()))?,
    })
}

/// Что сделать вместо запуска службы.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Run,
    /// Напечатать строку подключения трея (ключ создаётся, если его нет).
    ConnectionString,
    /// Сменить ключ и напечатать новую строку.
    NewConnectionString,
}

/// `[--config файл] [--connection-string | --new-connection-string]`.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<(Option<PathBuf>, Action)> {
    let (mut config, mut action) = (None, Action::Run);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = Some(args.next().context("--config: нужен путь")?.into()),
            "--connection-string" => action = Action::ConnectionString,
            "--new-connection-string" => action = Action::NewConnectionString,
            _ => anyhow::bail!("использование: hp-server [--config server.env] [--connection-string | --new-connection-string]"),
        }
    }
    Ok((config, action))
}

/// Строка подключения трея к службе с этими настройками (`--connection-string`).
pub fn print_connection_string(common: &Common, action: Action) -> Result<()> {
    let addr = common.control.context("управление выключено (CONTROL_ADDR=off)")?;
    let key = match action {
        Action::NewConnectionString => hp_control::replace_key(&common.key_file)?,
        _ => hp_control::load_or_create_key(&common.key_file)?,
    };
    println!("{}", hp_control::connection_string(addr, &key));
    Ok(())
}

/// Файл настроек по умолчанию: `server.env` рядом с бинарником, если он есть.
pub fn default_config() -> Option<PathBuf> {
    let path = std::env::current_exe().ok()?.parent()?.join("server.env");
    path.is_file().then_some(path)
}

/// Точка входа `hp-server`.
pub fn main() -> Result<()> {
    let (config, action) = parse_args(std::env::args().skip(1))?;
    let settings = Settings::load(config.or_else(default_config).as_deref())?;
    if action != Action::Run {
        return print_connection_string(&Common::from_settings(&settings)?, action);
    }
    init_logging(&settings)?;
    let mut common = Common::from_settings(&settings)?;
    let discovery = p2p_discovery(&settings)?;
    let discoveries = vec![discovery.clone(); common.peers.len()];
    common.pairing = Some(discovery);
    tokio::runtime::Runtime::new()?.block_on(serve(discoveries, common))
}

/// Изменение набора пиров во время работы (файл клиентов `vps-server`).
pub enum PeerChange {
    Add(Peer, Discovery),
    Remove(Uuid),
}

/// Сервер на TUN: поднимает интерфейс и по набору дыр на каждого пира (`discoveries[i]` — способ
/// встречи с `common.peers[i]`), раздаёт адреса и гоняет мост TUN ↔ дыры, пока жив процесс.
pub async fn serve(discoveries: Vec<Discovery>, common: Common) -> Result<()> {
    serve_with_changes(discoveries, common, None).await
}

/// То же, что `serve`, плюс канал изменений пиров: пиры добавляются и удаляются без перезапуска.
pub async fn serve_with_changes(discoveries: Vec<Discovery>, common: Common, changes: Option<mpsc::Receiver<PeerChange>>) -> Result<()> {
    anyhow::ensure!(discoveries.len() == common.peers.len(), "способов встречи {} на {} пиров", discoveries.len(), common.peers.len());
    let (server_addr, prefix) = common.tun.address.context("у сервера нет адреса в туннеле (TUN_ADDR)")?;
    // Адаптер мимо VPN ищем до того, как появится свой TUN: его адрес не кандидат.
    let stun_addrs = common
        .pairing
        .iter()
        .chain(&discoveries)
        .find_map(|d| match d {
            Discovery::StunMqtt { stun_addrs, .. } => Some(stun_addrs.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let binding = bypass::resolve(common.bind, &stun_addrs, common.tun.address).await;
    match common.mode {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Mode::Tun => {
            let tun = hp_tun::Tun::create(&common.tun).context("создание TUN (нужны права root)")?;
            log::info!("сервер: TUN {} {server_addr}/{prefix}", tun.name());
            run(hp_tun::hub::Hub::start(tun), discoveries, common, binding, changes).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        Mode::Tun => anyhow::bail!("MODE=tun есть только на Linux; здесь — MODE=netstack"),
        Mode::Netstack => {
            #[cfg(feature = "netstack")]
            {
                let (device, stack_end) = hp_tun::device::channel_pair();
                let stack = hp_netstack::start(stack_end);
                log::info!("сервер: свой сетевой стек, адрес {server_addr}/{prefix} (соединения телефонов открывает этот процесс)");
                let result = run(hp_tun::hub::Hub::start(device), discoveries, common, binding, changes).await;
                stack.abort();
                result
            }
            #[cfg(not(feature = "netstack"))]
            anyhow::bail!("сборка без фичи netstack")
        }
    }
}

/// Раздаёт адреса, поднимает наборы дыр к пирам и гоняет мост, пока жив процесс.
async fn run<D: hp_tun::device::PacketDevice>(
    hub: hp_tun::hub::Hub<D>,
    discoveries: Vec<Discovery>,
    common: Common,
    binding: bypass::Binding,
    changes: Option<mpsc::Receiver<PeerChange>>,
) -> Result<()> {
    let (server_addr, prefix) = common.tun.address.context("у сервера нет адреса в туннеле")?;
    let book = AddressBook::load(server_addr, prefix, common.address_file.clone())?;
    log::info!(
        "сервер: порядок пакетов: ожидание {} мс, дыр для данных: {}, сокеты дыр: {}",
        common.reorder_wait.as_millis(),
        if common.data_holes == 0 { "все".to_string() } else { common.data_holes.to_string() },
        binding.ip.map(|ip| ip.to_string()).unwrap_or_else(|| "любой адрес".into())
    );
    log::info!("сервер: DNS для клиентов {:?}", common.dns);
    let options = MultiLinkOptions {
        reorder_wait: common.reorder_wait,
        data_holes: common.data_holes,
        bind_ip: binding.ip,
        bind_ifindex: binding.ifindex,
        pool: common.pool,
        hole_age: common.hole_age,
        ..MultiLinkOptions::default()
    };
    // hp-stats (фича `stats`, PLAN-ML.md): сборщик запускается один раз на процесс; `_stats_task`
    // держим живым до конца `run()` (сам `run()` не возвращается, пока жив процесс).
    #[cfg(feature = "stats")]
    let (stats_handle, _stats_task) = match &common.stats {
        Some(s) => {
            let (handle, task) = hp_stats::spawn(hp_stats::CollectorConfig {
                out_dir: s.out_dir.clone(),
                channel_capacity: s.channel_capacity,
                loss_timeout: s.loss_timeout,
                max_file_bytes: s.max_file_bytes,
                ..hp_stats::CollectorConfig::default()
            })
            .with_context(|| format!("hp-stats: не удалось открыть каталог {}", s.out_dir.display()))?;
            log::info!("hp-stats: сбор статистики пакетов включён, CSV в {}", s.out_dir.display());
            (Some(handle), Some(task))
        }
        None => (None, None),
    };
    let service = Arc::new(Service::new(
        hub,
        book,
        common.dns.clone(),
        options,
        common.pairing.clone(),
        common.peers_file.clone(),
        common.mode,
        binding.ip,
        #[cfg(feature = "stats")]
        stats_handle,
    ));
    for (peer, discovery) in common.peers.iter().zip(discoveries) {
        service.add_configured(*peer, discovery).await?;
    }
    service.add_paired_from_file().await?;
    let dynamic = changes.is_some();
    if let Some(mut changes) = changes {
        let service = service.clone();
        tokio::spawn(async move {
            while let Some(change) = changes.recv().await {
                let result = match change {
                    PeerChange::Add(peer, discovery) => service.add_from_file(peer, discovery).await,
                    PeerChange::Remove(peer_id) => service.remove_peer(&connection::auth::peer_name(&peer_id)),
                };
                if let Err(e) = result {
                    log::warn!("клиенты: {e:#}");
                }
            }
        });
    }

    match common.control {
        Some(addr) => {
            hp_control::load_or_create_key(&common.key_file)?;
            let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("управление: не удалось занять {addr}"))?;
            log::info!("управление: {addr}; строка подключения для трея — hp-server --connection-string");
            tokio::spawn(hp_control::server::serve(listener, common.key_file.clone(), service.clone()));
        }
        None if !dynamic && service.live_counts().is_empty() => anyhow::bail!("нет ни одного пира (MY_ID/PEER_ID) и выключено управление (CONTROL_ADDR)"),
        None => {}
    }

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_log = std::time::Instant::now();
    let mut last = None;
    loop {
        tick.tick().await;
        service.housekeeping();
        if last_log.elapsed() < STATUS_INTERVAL {
            continue;
        }
        last_log = std::time::Instant::now();
        let now = (service.live_counts(), service.hub().stats().snapshot());
        if Some(&now) != last.as_ref() {
            let (to, ordered, from, dropped) = now.1;
            let holes: Vec<String> = now.0.iter().map(|n| format!("{n}/{TARGET_LINKS}")).collect();
            log::info!("дыры {}, к пирам {to} (TCP с номером {ordered}), от пиров {from}, потеряно {dropped}", holes.join(" "));
            last = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn config_argument_is_optional() {
        assert_eq!(parse_args(args(&[])).unwrap(), (None, Action::Run));
        assert_eq!(parse_args(args(&["--config", "/etc/hp/server.env"])).unwrap(), (Some(PathBuf::from("/etc/hp/server.env")), Action::Run));
        assert_eq!(parse_args(args(&["--connection-string"])).unwrap(), (None, Action::ConnectionString));
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["install"])).is_err());
    }

    fn getter(vars: &[(&str, String)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: std::collections::HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn one_peer_or_a_numbered_list() {
        let (a, b, c, d) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let single = parse_peers(&getter(&[("MY_ID", a.to_string()), ("PEER_ID", b.to_string())])).unwrap();
        assert_eq!(single, vec![Peer { my_id: a, peer_id: b }]);
        let list = parse_peers(&getter(&[
            ("PEER_1_MY_ID", a.to_string()),
            ("PEER_1_PEER_ID", b.to_string()),
            ("PEER_2_MY_ID", c.to_string()),
            ("PEER_2_PEER_ID", d.to_string()),
        ]))
        .unwrap();
        assert_eq!(list, vec![Peer { my_id: a, peer_id: b }, Peer { my_id: c, peer_id: d }]);
        let repeated = getter(&[("PEER_1_MY_ID", a.to_string()), ("PEER_1_PEER_ID", b.to_string()), ("PEER_2_MY_ID", a.to_string()), ("PEER_2_PEER_ID", d.to_string())]);
        assert!(parse_peers(&repeated).is_err(), "GUID сервера у пиров должен различаться");
        assert!(parse_peers(&getter(&[])).unwrap().is_empty(), "без пиров — телефоны из трея");
        assert!(parse_peers(&getter(&[("MY_ID", a.to_string())])).is_err());
    }
}
