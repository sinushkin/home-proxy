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
//!   CLIENT_TIMEOUT_SECS — через сколько секунд тишины клиент удаляется (300);
//!   REORDER_WAIT_MS — сколько мс ждать недостающий пакет WireGuard при восстановлении
//!   порядка на приёме (8; 0 — выключить);
//!   DATA_HOLES — через сколько дыр слать данные (0 — через все живые, 1 — через одну).
//!
//! Логи: `RUST_LOG` (по умолчанию `info`), `LOG_TARGET=syslog` — в syslog,
//! `LOG_FILE=путь` — в файл.
//!
//! Библиотечная часть (`bridge`, `settings`, `Common`, `serve`) переиспользуется
//! крейтом `vps-server`.
//!
//! Запуск: `hp-server [--config server.env]` — обычный процесс (без `--config` берётся
//! `server.env` рядом с бинарником, если есть, иначе только окружение).
//! Windows: `hp-server install --config C:\путь\server.env` регистрирует службу
//! `homeproxy-server`, `hp-server uninstall` удаляет (см. `windows/README.md`).

pub mod bridge;
pub mod settings;
#[cfg(windows)]
mod winsvc;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bridge::{Bridge, ClientKey, Reply};
use connection::multilink::{Discovery, MultiLink, MultiLinkOptions, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use settings::Settings;
use uuid::Uuid;

const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
const STATUS_INTERVAL: Duration = Duration::from_secs(60);

/// Ответы WireGuard'а уходят пиру по дырам: роутеру — обёрнутыми с номером
/// клиента, телефону напрямую — обычной `Data`.
struct ToPeer(Arc<MultiLink>);

impl Reply for ToPeer {
    async fn send(&self, client: ClientKey, payload: &[u8]) -> Result<()> {
        match client {
            ClientKey::Routed(client_id) => self.0.send_wrapped(client_id, payload).await.map(|_| ()),
            ClientKey::Direct => self.0.send_data(payload).await.map(|_| ()),
        }
    }
}

/// Настройки моста, общие для `hp-server` и `vps-server`.
pub struct Common {
    pub my_id: Uuid,
    pub peer_id: Uuid,
    pub wg_addr: SocketAddr,
    pub client_timeout: Duration,
    pub reorder_wait: Duration,
    pub data_holes: u8,
}

impl Common {
    pub fn from_settings(settings: &Settings) -> Result<Self> {
        let get = |name: &str| settings.get(name);
        let need = |name: &str| get(name).with_context(|| format!("не задана переменная {name}"));
        let client_timeout_secs: u64 = match get("CLIENT_TIMEOUT_SECS") {
            Some(value) => value.parse().context("CLIENT_TIMEOUT_SECS: ожидается число секунд")?,
            None => 300,
        };
        Ok(Self {
            my_id: need("MY_ID")?.parse().context("MY_ID: некорректный GUID")?,
            peer_id: need("PEER_ID")?.parse().context("PEER_ID: некорректный GUID")?,
            wg_addr: get("WG_ADDR")
                .unwrap_or_else(|| "127.0.0.1:51820".to_string())
                .parse()
                .context("WG_ADDR: ожидается ip:порт")?,
            client_timeout: Duration::from_secs(client_timeout_secs),
            reorder_wait: match get("REORDER_WAIT_MS") {
                Some(value) => Duration::from_millis(value.parse().context("REORDER_WAIT_MS: ожидается число миллисекунд")?),
                None => DEFAULT_REORDER_WAIT,
            },
            data_holes: match get("DATA_HOLES") {
                Some(value) => value.parse().context("DATA_HOLES: ожидается число дыр 0..=255")?,
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
        mqtt_addr: need("MQTT_ADDR")?.parse().context("MQTT_ADDR: ожидается ip:порт")?,
        mqtt_ca_pem: std::fs::read(&mqtt_ca)
            .with_context(|| format!("не удалось прочитать CA-сертификат {}", mqtt_ca.display()))?,
    })
}

enum Command {
    Run,
    Service,
    Install,
    Uninstall,
}

struct Cli {
    command: Command,
    config: Option<PathBuf>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Cli> {
    let mut cli = Cli { command: Command::Run, config: None };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                cli.config = Some(args.next().context("--config: ожидается путь к файлу")?.into());
            }
            "--service" => cli.command = Command::Service,
            "install" => cli.command = Command::Install,
            "uninstall" => cli.command = Command::Uninstall,
            other => anyhow::bail!(
                "неизвестный аргумент {other}; ожидается [--config файл] или install/uninstall/--service"
            ),
        }
    }
    Ok(cli)
}

/// Файл настроек по умолчанию: `server.env` рядом с бинарником, если он есть.
pub fn default_config() -> Option<PathBuf> {
    let path = std::env::current_exe().ok()?.parent()?.join("server.env");
    path.is_file().then_some(path)
}

/// Поднимает логи и runtime и гоняет сервер, пока он сам не завершится или не
/// придёт `shutdown` (остановка службы).
fn run_blocking(settings: &Settings, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
    init_logging(settings)?;
    let common = Common::from_settings(settings)?;
    let discovery = p2p_discovery(settings)?;
    tokio::runtime::Runtime::new()?.block_on(async {
        tokio::select! {
            result = serve(discovery, common) => result,
            () = shutdown => {
                log::info!("получена команда остановки");
                Ok(())
            }
        }
    })
}

/// Точка входа `hp-server`.
pub fn main() -> Result<()> {
    let cli = parse_args(std::env::args().skip(1))?;
    let config = cli.config.clone().or_else(default_config);
    match cli.command {
        Command::Run => run_blocking(&Settings::load(config.as_deref())?, std::future::pending()),
        #[cfg(windows)]
        Command::Service => winsvc::run_dispatcher(config),
        #[cfg(windows)]
        Command::Install => winsvc::install(cli.config),
        #[cfg(windows)]
        Command::Uninstall => winsvc::uninstall(),
        #[cfg(not(windows))]
        Command::Service | Command::Install | Command::Uninstall => {
            anyhow::bail!("служба поддерживается только на Windows")
        }
    }
}

/// Мост «дыры → WireGuard»: поднимает `MultiLink` с заданным способом встречи и
/// раскладывает пакеты по клиентам, пока канал входящих жив.
pub async fn serve(discovery: Discovery, common: Common) -> Result<()> {
    log::info!(
        "сервер: я {} ищу пира {} (роутер или телефон), WireGuard {}, клиент удаляется через {} с тишины, порядок пакетов: ожидание {} мс, дыр для данных: {}",
        common.my_id,
        common.peer_id,
        common.wg_addr,
        common.client_timeout.as_secs(),
        common.reorder_wait.as_millis(),
        if common.data_holes == 0 { "все".to_string() } else { common.data_holes.to_string() }
    );

    let options = MultiLinkOptions { reorder_wait: common.reorder_wait, data_holes: common.data_holes, local_port_base: 0, ..MultiLinkOptions::default() };
    let (link, mut incoming) = MultiLink::start_discovery("", discovery, common.my_id, common.peer_id, options).await?;
    let link = Arc::new(link);
    let bridge = Bridge::new(common.wg_addr, ToPeer(link.clone()), common.client_timeout);

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
                log::info!("клиентов: {}, дыр к пиру: {}/{TARGET_LINKS}", now.0, now.1);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_means_plain_run() {
        let cli = parse_args(args(&[])).unwrap();
        assert!(matches!(cli.command, Command::Run));
        assert!(cli.config.is_none());
    }

    #[test]
    fn install_takes_a_config_path() {
        let cli = parse_args(args(&["install", "--config", "C:\\hp\\server.env"])).unwrap();
        assert!(matches!(cli.command, Command::Install));
        assert_eq!(cli.config, Some(PathBuf::from("C:\\hp\\server.env")));
    }

    #[test]
    fn service_flag_and_bad_arguments() {
        assert!(matches!(parse_args(args(&["--service"])).unwrap().command, Command::Service));
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["--nope"])).is_err());
    }

}
