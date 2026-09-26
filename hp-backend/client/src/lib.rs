//! Клиент телефона: набор из 10 дыр к роутеру (или ПК) и мост TUN ↔ дыры без WireGuard. Это
//! ядро библиотеки для Android (`../android-lib`, приложение — `../../android-vpn`), отдельно от
//! JNI, чтобы его можно было проверять на обычном хосте.
//!
//! Порядок: `Client::start` поднимает дыры и keep-alive; TUN (на Android — дескриптор от
//! `VpnService`) подключается позже, `attach_tun`, когда живёт хотя бы одна дыра, и отключается
//! `detach_tun`, не трогая дыр. Пока TUN не подключён, пришедшее от пира отбрасывается.

pub mod bridge;

pub use connection::multilink::DEFAULT_REORDER_WAIT;
pub use connection::stun::parse_servers as parse_stun_servers;

use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use connection::multilink::{Discovery, Incoming, MultiLink, MultiLinkOptions, TARGET_LINKS};
use tokio::sync::mpsc;
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
    /// GUID роутера или ПК, которого ищем (набор дыр «к этому телефону»).
    pub peer_id: Uuid,
    /// Сколько миллисекунд ждать недостающий TCP-пакет при восстановлении порядка
    /// (0 — не восстанавливать).
    pub reorder_wait_ms: u32,
    /// Через сколько дыр слать данные (0 — через все живые).
    pub data_holes: u8,
}

/// Снимок состояния для показа пользователю.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub live_holes: usize,
    pub tun: bool,
    pub to_peer: u32,
    pub from_peer: u32,
    pub dropped: u32,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "дыры {}/{TARGET_LINKS}, TUN {}, отправлено {}, получено {}, потеряно {}",
            self.live_holes,
            if self.tun { "подключён" } else { "нет" },
            self.to_peer,
            self.from_peer,
            self.dropped
        )
    }
}

/// Мост с TUN и канал, в который перекладываются пакеты от пира.
struct Attached {
    bridge: hp_tun::bridge::Bridge,
    to_bridge: mpsc::Sender<Incoming>,
}

/// Запущенный клиент. Должен создаваться внутри tokio-рантайма; задачи
/// `MultiLink` живут, пока жив рантайм (остановка — остановкой рантайма).
pub struct Client {
    multilink: Arc<MultiLink>,
    attached: Arc<Mutex<Option<Attached>>>,
}

impl Client {
    pub async fn start(config: ClientConfig) -> Result<Self> {
        let discovery = Discovery::StunMqtt {
            stun_addrs: config.stun_addrs,
            mqtt_addr: config.mqtt_addr,
            mqtt_ca_pem: config.mqtt_ca_pem,
        };
        let options = MultiLinkOptions {
            reorder_wait: std::time::Duration::from_millis(u64::from(config.reorder_wait_ms)),
            data_holes: config.data_holes,
            ..MultiLinkOptions::default()
        };
        let (multilink, incoming) = MultiLink::start_discovery("", discovery, config.my_id, config.peer_id, options).await?;
        let attached = Arc::new(Mutex::new(None));
        tokio::spawn(switch(incoming, attached.clone()));
        log::info!("клиент запущен: дыры к {}", config.peer_id);
        Ok(Self { multilink: Arc::new(multilink), attached })
    }

    /// Подключает TUN: IP-пакеты из него уходят пиру, пакеты от пира пишутся в него. Прежний
    /// TUN (если был) отключается.
    pub fn attach_tun(&self, tun: hp_tun::Tun) {
        let (to_bridge, from_switch) = mpsc::channel(256);
        let bridge = hp_tun::bridge::Bridge::start(tun, self.multilink.clone(), from_switch);
        *self.attached.lock().unwrap() = Some(Attached { bridge, to_bridge });
        log::info!("TUN подключён");
    }

    /// Отключает TUN (дыры остаются); дескриптор закрывается.
    pub fn detach_tun(&self) {
        if self.attached.lock().unwrap().take().is_some() {
            log::info!("TUN отключён");
        }
    }

    pub fn status(&self) -> Status {
        let attached = self.attached.lock().unwrap();
        let (to_peer, _, from_peer, dropped) =
            attached.as_ref().map(|a| a.bridge.stats().snapshot()).unwrap_or_default();
        Status { live_holes: self.multilink.live_count(), tun: attached.is_some(), to_peer, from_peer, dropped }
    }
}

/// Пакеты от пира — в текущий мост с TUN; пока TUN нет, отбрасываются.
async fn switch(mut incoming: mpsc::Receiver<Incoming>, attached: Arc<Mutex<Option<Attached>>>) {
    while let Some(packet) = incoming.recv().await {
        let target = attached.lock().unwrap().as_ref().map(|a| a.to_bridge.clone());
        if let Some(target) = target {
            let _ = target.try_send(packet);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line_is_readable() {
        let status = Status { live_holes: 7, tun: true, to_peer: 12, from_peer: 10, dropped: 1 };
        assert_eq!(status.to_string(), "дыры 7/10, TUN подключён, отправлено 12, получено 10, потеряно 1");
    }
}
