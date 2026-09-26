//! Сервер: принимает IP-пакеты клиентов по дырам и пишет их в TUN, ответы из TUN — обратно по
//! дырам (`hp_tun::bridge`); в интернет пакеты выходят через NAT подсети туннеля на этой машине
//! (настраивается отдельно). Пиром может быть телефон напрямую (`Ordered`/`Data`) или роутер
//! OpenWrt (пакеты его телефонов — `WrappedData` с `client_id`, клиент узнаётся по адресу
//! источника). Набор из 10 дыр к пиру постоянный.
//!
//! Настройки — переменные окружения и/или файл `KEY=VALUE` (`--config server.env`):
//!   STUN_ADDR, MQTT_ADDR, MQTT_CA — как у `peer` (STUN_ADDR — один или несколько
//!   серверов через запятую: `ip:порт,ip2:порт`);
//!   MY_ID / PEER_ID — GUID сервера и GUID пира (телефона или роутера);
//!   TUN_ADDR — адрес сервера в туннеле и подсеть клиентов (`10.80.0.1/16`, по умолчанию так),
//!   TUN_NAME (`hp0`), TUN_MTU (1400); нужны права root (`CAP_NET_ADMIN`);
//!   REORDER_WAIT_MS — сколько мс ждать недостающий TCP-пакет при восстановлении порядка (8;
//!   0 — выключить); DATA_HOLES — через сколько дыр слать данные (0 — через все живые).
//!
//! Логи: `RUST_LOG` (по умолчанию `info`), `LOG_TARGET=syslog` — в syslog,
//! `LOG_FILE=путь` — в файл.
//!
//! Библиотечная часть (`settings`, `Common`, `serve`) переиспользуется `vps-server` и
//! `hp-router`. Запуск: `hp-server [--config server.env]` (без `--config` берётся
//! `server.env` рядом с бинарником, если есть, иначе только окружение). Linux (в т.ч. WSL2 на
//! Windows, см. `wsl/README.md`).

pub mod settings;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use anyhow::{Context, Result};
use connection::multilink::{Discovery, MultiLink, MultiLinkOptions, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use settings::Settings;
use uuid::Uuid;

const STATUS_INTERVAL: Duration = Duration::from_secs(30);

/// Адрес сервера в туннеле по умолчанию: подсеть `/16` покрывает роутер (`10.80.0.2`) и
/// телефоны (`10.80.1.<n>`).
pub const DEFAULT_TUN_ADDR: &str = "10.80.0.1/16";

/// Настройки, общие для `hp-server` и `vps-server`.
pub struct Common {
    pub my_id: Uuid,
    pub peer_id: Uuid,
    pub tun: hp_tun::TunConfig,
    pub reorder_wait: Duration,
    pub data_holes: u8,
}

impl Common {
    pub fn from_settings(settings: &Settings) -> Result<Self> {
        let get = |name: &str| settings.get(name);
        let need = |name: &str| get(name).with_context(|| format!("не задана переменная {name}"));
        let tun_addr = get("TUN_ADDR").unwrap_or_else(|| DEFAULT_TUN_ADDR.to_string());
        Ok(Self {
            my_id: need("MY_ID")?.trim().parse().context("MY_ID: некорректный GUID")?,
            peer_id: need("PEER_ID")?.trim().parse().context("PEER_ID: некорректный GUID")?,
            tun: hp_tun::TunConfig {
                name: get("TUN_NAME").unwrap_or_else(|| "hp0".into()),
                address: Some(hp_tun::parse_cidr(&tun_addr).context("TUN_ADDR: ожидается ip/префикс")?),
                mtu: Some(match get("TUN_MTU") {
                    Some(value) => value.trim().parse().context("TUN_MTU: ожидается число")?,
                    None => 1400,
                }),
                up: true,
            },
            reorder_wait: match get("REORDER_WAIT_MS") {
                Some(value) => Duration::from_millis(value.trim().parse().context("REORDER_WAIT_MS: ожидается число миллисекунд")?),
                None => DEFAULT_REORDER_WAIT,
            },
            data_holes: match get("DATA_HOLES") {
                Some(value) => value.trim().parse().context("DATA_HOLES: ожидается число дыр 0..=255")?,
                None => 0,
            },
        })
    }
}

/// Логи по настройкам: `LOG_FILE` считается от каталога файла настроек.
pub fn init_logging(settings: &Settings) -> Result<()> {
    hp_logging::init_with(|name| match name {
        "LOG_FILE" => settings.get(name).map(|path| settings.resolve(&path).to_string_lossy().into_owned()),
        _ => settings.get(name),
    })?;
    Ok(())
}

/// Обычный (P2P) режим: встреча через STUN и MQTT.
fn p2p_discovery(settings: &Settings) -> Result<Discovery> {
    let need = |name: &str| settings.get(name).with_context(|| format!("не задана переменная {name}"));
    let mqtt_ca = settings.resolve(&need("MQTT_CA")?);
    Ok(Discovery::StunMqtt {
        stun_addrs: connection::stun::parse_servers(&need("STUN_ADDR")?).context("STUN_ADDR")?,
        mqtt_addr: need("MQTT_ADDR")?.trim().parse().context("MQTT_ADDR: ожидается ip:порт")?,
        mqtt_ca_pem: std::fs::read(&mqtt_ca)
            .with_context(|| format!("не удалось прочитать CA-сертификат {}", mqtt_ca.display()))?,
    })
}

/// `--config файл` или ничего.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<PathBuf>> {
    let mut args = args.into_iter();
    match (args.next().as_deref(), args.next(), args.next()) {
        (None, _, _) => Ok(None),
        (Some("--config"), Some(path), None) => Ok(Some(path.into())),
        _ => anyhow::bail!("использование: hp-server [--config server.env]"),
    }
}

/// Файл настроек по умолчанию: `server.env` рядом с бинарником, если он есть.
pub fn default_config() -> Option<PathBuf> {
    let path = std::env::current_exe().ok()?.parent()?.join("server.env");
    path.is_file().then_some(path)
}

/// Точка входа `hp-server`.
pub fn main() -> Result<()> {
    let config = parse_args(std::env::args().skip(1))?.or_else(default_config);
    let settings = Settings::load(config.as_deref())?;
    init_logging(&settings)?;
    let common = Common::from_settings(&settings)?;
    let discovery = p2p_discovery(&settings)?;
    tokio::runtime::Runtime::new()?.block_on(serve(discovery, common))
}

/// Сервер на TUN: поднимает интерфейс и `MultiLink` с заданным способом встречи и гоняет мост
/// TUN ↔ дыры, пока жив процесс.
pub async fn serve(discovery: Discovery, common: Common) -> Result<()> {
    let tun = hp_tun::Tun::create(&common.tun).context("создание TUN (нужны права root)")?;
    let (addr, prefix) = common.tun.address.unwrap_or((std::net::Ipv4Addr::UNSPECIFIED, 0));
    log::info!(
        "сервер: я {}, пир {}, TUN {} {addr}/{prefix}, порядок пакетов: ожидание {} мс, дыр для данных: {}",
        common.my_id,
        common.peer_id,
        tun.name(),
        common.reorder_wait.as_millis(),
        if common.data_holes == 0 { "все".to_string() } else { common.data_holes.to_string() }
    );
    let options = MultiLinkOptions { reorder_wait: common.reorder_wait, data_holes: common.data_holes, ..MultiLinkOptions::default() };
    let (link, incoming) = MultiLink::start_discovery("", discovery, common.my_id, common.peer_id, options).await?;
    let link = Arc::new(link);
    let bridge = hp_tun::bridge::Bridge::start(tun, link.clone(), incoming);
    let mut last = None;
    loop {
        tokio::time::sleep(STATUS_INTERVAL).await;
        let stats = bridge.stats();
        let phones = (stats.clients_to.load(Relaxed), stats.clients_from.load(Relaxed));
        let now = (link.live_count(), stats.snapshot(), phones);
        if Some(now) != last {
            let (to, ordered, from, dropped) = now.1;
            log::info!(
                "дыры {}/{TARGET_LINKS}, к пиру {to} (TCP с номером {ordered}), от пира {from}, потеряно {dropped}; из них телефонам за роутером {}, от них {}",
                now.0,
                phones.0,
                phones.1
            );
            last = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn config_argument_is_optional() {
        assert_eq!(parse_args(args(&[])).unwrap(), None);
        assert_eq!(parse_args(args(&["--config", "/etc/hp/server.env"])).unwrap(), Some(PathBuf::from("/etc/hp/server.env")));
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["install"])).is_err());
    }
}
