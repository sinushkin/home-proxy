//! Клиент с локальным мостом для WireGuard: то же, что делает приложение на телефоне
//! (`hp-client`), только на обычном Linux. Поднимает набор дыр к серверу и слушает
//! `127.0.0.1:<порт>`: этот адрес указывается как Endpoint у WireGuard.
//!
//! Аргументы: `<stun_addr[,stun2_addr]> <mqtt_addr> <mqtt_ca> <my_peer_id> <peer_id> [порт]`
//! (порт по умолчанию 51821). Необязательные переменные окружения:
//! `REORDER_WAIT_MS` (по умолчанию 8, 0 — не восстанавливать порядок пакетов),
//! `DATA_HOLES` (0 — данные через все живые дыры, 1 — через одну).

use std::net::SocketAddr;

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
        "использование: client <stun[,stun2]> <mqtt> <mqtt_ca> <my_peer_id> <peer_id> [порт]"
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
    let client = Client::start(config).await?;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        log::info!("{}", client.status());
    }
}
