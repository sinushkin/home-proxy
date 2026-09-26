//! `vps-client`: клиент `vps-server` с белым IP. Обычная схема «клиент — сервер»: адрес
//! сервера известен заранее, ни STUN, ни MQTT, ни пробива. Поднимает TUN и 10 дыр к серверу:
//! IP-пакеты по дырам как есть (TCP с номером в потоке, сервер возвращает порядок). На роутере
//! OpenWrt вместо него — `hp-router` (то же плюс телефоны).
//!
//! Аргументы: `<ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера>` (порт знакомства по
//! умолчанию 40000). Окружение: `TUN_ADDR` (`10.80.0.2/24`), `TUN_NAME` (`hp0`), `TUN_MTU`
//! (1400), `REORDER_WAIT_MS` (начальное ожидание буфера порядка, 8; 0 — выключить),
//! `DATA_HOLES` (0 — данные через все живые дыры), `RUNTIME=multi` (многопоточный tokio; по
//! умолчанию однопоточный), `RUST_LOG`, `LOG_TARGET=syslog` (OpenWrt). Нужны права root.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use connection::multilink::{Discovery, MultiLink, MultiLinkOptions, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use uuid::Uuid;

const STATUS_INTERVAL: Duration = Duration::from_secs(30);

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match std::env::var(name) {
        Ok(value) => value.trim().parse().map_err(|_| anyhow::anyhow!("{name}: некорректное число")),
        Err(_) => Ok(default),
    }
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
    let tun_addr = std::env::var("TUN_ADDR").unwrap_or_else(|_| "10.80.0.2/24".into());
    let config = hp_tun::TunConfig {
        name: std::env::var("TUN_NAME").unwrap_or_else(|_| "hp0".into()),
        address: Some(hp_tun::parse_cidr(&tun_addr).context("TUN_ADDR: ожидается ip/префикс")?),
        mtu: Some(env_number("TUN_MTU", 1400u16)?),
        up: true,
    };
    let tun = hp_tun::Tun::create(&config).context("создание TUN (нужны права root)")?;
    log::info!("vps-client: сервер {server}, TUN {} {tun_addr}", tun.name());
    let (link, incoming) =
        MultiLink::start_discovery("", Discovery::VpsClient { server }, my_id, server_id, options).await?;
    let link = Arc::new(link);
    let bridge = hp_tun::bridge::Bridge::start(tun, link.clone(), incoming);
    let mut last = None;
    loop {
        tokio::time::sleep(STATUS_INTERVAL).await;
        let now = (link.live_count(), bridge.stats().snapshot());
        if Some(now) != last {
            let (to, ordered, from, dropped) = now.1;
            log::info!("дыры {}/{TARGET_LINKS}, к серверу {to} (TCP с номером {ordered}), от сервера {from}, потеряно {dropped}", now.0);
            last = Some(now);
        }
    }
}
