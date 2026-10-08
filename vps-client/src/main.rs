//! `vps-client`: клиент `vps-server` с белым IP. Обычная схема «клиент — сервер»: адрес
//! сервера известен заранее, ни STUN, ни MQTT, ни пробива. Поднимает TUN и 10 дыр к серверу:
//! IP-пакеты по дырам как есть (TCP с номером в потоке, сервер возвращает порядок). Маршруты
//! туннеля ставит сам (`hp_tun::routes`), DNS и прочее системное — скрипты хуков.
//!
//! Аргументы: `<ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера>` (порт знакомства по
//! умолчанию 40000). Адрес в туннеле выдаёт сервер. Окружение: `TUN_NAME` (`hp0`), `TUN_MTU`
//! (1400), `REORDER_WAIT_MS` (начальное ожидание буфера порядка, 8; 0 — выключить),
//! `DATA_HOLES` (0 — данные через все живые дыры), `ROUTES` (`auto` — маршруты туннеля; `off` —
//! не трогать маршруты), `ON_TUN_UP` / `ON_TUN_DOWN` (скрипты хуков, по умолчанию
//! `/etc/vps-client/on-tun-up.sh` и `on-tun-down.sh`, если файлов нет — ничего не делается),
//! `RUNTIME=multi`, `RUST_LOG`, `LOG_TARGET=syslog` (OpenWrt). Нужны права root (Windows — администратор
//! и `wintun.dll`, см. README; хуки по умолчанию — `HookLauncher::default_scripts`).
//!
//! Переменные хуков — см. `hp_tun::routes::Hooks`; дополнительно `DNS` — адреса DNS от сервера
//! через запятую (`8.8.8.8,77.8.8.8`).

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use connection::multilink::{Discovery, MultiLink, MultiLinkOptions, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use connection::proto::AddressKind;
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
    anyhow::ensure!(args.len() == 3, "использование: vps-client <ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера>");
    let server = connection::vps::parse_server(&args[0]).context("адрес сервера: ожидается ip или ip:порт")?;
    let my_id: Uuid = args[1].parse().context("мой GUID")?;
    let server_id: Uuid = args[2].parse().context("GUID сервера")?;
    let options = MultiLinkOptions {
        reorder_wait: Duration::from_millis(env_number("REORDER_WAIT_MS", DEFAULT_REORDER_WAIT.as_millis() as u64)?),
        data_holes: env_number("DATA_HOLES", 0u8)?,
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
    let bridge = hp_tun::bridge::Bridge::start(tun, link.clone(), incoming);
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
