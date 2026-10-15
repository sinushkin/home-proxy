//! `vps-relay` — релей VPS-сервера (`connection::vps::relay`): для клиентов — ещё один такой же
//! сервер, а сам он перекладывает их пакеты на настоящий. К серверу релей приходит сам, по своему
//! GUID (у сервера — в `relays.txt`), с белого или серого адреса; клиенты узнают о нём от сервера.
//!
//! Настройки — окружение (для systemd — `EnvironmentFile`):
//!   VPS_SERVER — сервер `ip[:порт знакомства]` (порт по умолчанию 40000);
//!   MY_ID — GUID релея; SERVER_ID — GUID сервера;
//!   RELAY_PUBLIC_IP — белый адрес релея для клиентов (по умолчанию — адрес, с которого релей
//!   пришёл к серверу);
//!   RELAY_BOOTSTRAP_PORT — порт знакомства для клиентов (40000);
//!   RELAY_PORTS — порты сессий `a-b` (40001-49999);
//!   RUST_LOG, LOG_TARGET, LOG_FILE — логи (`hp-logging`).
//! Брандмауэр релея должен пропускать входящий UDP на порт знакомства и `RELAY_PORTS`.

use std::ops::RangeInclusive;

use anyhow::{Context, Result};
use connection::multilink::MultiLinkOptions;
use connection::vps::{self, relay};
use uuid::Uuid;

fn ports(value: &str) -> Option<RangeInclusive<u16>> {
    let (a, b) = value.trim().split_once('-')?;
    let (a, b): (u16, u16) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (a != 0 && a <= b).then_some(a..=b)
}

#[tokio::main]
async fn main() -> Result<()> {
    hp_logging::init()?;
    let get = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let need = |name: &str| get(name).with_context(|| format!("не задан {name}"));
    let id = |name: &str| -> Result<Uuid> { need(name)?.parse().with_context(|| format!("{name}: некорректный GUID")) };
    let config = relay::RelayConfig {
        server: vps::parse_server(&need("VPS_SERVER")?).context("VPS_SERVER: ожидается ip или ip:порт")?,
        my_id: id("MY_ID")?,
        server_id: id("SERVER_ID")?,
        public_ip: get("RELAY_PUBLIC_IP").map(|v| v.parse()).transpose().context("RELAY_PUBLIC_IP: ожидается IP")?,
        bootstrap_port: get("RELAY_BOOTSTRAP_PORT").map_or(Ok(vps::DEFAULT_BOOTSTRAP_PORT), |v| v.parse()).context("RELAY_BOOTSTRAP_PORT")?,
        ports: get("RELAY_PORTS").map_or(Some(vps::DEFAULT_SLOT_PORTS), |v| ports(&v)).context("RELAY_PORTS: ожидается «a-b»")?,
        options: MultiLinkOptions::default(),
    };
    let _relay = relay::start(config).await?;
    tokio::signal::ctrl_c().await.context("ожидание сигнала")?;
    log::info!("vps-relay: остановлен");
    Ok(())
}
