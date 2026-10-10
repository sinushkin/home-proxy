//! `vps-client`: клиент `vps-server` с белым IP. Обычная схема «клиент — сервер»: адрес
//! сервера известен заранее, ни STUN, ни MQTT, ни пробива. Поднимает TUN и динамический набор дыр к серверу (4..10, дыры стареют и заменяются):
//! IP-пакеты по дырам как есть (TCP с номером в потоке, сервер возвращает порядок). Маршруты
//! туннеля ставит сам (`hp_tun::routes`), DNS и прочее системное — скрипты хуков.
//!
//! Аргументы: `<ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера>` (порт знакомства по
//! умолчанию 40000). Адрес в туннеле выдаёт сервер. Окружение: `TUN_NAME` (`hp0`), `TUN_MTU`
//! (1400), `REORDER_WAIT_MS` (начальное ожидание буфера порядка, 8; 0 — выключить),
//! `DATA_HOLES` (0 — данные через все живые дыры), набор дыр динамический: `HOLES_MIN` (4 — не
//! меньше стольких в работе), `HOLES_MAX` (10), `HOLE_AGE` (`180-600` по умолчанию — срок жизни дыры в секундах,
//! случайный в этих пределах), `ROUTES` (`auto` — маршруты туннеля; `off` —
//! не трогать маршруты), `ON_TUN_UP` / `ON_TUN_DOWN` (скрипты хуков, по умолчанию
//! `/etc/vps-client/on-tun-up.sh` и `on-tun-down.sh`, если файлов нет — ничего не делается),
//! `RUNTIME=multi`, `RUST_LOG`, `LOG_TARGET=syslog` (OpenWrt). Нужны права root (Windows — администратор
//! и `wintun.dll`, см. README; хуки по умолчанию — `HookLauncher::default_scripts`).
//!
//! Управление (для `hpctl` и трея, как у `hp-router`): `CONTROL_ADDR` (`127.0.0.1:47001` или адрес
//! LAN; по умолчанию выключено, `0.0.0.0` нельзя), `CONTROL_KEY_FILE` (по умолчанию `control.key`
//! рядом с хуками). `vps-client --connection-string` печатает строку подключения для трея,
//! `--new-connection-string` меняет ключ (прежние строки перестают работать).
//!
//! Переменные хуков — см. `hp_tun::routes::Hooks`; дополнительно `DNS` — адреса DNS от сервера
//! через запятую (`8.8.8.8,77.8.8.8`).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use connection::holes::PoolPolicy;
use connection::discovery::Discovery;
use connection::multilink::{ConnState, LinkStatus, MultiLink, MultiLinkOptions, DEFAULT_HOLE_AGE, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use connection::proto::AddressKind;
use hp_control::proto;
use hp_tun::platform::{HookLauncher, NativeHooks, NativeShutdown, NativeTun};
use hp_tun::routes::{hook_env, Hooks, Routes};
use uuid::Uuid;

const STATUS_INTERVAL: Duration = Duration::from_secs(30);
/// Как часто сверяем маршруты (снесённые ставим заново, сменился аплинк — выходим).
const ROUTES_CHECK: Duration = Duration::from_secs(10);

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match std::env::var(name) {
        Ok(value) => value.trim().parse().map_err(|_| anyhow::anyhow!("{name}: некорректное число")),
        Err(_) => Ok(default),
    }
}

/// Диапазон секунд `мин-макс` (например `60-180`) или одно число.
fn env_age_range(name: &str, default: (Duration, Duration)) -> Result<(Duration, Duration)> {
    let Ok(value) = std::env::var(name) else { return Ok(default) };
    let bad = || anyhow::anyhow!("{name}: ожидается «мин-макс» в секундах, например 60-180");
    let (min, max) = value.trim().split_once('-').unwrap_or((value.trim(), value.trim()));
    let (min, max): (u64, u64) = (min.trim().parse().map_err(|_| bad())?, max.trim().parse().map_err(|_| bad())?);
    anyhow::ensure!(min >= 1 && min <= max, bad());
    Ok((Duration::from_secs(min), Duration::from_secs(max)))
}

/// `[--config путь] --connection-string | --new-connection-string` → (файл настроек, новый ключ?).
/// Страница LuCI зовёт именно так: у неё нет окружения службы, настройки берутся из файла.
fn parse_connection_args(args: &[String]) -> Option<(Option<PathBuf>, bool)> {
    match args {
        [flag] => connection_flag(flag).map(|new_key| (None, new_key)),
        [config, path, flag] if config == "--config" => connection_flag(flag).map(|new_key| (Some(PathBuf::from(path)), new_key)),
        _ => None,
    }
}

fn connection_flag(flag: &str) -> Option<bool> {
    match flag {
        "--connection-string" => Some(false),
        "--new-connection-string" => Some(true),
        _ => None,
    }
}

/// Настройки `KEY=VALUE` (как у службы: `vps-client.conf`); пустые строки, `#` и кавычки пропускаются.
fn parse_conf(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().trim_matches(|c| c == '"' || c == '\'').to_string()))
        .collect()
}

/// `CONTROL_ADDR` и файл ключа по настройкам `get` (файл настроек или окружение).
/// Ключ — `CONTROL_KEY_FILE`, иначе `control.key` рядом с хуками службы; относительный путь —
/// от каталога файла настроек.
fn control_settings(get: impl Fn(&str) -> Option<String>, conf_dir: Option<&std::path::Path>) -> Result<(Option<SocketAddr>, PathBuf)> {
    let addr = hp_control::parse_control_addr(get("CONTROL_ADDR").as_deref())?;
    let key_file = match get("CONTROL_KEY_FILE").map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
        Some(path) => match conf_dir {
            Some(dir) => dir.join(path),
            None => PathBuf::from(path),
        },
        None => {
            let (up, _) = NativeHooks::default_scripts();
            up.parent().map(PathBuf::from).unwrap_or_default().join(hp_control::KEY_FILE)
        }
    };
    Ok((addr, key_file))
}

/// `CONTROL_ADDR` и файл ключа из окружения (так их видит запущенная служба).
fn control_addr() -> Result<Option<SocketAddr>> {
    Ok(control_settings(|name| std::env::var(name).ok(), None)?.0)
}

fn control_key_file() -> PathBuf {
    control_settings(|name| std::env::var(name).ok(), None).map(|(_, key_file)| key_file).unwrap_or_default()
}

/// `vps-client [--config файл] --connection-string` / `--new-connection-string`: строка подключения
/// трея к этой службе. Без `--config` настройки берутся из окружения.
fn print_connection_string(config: Option<&std::path::Path>, new_key: bool) -> Result<()> {
    let (addr, key_file) = match config {
        Some(path) => {
            let text = std::fs::read_to_string(path).with_context(|| format!("не удалось прочитать {}", path.display()))?;
            let conf = parse_conf(&text);
            control_settings(|name| conf.get(name).cloned(), path.parent())?
        }
        None => (control_addr()?, control_key_file()),
    };
    let addr = addr.context("управление выключено: задайте CONTROL_ADDR (адрес LAN роутера или 127.0.0.1) в настройках службы")?;
    let key = if new_key { hp_control::replace_key(&key_file)? } else { hp_control::load_or_create_key(&key_file)? };
    println!("{}", hp_control::connection_string(addr, &key));
    Ok(())
}

/// Состояние службы для `hpctl` и трея: один «пир» — VPS-сервер.
struct Control {
    link: Arc<MultiLink>,
    bridge: Arc<hp_tun::bridge::Bridge>,
    started: Instant,
    server_name: String,
    address: Ipv4Addr,
}

fn peer_status(name: String, status: LinkStatus) -> proto::PeerStatus {
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
        kind: "vps".into(),
        reorder_wait_ms: status.reorder_wait_ms,
        ..Default::default()
    }
}

impl hp_control::server::Controlled for Control {
    fn service(&self) -> &'static str {
        "vps-client"
    }

    fn status(&self) -> proto::Status {
        let (to_peers, ordered, from_peers, dropped) = self.bridge.stats().snapshot();
        proto::Status {
            service: "vps-client".into(),
            uptime_s: self.started.elapsed().as_secs(),
            mode: "vps-client".into(),
            peers: vec![proto::PeerStatus { addresses: vec![self.address.to_string()], ..peer_status(self.server_name.clone(), self.link.status()) }],
            traffic: Some(proto::Traffic { to_peers: u64::from(to_peers), ordered: u64::from(ordered), from_peers: u64::from(from_peers), dropped: u64::from(dropped), ..Default::default() }),
            pairing_supported: false,
            ..Default::default()
        }
    }

    async fn create_pairing(&self) -> Result<proto::Pairing> {
        anyhow::bail!("vps-client не выпускает пары: сопряжение телефонов делает hp-server или hp-router")
    }

    fn remove_peer(&self, _name: &str) -> Result<()> {
        anyhow::bail!("vps-client — клиент одного сервера, удалять нечего")
    }
}

fn env_path(name: &str, default: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| default.into()))
}

/// Однопоточный tokio по умолчанию: на одноядерном роутере многопоточный тратит процессор на
/// пробуждения потоков (`futex` на каждый пакет). `RUNTIME=multi` — многопоточный.
fn main() -> Result<()> {
    let runtime = if std::env::var("RUNTIME").as_deref() == Ok("multi") {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    } else {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?
    };
    runtime.block_on(run())
}

async fn run() -> Result<()> {
    hp_logging::init()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some((config, new_key)) = parse_connection_args(&args) {
        return print_connection_string(config.as_deref(), new_key);
    }
    anyhow::ensure!(args.len() == 3, "использование: vps-client <ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера>");
    let server = connection::vps::parse_server(&args[0]).context("адрес сервера: ожидается ip или ip:порт")?;
    let my_id: Uuid = args[1].parse().context("мой GUID")?;
    let server_id: Uuid = args[2].parse().context("GUID сервера")?;
    let pool = PoolPolicy {
        min_active: env_number("HOLES_MIN", PoolPolicy::default().min_active)?,
        max_total: env_number("HOLES_MAX", PoolPolicy::default().max_total)?,
        ..PoolPolicy::default()
    };
    anyhow::ensure!(
        pool.min_active >= 1 && pool.min_active <= pool.max_total && pool.max_total <= TARGET_LINKS as usize,
        "HOLES_MIN должно быть от 1 до HOLES_MAX, а HOLES_MAX — не больше {TARGET_LINKS}"
    );
    let options = MultiLinkOptions {
        reorder_wait: Duration::from_millis(env_number("REORDER_WAIT_MS", DEFAULT_REORDER_WAIT.as_millis() as u64)?),
        data_holes: env_number("DATA_HOLES", 0u8)?,
        pool,
        hole_age: env_age_range("HOLE_AGE", DEFAULT_HOLE_AGE)?,
        ..MultiLinkOptions::default()
    };
    let (link, mut incoming) =
        MultiLink::start_discovery("", Discovery::VpsClient { server }, my_id, server_id, options).await?;
    let link = Arc::new(link);
    let control = link.take_control().expect("приёмник служебных сообщений забираем один раз");
    let (assigned, _control) = hp_tun::bridge::request_address(&link, control, &mut incoming, AddressKind::Host).await;
    let tun_name = std::env::var("TUN_NAME").unwrap_or_else(|_| "hp0".into());
    let config = hp_tun::TunConfig {
        name: tun_name.clone(),
        address: Some((assigned.address, assigned.prefix)),
        mtu: Some(env_number("TUN_MTU", 1400u16)?),
        up: true,
    };
    let tun = NativeTun::create(&config).context("создание TUN (нужны права root)")?;
    let tun_name = tun.name().to_string();
    log::info!("vps-client: сервер {server}, TUN {tun_name} {}/{}", assigned.address, assigned.prefix);

    let IpAddr::V4(server_v4) = server.ip() else { anyhow::bail!("нужен IPv4-адрес сервера") };
    // Маршруты: сначала обход (адрес сервера через аплинк), потом весь трафик — в туннель.
    let mut routes = match std::env::var("ROUTES").as_deref() {
        Ok("off") => None,
        _ => Some(Routes::start(&tun_name, vec![server_v4], server_v4).await?),
    };
    if let Some(routes) = routes.as_mut() {
        routes.tunnel_up().await?;
    }
    let (default_up, default_down) = NativeHooks::default_scripts();
    let (default_up, default_down) = (default_up.to_string_lossy().into_owned(), default_down.to_string_lossy().into_owned());
    let hooks = Hooks {
        up: env_path("ON_TUN_UP", &default_up),
        down: env_path("ON_TUN_DOWN", &default_down),
        env: hook_env(&tun_name, (assigned.address, assigned.prefix), &assigned.dns, server_v4, &[server_v4], routes.as_ref()),
    };
    hooks.up().await;

    let _refresh = hp_tun::bridge::spawn_address_refresh(link.clone(), AddressKind::Host, None);
    let bridge = Arc::new(hp_tun::bridge::Bridge::start(tun, link.clone(), incoming));
    if let Some(addr) = control_addr()? {
        let key_file = control_key_file();
        hp_control::load_or_create_key(&key_file)?;
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("управление: не удалось занять {addr}"))?;
        log::info!("управление: {addr}; строка подключения — vps-client --connection-string");
        let controlled = Arc::new(Control {
            link: link.clone(),
            bridge: bridge.clone(),
            started: Instant::now(),
            server_name: connection::auth::peer_name(&server_id),
            address: assigned.address,
        });
        tokio::spawn(hp_control::server::serve(listener, key_file, controlled));
    }
    let mut last = None;
    let mut status = tokio::time::interval(STATUS_INTERVAL);
    let mut check = tokio::time::interval(ROUTES_CHECK);
    let mut shutdown = NativeShutdown::new()?;
    loop {
        tokio::select! {
            _ = status.tick() => {
                let now = (link.live_count(), bridge.stats().snapshot());
                if Some(now) != last {
                    let (to, ordered, from, dropped) = now.1;
                    log::info!("дыры {}/{TARGET_LINKS}, к серверу {to} (TCP с номером {ordered}), от сервера {from}, потеряно {dropped}", now.0);
                    last = Some(now);
                }
            }
            _ = check.tick(), if routes.is_some() => {
                if let Some(routes) = routes.as_mut() {
                    // Ошибка — аплинк сменился: выходим, procd перезапустит и сокеты привяжутся заново.
                    routes.check().await?;
                }
            }
            signal = shutdown.wait() => {
                log::info!("vps-client: остановка ({signal}), снимаю маршруты");
                if let Some(routes) = routes.as_mut() {
                    routes.tunnel_down().await;
                }
                hooks.down().await;
                return Ok(());
            }
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
    fn connection_string_flags_with_and_without_a_config() {
        assert_eq!(parse_connection_args(&args(&["--connection-string"])), Some((None, false)));
        assert_eq!(parse_connection_args(&args(&["--new-connection-string"])), Some((None, true)));
        assert_eq!(
            parse_connection_args(&args(&["--config", "/etc/vps-client/vps-client.conf", "--new-connection-string"])),
            Some((Some("/etc/vps-client/vps-client.conf".into()), true))
        );
        // Обычный запуск службы: адрес, свой и чужой GUID.
        assert_eq!(parse_connection_args(&args(&["203.0.113.10:40600", "a", "b"])), None);
        assert_eq!(parse_connection_args(&args(&["--config", "x", "--other"])), None);
    }

    #[test]
    fn conf_is_key_value_with_comments_and_quotes() {
        let conf = parse_conf(
            r#"# служба
VPS_SERVER=203.0.113.10:40600

CONTROL_ADDR = "192.168.1.1:47001"
RUST_LOG='info'
"#,
        );
        assert_eq!(conf.get("CONTROL_ADDR").map(String::as_str), Some("192.168.1.1:47001"));
        assert_eq!(conf.get("RUST_LOG").map(String::as_str), Some("info"));
        assert_eq!(conf.len(), 3);
    }

    #[test]
    fn control_settings_come_from_the_config_and_the_key_path_is_relative_to_it() {
        let conf = parse_conf("CONTROL_ADDR=192.168.1.1:47001
CONTROL_KEY_FILE=keys/control.key
");
        let (addr, key) = control_settings(|n| conf.get(n).cloned(), Some(std::path::Path::new("/etc/vps-client"))).unwrap();
        assert_eq!(addr, Some("192.168.1.1:47001".parse().unwrap()));
        assert_eq!(key, PathBuf::from("/etc/vps-client/keys/control.key"));
        // Управление выключено: адреса нет, ключ — по умолчанию рядом с хуками.
        let (addr, key) = control_settings(|_| None, None).unwrap();
        assert_eq!(addr, None);
        assert!(key.ends_with(hp_control::KEY_FILE));
        // 0.0.0.0 нельзя.
        assert!(control_settings(|n| (n == "CONTROL_ADDR").then(|| "0.0.0.0:47001".to_string()), None).is_err());
    }
}
