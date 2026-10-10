//! Точки входа в `MultiLink`: как стороны узнают друг о друге. Тонкий слой над режимами — P2P
//! (`p2p`, оба за NAT: STUN + MQTT + пробив) и VPS (`vps`, у сервера белый IP): собирает настройки
//! выбранного режима и запускает его через `MultiLink::start_mode`. Сам `MultiLink` о режимах
//! ничего не знает.

#[cfg(feature = "vps")]
use std::net::IpAddr;
use std::net::SocketAddr;
#[cfg(feature = "p2p")]
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::multilink::{Incoming, ModeContext, ModeStarted, MultiLink, MultiLinkOptions};

#[cfg(feature = "p2p")]
use crate::multilink::RegistrationSource;

/// Как стороны узнают друг о друге.
#[derive(Clone, Debug)]
pub enum Discovery {
    /// Обычный режим: STUN + MQTT, пробив NAT.
    #[cfg(feature = "p2p")]
    StunMqtt { stun_addrs: Vec<SocketAddr>, mqtt_addr: SocketAddr, mqtt_ca_pem: Vec<u8> },
    /// Сервер с белым IP: слушает порт знакомства, дыры занимают порты из банка, пробив пассивный
    /// (ждём клиента). `bootstrap` — порт знакомства процесса и банк портов (`vps::Bootstrap`),
    /// общие для всех клиентов.
    #[cfg(feature = "vps")]
    VpsServer { public_ip: IpAddr, bootstrap: crate::vps::Bootstrap },
    /// Клиент VPS-сервера: знает `ip:порт знакомства`, ни STUN, ни MQTT не нужен.
    #[cfg(feature = "vps")]
    VpsClient { server: SocketAddr },
}

impl MultiLink {
    /// P2P: `label` — метка набора для логов (пустая — без метки). `stun_addrs` — один или несколько
    /// STUN-серверов: сокет каждой дыры опрашивает их все, и всё увиденное публикуется как адреса
    /// дыры (пир стучится по каждому).
    #[cfg(feature = "p2p")]
    pub async fn start(
        label: &str,
        stun_addrs: Vec<SocketAddr>,
        mqtt_addr: SocketAddr,
        mqtt_ca_pem: Vec<u8>,
        my_peer_id: Uuid,
        peer_id: Uuid,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        Self::start_with(label, stun_addrs, mqtt_addr, mqtt_ca_pem, my_peer_id, peer_id, MultiLinkOptions::default()).await
    }

    /// То же, что `start`, с явными настройками.
    #[cfg(feature = "p2p")]
    pub async fn start_with(
        label: &str,
        stun_addrs: Vec<SocketAddr>,
        mqtt_addr: SocketAddr,
        mqtt_ca_pem: Vec<u8>,
        my_peer_id: Uuid,
        peer_id: Uuid,
        options: MultiLinkOptions,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        let discovery = Discovery::StunMqtt { stun_addrs, mqtt_addr, mqtt_ca_pem };
        Self::start_discovery(label, discovery, my_peer_id, peer_id, options).await
    }

    /// Запуск с явным способом знакомства.
    pub async fn start_discovery(
        label: &str,
        discovery: Discovery,
        my_peer_id: Uuid,
        peer_id: Uuid,
        options: MultiLinkOptions,
    ) -> Result<(Self, mpsc::Receiver<Incoming>)> {
        Self::start_mode(label, my_peer_id, peer_id, options, move |ctx| start_mode_of(discovery, ctx)).await
    }
}

async fn start_mode_of(discovery: Discovery, ctx: ModeContext) -> Result<ModeStarted> {
    match discovery {
        #[cfg(feature = "p2p")]
        Discovery::StunMqtt { stun_addrs, mqtt_addr, mqtt_ca_pem } => {
            let config = p2p_config(&ctx, stun_addrs, mqtt_addr, mqtt_ca_pem);
            let started = crate::p2p::start(&ctx.label, config, ctx.factory.clone()).await?;
            let registration = started.registrar.map(|registrar| -> RegistrationSource { Arc::new(move || registrar.last_peer_registration()) });
            Ok(ModeStarted { tasks: started.tasks, hole_records: Some(started.hole_records), registration })
        }
        #[cfg(feature = "vps")]
        Discovery::VpsServer { public_ip, bootstrap } => {
            let config = crate::vps::ServerConfig { public_ip, pair: vps_pair(&ctx), bootstrap, pool: ctx.options.pool };
            let tasks = crate::vps::start_server(&ctx.label, config, ctx.factory.clone()).await?;
            Ok(ModeStarted { tasks, hole_records: None, registration: None })
        }
        #[cfg(feature = "vps")]
        Discovery::VpsClient { server } => {
            let config = crate::vps::ClientConfig {
                server,
                pair: vps_pair(&ctx),
                bind_ip: ctx.bind_ip,
                bind_ifindex: ctx.options.bind_ifindex,
                pool: ctx.options.pool,
                hole_age: ctx.options.hole_age,
            };
            let tasks = crate::vps::start_client(&ctx.label, config, ctx.factory.clone()).await?;
            Ok(ModeStarted { tasks, hole_records: None, registration: None })
        }
    }
}

#[cfg(feature = "vps")]
fn vps_pair(ctx: &ModeContext) -> crate::vps::Pair {
    crate::vps::Pair { my_peer_id: ctx.my_peer_id, peer_id: ctx.peer_id, secret: ctx.pair.clone() }
}

#[cfg(feature = "p2p")]
fn p2p_config(ctx: &ModeContext, stun_addrs: Vec<SocketAddr>, mqtt_addr: SocketAddr, mqtt_ca_pem: Vec<u8>) -> crate::p2p::Config {
    crate::p2p::Config {
        stun_addrs,
        mqtt_addr,
        mqtt_ca_pem,
        my_peer_id: ctx.my_peer_id,
        peer_id: ctx.peer_id,
        pair: ctx.pair.clone(),
        bind_ip: ctx.bind_ip,
        bind_ifindex: ctx.options.bind_ifindex,
        local_port_base: ctx.options.local_port_base,
        pool: ctx.options.pool,
        hole_age: ctx.options.hole_age,
    }
}
