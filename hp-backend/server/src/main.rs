//! Сервер: принимает пакеты клиентов и передаёт их WireGuard'у, для каждого
//! клиента с отдельного локального UDP-порта (как `server-rs`). Пиром может быть
//! роутер (пакеты `WrappedData` с `client_id`, ответы уходят обёрнутыми с тем же
//! `client_id`) или сам телефон (обычная `Data`, ответ тоже обычной `Data`).
//! Набор из 10 дыр к пиру постоянный.
//!
//! Настройки — переменные окружения (удобно через `run.sh` и `.env`):
//!   STUN_ADDR, MQTT_ADDR, MQTT_CA — как у `peer` (STUN_ADDR — один или несколько
//!   серверов через запятую: `ip:порт,ip2:порт`);
//!   MY_ID / PEER_ID — GUID сервера и GUID пира: роутера (набор «server» роутера)
//!   либо телефона, если он подключается напрямую;
//!   WG_ADDR — адрес WireGuard (по умолчанию 127.0.0.1:51820);
//!   CLIENT_TIMEOUT_SECS — через сколько секунд тишины клиент удаляется (300).
//!
//! Логи: `RUST_LOG` (по умолчанию `info`), `LOG_TARGET=syslog` — в syslog.

mod bridge;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bridge::{Bridge, ClientKey, Reply};
use connection::multilink::{MultiLink, TARGET_LINKS};
use uuid::Uuid;

const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
const STATUS_INTERVAL: Duration = Duration::from_secs(60);

/// Ответы WireGuard'а уходят пиру по дырам: роутеру — обёрнутыми с номером
/// клиента, телефону напрямую — обычной `Data`.
struct ToPeer(Arc<MultiLink>);

impl Reply for ToPeer {
    async fn send(&self, client: ClientKey, payload: Vec<u8>) -> Result<()> {
        match client {
            ClientKey::Routed(client_id) => self.0.send_wrapped(client_id, payload).await.map(|_| ()),
            ClientKey::Direct => self.0.send_data(payload).await.map(|_| ()),
        }
    }
}

struct Config {
    stun_addrs: Vec<SocketAddr>,
    mqtt_addr: SocketAddr,
    mqtt_ca: String,
    my_id: Uuid,
    peer_id: Uuid,
    wg_addr: SocketAddr,
    client_timeout: Duration,
}

impl Config {
    fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let need = |name: &str| get(name).with_context(|| format!("не задана переменная {name}"));
        let client_timeout_secs: u64 = match get("CLIENT_TIMEOUT_SECS") {
            Some(value) => value.parse().context("CLIENT_TIMEOUT_SECS: ожидается число секунд")?,
            None => 300,
        };
        Ok(Self {
            stun_addrs: connection::stun::parse_servers(&need("STUN_ADDR")?).context("STUN_ADDR")?,
            mqtt_addr: need("MQTT_ADDR")?.parse().context("MQTT_ADDR: ожидается ip:порт")?,
            mqtt_ca: need("MQTT_CA")?,
            my_id: need("MY_ID")?.parse().context("MY_ID: некорректный GUID")?,
            peer_id: need("PEER_ID")?.parse().context("PEER_ID: некорректный GUID")?,
            wg_addr: get("WG_ADDR")
                .unwrap_or_else(|| "127.0.0.1:51820".to_string())
                .parse()
                .context("WG_ADDR: ожидается ip:порт")?,
            client_timeout: Duration::from_secs(client_timeout_secs),
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    hp_logging::init()?;
    let config = Config::from_env(|name| std::env::var(name).ok())?;
    let ca_pem = std::fs::read(&config.mqtt_ca)
        .with_context(|| format!("не удалось прочитать CA-сертификат {}", config.mqtt_ca))?;

    log::info!(
        "сервер: я {} ищу пира {} (роутер или телефон), WireGuard {}, клиент удаляется через {} с тишины",
        config.my_id,
        config.peer_id,
        config.wg_addr,
        config.client_timeout.as_secs()
    );

    let (link, mut incoming) = MultiLink::start(
        "",
        config.stun_addrs,
        config.mqtt_addr,
        ca_pem,
        config.my_id,
        config.peer_id,
    )
    .await?;
    let link = Arc::new(link);
    let bridge = Bridge::new(config.wg_addr, ToPeer(link.clone()), config.client_timeout);

    let cleanup_bridge = bridge.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
        loop {
            ticker.tick().await;
            cleanup_bridge.cleanup();
        }
    });

    let status_bridge = bridge.clone();
    let status_link = link.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(STATUS_INTERVAL);
        let mut last = (usize::MAX, usize::MAX);
        loop {
            ticker.tick().await;
            let now = (status_bridge.client_count(), status_link.live_count());
            if now != last {
                log::info!("клиентов: {}, дыр к роутеру: {}/{TARGET_LINKS}", now.0, now.1);
                last = now;
            }
        }
    });

    while let Some(packet) = incoming.recv().await {
        let client = match packet.wrapped {
            Some(info) => ClientKey::Routed(info.client_id),
            None => ClientKey::Direct,
        };
        if let Err(e) = bridge.send_to_wireguard(client, &packet.payload).await {
            log::warn!("{e:#}");
        }
    }
    log::warn!("канал входящих закрыт, выходим");
    Ok(())
}
