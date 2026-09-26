//! `vps-client`: клиент `vps-server` с белым IP. Обычная схема «клиент — сервер»: адрес
//! сервера известен заранее, ни STUN, ни MQTT, ни пробива. Поднимает 10 дыр к серверу и
//! мост для WireGuard на `127.0.0.1:<порт>` (как `hp-client`): это Endpoint WireGuard.
//!
//! Аргументы: `<ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера> [порт моста]`
//! (порт знакомства по умолчанию 40000, моста — 51821). Окружение: `REORDER_WAIT_MS`
//! (начальное ожидание буфера порядка, 8; 0 — выключить), `DATA_HOLES` (0 — данные через
//! все живые дыры), `RUST_LOG`, `LOG_TARGET=syslog` (OpenWrt).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use connection::multilink::{Discovery, MultiLink, MultiLinkOptions, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use connection::relay::PlainOut;
use hp_client::bridge::Bridge;
use tokio::net::UdpSocket;
use uuid::Uuid;

const STATUS_INTERVAL: Duration = Duration::from_secs(30);

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match std::env::var(name) {
        Ok(value) => value.trim().parse().map_err(|_| anyhow::anyhow!("{name}: некорректное число")),
        Err(_) => Ok(default),
    }
}

/// `ip` или `ip:порт`; без порта — порт знакомства по умолчанию.
fn parse_server(value: &str) -> Result<SocketAddr> {
    match value.parse::<SocketAddr>() {
        Ok(addr) => Ok(addr),
        Err(_) => Ok(SocketAddr::new(
            value.parse().context("адрес сервера: ожидается ip или ip:порт")?,
            connection::vps::DEFAULT_BOOTSTRAP_PORT,
        )),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    hp_logging::init()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        args.len() == 3 || args.len() == 4,
        "использование: vps-client <ip_сервера[:порт_знакомства]> <мой_guid> <guid_сервера> [порт моста]"
    );
    let server = parse_server(&args[0])?;
    let my_id: Uuid = args[1].parse().context("мой GUID")?;
    let server_id: Uuid = args[2].parse().context("GUID сервера")?;
    let local_port: u16 = match args.get(3) {
        Some(port) => port.parse().context("порт моста")?,
        None => 51821,
    };
    let options = MultiLinkOptions {
        reorder_wait: Duration::from_millis(env_number("REORDER_WAIT_MS", DEFAULT_REORDER_WAIT.as_millis() as u64)?),
        data_holes: env_number("DATA_HOLES", 0u8)?,
        local_port_base: 0,
    };

    // Порт моста занимаем первым: ошибку «адрес занят» лучше получить до сети.
    let socket = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], local_port)))
        .await
        .with_context(|| format!("не удалось занять локальный порт {local_port}"))?;
    let (link, incoming) =
        MultiLink::start_discovery("", Discovery::VpsClient { server }, my_id, server_id, options).await?;
    let link = Arc::new(link);
    let bridge = Bridge::start(socket, PlainOut(link.clone()), incoming)?;
    log::info!("vps-client: сервер {server}, мост для WireGuard на {}", bridge.local_addr());

    let mut last = None;
    loop {
        tokio::time::sleep(STATUS_INTERVAL).await;
        let (to_server, from_server, dropped) = bridge.stats().snapshot();
        let now = (link.live_count(), to_server, from_server, dropped);
        if Some(now) != last {
            log::info!(
                "дыры {}/{TARGET_LINKS}, к серверу {to_server}, от сервера {from_server}, потеряно {dropped}",
                now.0
            );
            last = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_address_with_or_without_port() {
        assert_eq!(parse_server("203.0.113.10").unwrap(), "203.0.113.10:40000".parse().unwrap());
        assert_eq!(parse_server("203.0.113.10:41000").unwrap(), "203.0.113.10:41000".parse().unwrap());
        assert!(parse_server("vps").is_err());
    }
}
