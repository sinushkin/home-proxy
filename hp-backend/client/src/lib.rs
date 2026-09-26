//! Клиент телефона: набор из 10 дыр к роутеру плюс локальный UDP-мост для
//! WireGuard. Это ядро библиотеки для Android (`../android-lib`, приложение — `../../android-vpn`), отдельно от JNI,
//! чтобы его можно было проверять на обычном хосте.

pub mod bridge;

pub use connection::multilink::DEFAULT_REORDER_WAIT;
pub use connection::stun::parse_servers as parse_stun_servers;

use std::fmt;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use bridge::Bridge;
use connection::multilink::{Discovery, MultiLink, MultiLinkOptions, TARGET_LINKS};
use connection::relay::PlainOut;
use std::sync::Arc;
use tokio::net::UdpSocket;
use uuid::Uuid;

/// Настройки клиента.
#[derive(Clone)]
pub struct ClientConfig {
    /// Один или несколько STUN-серверов.
    pub stun_addrs: Vec<SocketAddr>,
    pub mqtt_addr: SocketAddr,
    /// PEM с CA-сертификатом MQTT-брокера.
    pub mqtt_ca_pem: Vec<u8>,
    /// GUID телефона.
    pub my_id: Uuid,
    /// GUID роутера, которого ищем (набор дыр «к этому телефону»).
    pub peer_id: Uuid,
    /// Порт локального моста на 127.0.0.1 (0 — любой свободный).
    pub local_port: u16,
    /// Сколько миллисекунд ждать недостающий пакет WireGuard при восстановлении порядка
    /// (0 — не восстанавливать).
    pub reorder_wait_ms: u32,
    /// Через сколько дыр слать данные (0 — через все живые).
    pub data_holes: u8,
}

/// Снимок состояния для показа пользователю.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub live_holes: usize,
    pub local_addr: SocketAddr,
    pub to_router: u32,
    pub from_router: u32,
    pub dropped: u32,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "дыры {}/{TARGET_LINKS}, WireGuard -> {}, в роутер {}, от роутера {}, потеряно {}",
            self.live_holes, self.local_addr, self.to_router, self.from_router, self.dropped
        )
    }
}

/// Запущенный клиент. Должен создаваться внутри tokio-рантайма; задачи
/// `MultiLink` живут, пока жив рантайм (остановка — остановкой рантайма).
pub struct Client {
    multilink: Arc<MultiLink>,
    bridge: Bridge,
}

impl Client {
    pub async fn start(config: ClientConfig) -> Result<Self> {
        let discovery = Discovery::StunMqtt {
            stun_addrs: config.stun_addrs,
            mqtt_addr: config.mqtt_addr,
            mqtt_ca_pem: config.mqtt_ca_pem,
        };
        let options = options(config.reorder_wait_ms, config.data_holes);
        Self::start_discovery(discovery, config.my_id, config.peer_id, config.local_port, options).await
    }

    async fn start_discovery(
        discovery: Discovery,
        my_id: Uuid,
        peer_id: Uuid,
        local_port: u16,
        options: MultiLinkOptions,
    ) -> Result<Self> {
        // Порт занимаем первым: ошибку «адрес занят» лучше получить до сети.
        let socket = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], local_port)))
            .await
            .with_context(|| format!("не удалось занять локальный порт {local_port}"))?;
        let (multilink, incoming) = MultiLink::start_discovery("", discovery, my_id, peer_id, options).await?;
        let multilink = Arc::new(multilink);
        let bridge = Bridge::start(socket, PlainOut(multilink.clone()), incoming)?;
        log::info!("клиент запущен: мост для WireGuard на {}", bridge.local_addr());
        Ok(Self { multilink, bridge })
    }

    pub fn status(&self) -> Status {
        let (to_router, from_router, dropped) = self.bridge.stats().snapshot();
        Status {
            live_holes: self.multilink.live_count(),
            local_addr: self.bridge.local_addr(),
            to_router,
            from_router,
            dropped,
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.bridge.local_addr()
    }
}

fn options(reorder_wait_ms: u32, data_holes: u8) -> MultiLinkOptions {
    MultiLinkOptions {
        reorder_wait: std::time::Duration::from_millis(u64::from(reorder_wait_ms)),
        data_holes,
        local_port_base: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_is_readable() {
        let status = Status {
            live_holes: 7,
            local_addr: "127.0.0.1:51821".parse().unwrap(),
            to_router: 12,
            from_router: 10,
            dropped: 1,
        };
        assert_eq!(
            status.to_string(),
            "дыры 7/10, WireGuard -> 127.0.0.1:51821, в роутер 12, от роутера 10, потеряно 1"
        );
    }
}
