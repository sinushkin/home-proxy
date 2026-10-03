//! `vps-server`: сервер на машине с белым IP. Обычная схема «клиент — сервер»: ни STUN,
//! ни MQTT, ни пробива. Клиенты (`vps-client`) приходят на один порт знакомства, сервер раздаёт
//! каждому случайные порты слотов из диапазона и переносит дыры при просадках. IP-пакеты клиентов
//! идут в TUN, как у `hp-server` (тот же `hp_server::serve_with_changes`).
//!
//! Клиенты — из файла `clients.txt` (`CLIENTS_FILE`, по умолчанию рядом с настройками): один GUID
//! клиента на строку, `#` — комментарий. Файл перечитывается раз в 2 с: клиент добавляется и
//! удаляется без перезапуска сервера. Общий для всех GUID сервера — `MY_ID`.
//!
//! Запуск: `vps-server [--config vps.env]` (без `--config` — `vps.env` рядом с бинарником,
//! если есть, иначе только окружение). Настройки:
//!   MY_ID — GUID сервера (обязателен, один на всех клиентов);
//!   VPS_PUBLIC_IP — белый IP сервера (обязателен);
//!   VPS_BOOTSTRAP_PORT — порт знакомства (40000);
//!   VPS_PORTS — диапазон портов слотов `начало-конец` (40001-49999), общий для всех клиентов;
//!   CLIENTS_FILE — файл клиентов (clients.txt);
//!   TUN_ADDR (`10.80.0.1/16`), TUN_NAME, TUN_MTU, ADDRESS_FILE, REORDER_WAIT_MS, DATA_HOLES,
//!   RUST_LOG, LOG_FILE — как у `hp-server` (адреса клиентам и их телефонам раздаёт сервер);
//!   NAT подсети туннеля наружу настраивается отдельно;
//!   RUNTIME=multi — многопоточный tokio (по умолчанию однопоточный).
//! Брандмауэр должен пропускать входящий UDP на порт знакомства и весь `VPS_PORTS`.

use std::collections::HashSet;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use connection::auth::peer_name;
use connection::multilink::Discovery;
use connection::vps;
use hp_server::settings::Settings;
use hp_server::{Peer, PeerChange};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Как часто перечитываем файл клиентов.
const CLIENTS_POLL: Duration = Duration::from_secs(2);

/// `VPS_PORTS=начало-конец`.
fn parse_ports(value: &str) -> Result<RangeInclusive<u16>> {
    let (low, high) = value.split_once('-').context("VPS_PORTS: ожидается начало-конец")?;
    let (low, high): (u16, u16) = (low.trim().parse()?, high.trim().parse()?);
    anyhow::ensure!(low <= high, "VPS_PORTS: начало больше конца");
    Ok(low..=high)
}

/// Общие настройки сервера: белый IP, порт знакомства и диапазон портов слотов.
fn public_settings(settings: &Settings) -> Result<(IpAddr, u16, RangeInclusive<u16>)> {
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
    Ok((public_ip, bootstrap_port, ports))
}

/// Разбор файла клиентов: GUID-ы клиентов и предупреждения по строкам. Клиенты с одинаковым
/// коротким именем (первая группа GUID) не берём: порт знакомства различает клиентов по нему.
fn parse_clients(text: &str) -> (Vec<Uuid>, Vec<String>) {
    let (mut clients, mut problems, mut names) = (Vec::new(), Vec::new(), HashSet::new());
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Ok(id) = line.parse::<Uuid>() else {
            problems.push(format!("строка {}: ожидается GUID клиента, строка пропущена", n + 1));
            continue;
        };
        if !names.insert(peer_name(&id)) {
            problems.push(format!("строка {}: имя клиента {} уже занято другим клиентом, строка пропущена", n + 1, peer_name(&id)));
            continue;
        }
        clients.push(id);
    }
    (clients, problems)
}

/// Следит за файлом клиентов: сравнивает список с предыдущим и шлёт изменения в сервер.
/// Если файла нет или он не читается, список не меняем: случайно стёртый файл не должен
/// разорвать все туннели.
async fn watch_clients(path: PathBuf, server: Uuid, discovery: Discovery, changes: mpsc::Sender<PeerChange>) {
    let mut known: HashSet<Uuid> = HashSet::new();
    let mut last_text: Option<String> = None;
    let mut tick = tokio::time::interval(CLIENTS_POLL);
    loop {
        tick.tick().await;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                if last_text.is_some() {
                    log::warn!("клиенты: не удалось прочитать {}: {e}; список не меняем", path.display());
                    last_text = None;
                }
                continue;
            }
        };
        if last_text.as_deref() != Some(text.as_str()) {
            let (_, problems) = parse_clients(&text);
            for problem in problems {
                log::warn!("клиенты: {}: {problem}", path.display());
            }
            last_text = Some(text.clone());
        }
        let (clients, _) = parse_clients(&text);
        let wanted: HashSet<Uuid> = clients.into_iter().collect();
        let mut added: Vec<Uuid> = wanted.difference(&known).copied().collect();
        added.sort();
        let mut removed: Vec<Uuid> = known.difference(&wanted).copied().collect();
        removed.sort();
        for peer_id in added {
            log::info!("клиенты: добавлен {}", peer_name(&peer_id));
            let peer = Peer { my_id: server, peer_id };
            if changes.send(PeerChange::Add(peer, discovery.clone())).await.is_err() {
                return;
            }
        }
        for peer_id in removed {
            log::info!("клиенты: удалён {}", peer_name(&peer_id));
            if changes.send(PeerChange::Remove(peer_id)).await.is_err() {
                return;
            }
        }
        known = wanted;
    }
}

/// Файл клиентов: `CLIENTS_FILE`, относительный путь — от каталога файла настроек.
fn clients_file(settings: &Settings) -> PathBuf {
    settings.resolve(&settings.get("CLIENTS_FILE").unwrap_or_else(|| "clients.txt".into()))
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

async fn run(settings: Settings) -> Result<()> {
    let server_id: Uuid = settings
        .get("MY_ID")
        .context("не задан MY_ID (GUID сервера, общий для всех клиентов)")?
        .trim()
        .parse()
        .context("MY_ID: некорректный GUID")?;
    let (public_ip, bootstrap_port, ports) = public_settings(&settings)?;
    let common = hp_server::Common::from_settings_without_peers(&settings)?;
    let bootstrap = vps::Bootstrap::bind(bootstrap_port).await?;
    log::info!("vps-server: белый IP {public_ip}, порт знакомства {bootstrap_port}, порты слотов {}-{}", ports.start(), ports.end());
    let discovery = Discovery::VpsServer { public_ip, ports, bootstrap };
    let path = clients_file(&settings);
    log::info!("vps-server: клиенты из {}", path.display());
    let (changes_tx, changes_rx) = mpsc::channel(64);
    tokio::spawn(watch_clients(path, server_id, discovery, changes_tx));
    hp_server::serve_with_changes(Vec::new(), common, Some(changes_rx)).await
}

fn main() -> Result<()> {
    let settings = Settings::load(config_path()?.as_deref())?;
    hp_server::init_logging(&settings)?;
    // Однопоточный tokio по умолчанию (VPS часто с одним vCPU), `RUNTIME=multi` — многопоточный.
    let runtime = if settings.get("RUNTIME").as_deref() == Some("multi") {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    } else {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?
    };
    runtime.block_on(run(settings))
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
        let (ip, port, ports) = public_settings(&settings_from("VPS_PUBLIC_IP=203.0.113.10\n")).unwrap();
        assert_eq!(ip.to_string(), "203.0.113.10");
        assert_eq!(port, vps::DEFAULT_BOOTSTRAP_PORT);
        assert_eq!(ports, vps::DEFAULT_SLOT_PORTS);
        let error = public_settings(&settings_from("VPS_BOOTSTRAP_PORT=40000\n")).unwrap_err().to_string();
        assert!(error.contains("VPS_PUBLIC_IP"), "{error}");
    }

    #[test]
    fn bootstrap_port_must_be_outside_the_slot_range() {
        let text = "VPS_PUBLIC_IP=203.0.113.10\nVPS_BOOTSTRAP_PORT=40005\nVPS_PORTS=40001-40010\n";
        assert!(public_settings(&settings_from(text)).is_err());
    }

    #[test]
    fn clients_file_skips_comments_and_bad_lines() {
        let a = Uuid::new_v4();
        let text = format!("# клиенты\n\n{a}\nне-guid\n");
        let (clients, problems) = parse_clients(&text);
        assert_eq!(clients, vec![a]);
        assert_eq!(problems.len(), 1, "{problems:?}");
    }

    #[test]
    fn clients_with_the_same_short_name_are_not_both_taken() {
        let a = Uuid::parse_str("12345678-0000-4000-8000-000000000001").unwrap();
        let b = Uuid::parse_str("12345678-0000-4000-8000-000000000002").unwrap();
        let (clients, problems) = parse_clients(&format!("{a}\n{b}\n"));
        assert_eq!(clients, vec![a]);
        assert_eq!(problems.len(), 1, "{problems:?}");
    }

    #[tokio::test]
    async fn watcher_sends_only_the_difference() {
        let dir = std::env::temp_dir().join(format!("vps-clients-{}-{}", std::process::id(), uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clients.txt");
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        std::fs::write(&file, format!("{a}\n")).unwrap();
        let discovery = Discovery::VpsClient { server: "127.0.0.1:1".parse().unwrap() };
        let (tx, mut rx) = mpsc::channel(16);
        tokio::spawn(watch_clients(file.clone(), Uuid::new_v4(), discovery, tx));
        let wait = Duration::from_secs(10);

        let first = tokio::time::timeout(wait, rx.recv()).await.unwrap().unwrap();
        assert!(matches!(first, PeerChange::Add(Peer { peer_id, .. }, _) if peer_id == a));
        std::fs::write(&file, format!("{b}\n")).unwrap();
        let second = tokio::time::timeout(wait, rx.recv()).await.unwrap().unwrap();
        assert!(matches!(second, PeerChange::Add(Peer { peer_id, .. }, _) if peer_id == b));
        let third = tokio::time::timeout(wait, rx.recv()).await.unwrap().unwrap();
        assert!(matches!(third, PeerChange::Remove(id) if id == a));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
