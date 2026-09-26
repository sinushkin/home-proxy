//! `hp-client`: то же, что приложение на телефоне, только на обычном Linux: набор дыр к роутеру
//! или ПК (P2P, через STUN и MQTT) и TUN без WireGuard — IP-пакеты по дырам как есть (TCP с
//! номером в потоке).
//!
//! Аргументы: `<stun_addr[,stun2_addr]> <mqtt_addr> <mqtt_ca> <my_peer_id> <peer_id>`.
//! Адрес в туннеле выдаёт сервер (ПК или VPS за роутером). Переменные окружения: `TUN_NAME`
//! (`hp1`), `TUN_MTU` (1400),
//! `REORDER_WAIT_MS` (по умолчанию 8, 0 — не восстанавливать порядок), `DATA_HOLES` (0 — данные
//! через все живые дыры). Маршруты в TUN настраиваются отдельно; нужны права root.

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
    anyhow::ensure!(args.len() == 5, "использование: hp-client <stun[,stun2]> <mqtt> <mqtt_ca> <my_peer_id> <peer_id>");
    let mqtt_ca = std::fs::read(&args[2]).with_context(|| format!("не удалось прочитать CA {}", args[2]))?;
    let config = ClientConfig {
        stun_addrs: hp_client::parse_stun_servers(&args[0]).context("STUN")?,
        mqtt_addr: args[1].parse::<SocketAddr>().context("MQTT: ожидается ip:порт")?,
        mqtt_ca_pem: mqtt_ca,
        my_id: args[3].parse::<Uuid>().context("мой GUID некорректен")?,
        peer_id: args[4].parse::<Uuid>().context("GUID пира некорректен")?,
        reorder_wait_ms: env_number("REORDER_WAIT_MS", hp_client::DEFAULT_REORDER_WAIT.as_millis() as u32)?,
        data_holes: env_number("DATA_HOLES", 0u8)?,
    };
    let name = std::env::var("TUN_NAME").unwrap_or_else(|_| "hp1".into());
    let mtu = env_number("TUN_MTU", 1400u16)?;
    let peer = config.peer_id;
    let client = Client::start(config).await?;
    let assigned = loop {
        if let Some(assigned) = client.address() {
            break assigned;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    let tun_config = hp_tun::TunConfig { name, address: Some((assigned.address, assigned.prefix)), mtu: Some(mtu), up: true };
    let tun = hp_tun::Tun::create(&tun_config).context("создание TUN (нужны права root)")?;
    log::info!("hp-client: TUN {} {}/{}, сервер {peer}", tun.name(), assigned.address, assigned.prefix);
    client.attach_tun(tun);
    let mut last = None;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        let status = client.status();
        if Some(&status) != last.as_ref() {
            log::info!("{status}");
            last = Some(status);
        }
    }
}
