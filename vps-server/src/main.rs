//! `vps-server`: сервер на машине с белым IP. Обычная схема «клиент — сервер»: ни STUN,
//! ни MQTT, ни пробива. Клиент (`vps-client`) приходит на порт знакомства, сервер раздаёт
//! ему случайные порты слотов из диапазона и сам переносит дыру на другой порт при
//! просадках. Мост к WireGuard — тот же, что у `hp-server`.
//!
//! Запуск: `vps-server [--config vps.env]` (без `--config` — `vps.env` рядом с бинарником,
//! если есть, иначе только окружение). Настройки:
//!   VPS_PUBLIC_IP — белый IP сервера (обязателен);
//!   VPS_BOOTSTRAP_PORT — порт знакомства (40000);
//!   VPS_PORTS — диапазон портов слотов `начало-конец` (40001-49999);
//!   MY_ID, PEER_ID, WG_ADDR, CLIENT_TIMEOUT_SECS, REORDER_WAIT_MS, DATA_HOLES,
//!   RUST_LOG, LOG_FILE — как у `hp-server`;
//!   RUNTIME=multi — многопоточный tokio (по умолчанию однопоточный).
//! Брандмауэр должен пропускать входящий UDP на порт знакомства и весь `VPS_PORTS`.

use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::path::PathBuf;

use anyhow::{Context, Result};
use connection::multilink::Discovery;
use connection::vps;
use hp_server::settings::Settings;

/// `VPS_PORTS=начало-конец`.
fn parse_ports(value: &str) -> Result<RangeInclusive<u16>> {
    let (low, high) = value.split_once('-').context("VPS_PORTS: ожидается начало-конец")?;
    let (low, high): (u16, u16) = (low.trim().parse()?, high.trim().parse()?);
    anyhow::ensure!(low <= high, "VPS_PORTS: начало больше конца");
    Ok(low..=high)
}

fn discovery(settings: &Settings) -> Result<Discovery> {
    let public_ip: IpAddr = settings
        .get("VPS_PUBLIC_IP")
        .context("не задана переменная VPS_PUBLIC_IP")?
        .parse()
        .context("VPS_PUBLIC_IP: ожидается IP")?;
    let bootstrap_port = match settings.get("VPS_BOOTSTRAP_PORT") {
        Some(value) => value.parse().context("VPS_BOOTSTRAP_PORT: ожидается порт")?,
        None => vps::DEFAULT_BOOTSTRAP_PORT,
    };
    let ports = match settings.get("VPS_PORTS") {
        Some(value) => parse_ports(&value)?,
        None => vps::DEFAULT_SLOT_PORTS,
    };
    anyhow::ensure!(!ports.contains(&bootstrap_port), "порт знакомства {bootstrap_port} внутри VPS_PORTS");
    Ok(Discovery::VpsServer { public_ip, bootstrap_port, ports })
}

fn config_path() -> Result<Option<PathBuf>> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (None, _) => {
            let default = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("vps.env")));
            Ok(default.filter(|p| p.is_file()))
        }
        (Some("--config"), Some(path)) => Ok(Some(path.into())),
        _ => anyhow::bail!("использование: vps-server [--config vps.env]"),
    }
}

fn main() -> Result<()> {
    let settings = Settings::load(config_path()?.as_deref())?;
    hp_server::init_logging(&settings)?;
    let common = hp_server::Common::from_settings(&settings)?;
    let discovery = discovery(&settings)?;
    if let Discovery::VpsServer { public_ip, bootstrap_port, ports } = &discovery {
        log::info!("vps-server: белый IP {public_ip}, порт знакомства {bootstrap_port}, порты слотов {}-{}", ports.start(), ports.end());
    }
    // Однопоточный tokio по умолчанию (VPS часто с одним vCPU), `RUNTIME=multi` — многопоточный.
    let runtime = if settings.get("RUNTIME").as_deref() == Some("multi") {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    } else {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?
    };
    runtime.block_on(hp_server::serve(discovery, common))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_parse_and_reject_reversed_ranges() {
        assert_eq!(parse_ports("40001-49999").unwrap(), 40001..=49999);
        assert_eq!(parse_ports(" 5 - 5 ").unwrap(), 5..=5);
        assert!(parse_ports("9-1").is_err());
        assert!(parse_ports("40001").is_err());
    }

    fn settings_from(text: &str) -> Settings {
        let dir = std::env::temp_dir().join(format!("vps-server-{}-{}", std::process::id(), uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("vps.env");
        std::fs::write(&file, text).unwrap();
        let settings = Settings::load(Some(&file)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        settings
    }

    fn uuid_like() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn defaults_and_required_public_ip() {
        match discovery(&settings_from("VPS_PUBLIC_IP=203.0.113.10\n")).unwrap() {
            Discovery::VpsServer { bootstrap_port, ports, .. } => {
                assert_eq!(bootstrap_port, vps::DEFAULT_BOOTSTRAP_PORT);
                assert_eq!(ports, vps::DEFAULT_SLOT_PORTS);
            }
            _ => panic!("ожидали VPS-режим"),
        }
        let error = discovery(&settings_from("VPS_BOOTSTRAP_PORT=40000\n")).unwrap_err().to_string();
        assert!(error.contains("VPS_PUBLIC_IP"), "{error}");
    }

    #[test]
    fn bootstrap_port_must_be_outside_the_slot_range() {
        let text = "VPS_PUBLIC_IP=203.0.113.10\nVPS_BOOTSTRAP_PORT=40005\nVPS_PORTS=40001-40010\n";
        assert!(discovery(&settings_from(text)).is_err());
    }
}
