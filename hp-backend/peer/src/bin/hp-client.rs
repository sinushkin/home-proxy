//! `hp-client`: клиент с локальным мостом для WireGuard (P2P, через STUN и MQTT): то же, что делает приложение на телефоне
//! (`hp-client`), только на обычном Linux. Поднимает набор дыр к серверу и слушает
//! `127.0.0.1:<порт>`: этот адрес указывается как Endpoint у WireGuard.
//!
//! Аргументы: `<stun_addr[,stun2_addr]> <mqtt_addr> <mqtt_ca> <my_peer_id> <peer_id> [порт]`
//! (порт по умолчанию 51821). Необязательные переменные окружения:
//! `REORDER_WAIT_MS` (по умолчанию 8, 0 — не восстанавливать порядок пакетов),
//! `DATA_HOLES` (0 — данные через все живые дыры, 1 — через одну).
//!
//! `TUN_ADDR=10.80.1.7/32` — режим без WireGuard, как у телефона: IP-пакеты из TUN по дырам как
//! есть (TCP с номером в потоке), порт моста тогда не нужен (`TUN_NAME` — `hp1`, `TUN_MTU` — 1400).
//! Маршруты в TUN настраиваются отдельно.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use hp_client::{Client, ClientConfig};
use uuid::Uuid;

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match std::env::var(name) {
        Ok(value) => value.trim().parse().map_err(|_| anyhow::anyhow!("{name}: некорректное число")),
        Err(_) => Ok(default),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    hp_logging::init()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() == 5 || args.len() == 6,
        "использование: hp-client <stun[,stun2]> <mqtt> <mqtt_ca> <my_peer_id> <peer_id> [порт]"
    );
    let mqtt_ca = std::fs::read(&args[2]).with_context(|| format!("не удалось прочитать CA {}", args[2]))?;
    let config = ClientConfig {
        stun_addrs: hp_client::parse_stun_servers(&args[0]).context("STUN")?,
        mqtt_addr: args[1].parse::<SocketAddr>().context("MQTT: ожидается ip:порт")?,
        mqtt_ca_pem: mqtt_ca,
        my_id: args[3].parse::<Uuid>().context("мой GUID некорректен")?,
        peer_id: args[4].parse::<Uuid>().context("GUID пира некорректен")?,
        local_port: match args.get(5) {
            Some(port) => port.parse().context("порт")?,
            None => 51821,
        },
        reorder_wait_ms: env_number("REORDER_WAIT_MS", hp_client::DEFAULT_REORDER_WAIT.as_millis() as u32)?,
        data_holes: env_number("DATA_HOLES", 0u8)?,
    };
    log::info!("порядок пакетов: ожидание {} мс, дыр для данных: {}", config.reorder_wait_ms, config.data_holes);
    if let Ok(tun_addr) = std::env::var("TUN_ADDR") {
        return run_tun(&tun_addr, config).await;
    }
    let client = Client::start(config).await?;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        log::info!("{}", client.status());
    }
}

/// Режим TUN: набор дыр к серверу (роутеру или ПК) и мост TUN ↔ дыры.
async fn run_tun(tun_addr: &str, config: ClientConfig) -> Result<()> {
    use connection::multilink::{MultiLink, MultiLinkOptions, TARGET_LINKS};
    let tun_config = hp_tun::TunConfig {
        name: std::env::var("TUN_NAME").unwrap_or_else(|_| "hp1".into()),
        address: Some(hp_tun::parse_cidr(tun_addr).context("TUN_ADDR: ожидается ip/префикс")?),
        mtu: Some(env_number("TUN_MTU", 1400u16)?),
        up: true,
    };
    let tun = hp_tun::Tun::create(&tun_config).context("создание TUN (нужны права root)")?;
    log::info!("hp-client: TUN {} {tun_addr}, сервер {}", tun.name(), config.peer_id);
    let options = MultiLinkOptions {
        reorder_wait: std::time::Duration::from_millis(u64::from(config.reorder_wait_ms)),
        data_holes: config.data_holes,
        ..MultiLinkOptions::default()
    };
    let (link, incoming) = MultiLink::start_with(
        "",
        config.stun_addrs,
        config.mqtt_addr,
        config.mqtt_ca_pem,
        config.my_id,
        config.peer_id,
        options,
    )
    .await?;
    let link = Arc::new(link);
    let bridge = hp_tun::bridge::Bridge::start(tun, link.clone(), incoming);
    let mut last = None;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let now = (link.live_count(), bridge.stats().snapshot());
        if Some(now) != last {
            let (to, ordered, from, dropped) = now.1;
            log::info!("дыры {}/{TARGET_LINKS}, к серверу {to} (TCP с номером {ordered}), от сервера {from}, потеряно {dropped}", now.0);
            last = Some(now);
        }
    }
}
