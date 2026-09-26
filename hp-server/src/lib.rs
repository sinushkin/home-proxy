//! Сервер: принимает IP-пакеты клиентов по дырам и пишет их в TUN, ответы из TUN — обратно по
//! дырам (`hp_tun::hub`); в интернет пакеты выходят через NAT подсети туннеля на этой машине
//! (настраивается отдельно). Пиров может быть несколько (у каждого свой набор из 10 дыр): телефоны
//! напрямую (`Ordered`/`Data`) или роутер OpenWrt (пакеты его телефонов — `WrappedData` с
//! `client_id`). Адреса в туннеле раздаёт сервер (`addresses`): клиент просит адрес
//! (`AddressRequest`), сервер выдаёт постоянный для этого клиента.
//!
//! Настройки — переменные окружения и/или файл `KEY=VALUE` (`--config server.env`):
//!   STUN_ADDR, MQTT_ADDR, MQTT_CA — как у `peer` (STUN_ADDR — один или несколько
//!   серверов через запятую: `ip:порт,ip2:порт`);
//!   MY_ID / PEER_ID — GUID сервера и GUID пира (телефона или роутера); несколько пиров —
//!   PEER_<n>_MY_ID / PEER_<n>_PEER_ID, n = 1, 2, … подряд (GUID сервера у каждого свой: брокер
//!   различает соединения по нему);
//!   ADDRESS_FILE — где хранить выданные адреса (`addresses.state` рядом с файлом настроек);
//!   DNS — DNS для клиентов через запятую (по умолчанию — резолверы этой машины из resolv.conf;
//!   в конце всегда 8.8.8.8, 1.1.1.1);
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

pub mod addresses;
pub mod settings;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use connection::auth::peer_name;
use connection::multilink::{Control, Discovery, MultiLink, MultiLinkOptions, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use connection::proto::{AddressAssign, AddressKind};
use settings::Settings;
use tokio::sync::mpsc;
use uuid::Uuid;

use addresses::{client_key, AddressBook};

const STATUS_INTERVAL: Duration = Duration::from_secs(30);

/// Адрес сервера в туннеле по умолчанию: подсеть `/16` покрывает роутер (`10.80.0.2`) и
/// телефоны (`10.80.1.<n>`).
pub const DEFAULT_TUN_ADDR: &str = "10.80.0.1/16";

/// Пара GUID: сервер и пир (у каждого пира свой набор дыр).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    pub my_id: Uuid,
    pub peer_id: Uuid,
}

/// Настройки, общие для `hp-server` и `vps-server`.
pub struct Common {
    pub peers: Vec<Peer>,
    pub tun: hp_tun::TunConfig,
    /// Где хранить выданные адреса (`None` — только в памяти).
    pub address_file: Option<PathBuf>,
    /// DNS, которые сервер отдаёт клиентам вместе с адресом.
    pub dns: Vec<std::net::Ipv4Addr>,
    pub reorder_wait: Duration,
    pub data_holes: u8,
}

impl Common {
    pub fn from_settings(settings: &Settings) -> Result<Self> {
        let get = |name: &str| settings.get(name);
        let tun_addr = get("TUN_ADDR").unwrap_or_else(|| DEFAULT_TUN_ADDR.to_string());
        Ok(Self {
            peers: parse_peers(&get)?,
            address_file: Some(settings.resolve(get("ADDRESS_FILE").as_deref().unwrap_or("addresses.state").trim())),
            dns: addresses::client_dns(get("DNS").as_deref())?,
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

/// Пиры: `PEER_<n>_MY_ID` / `PEER_<n>_PEER_ID` (n = 1, 2, … подряд) или один — `MY_ID` / `PEER_ID`.
fn parse_peers(get: &impl Fn(&str) -> Option<String>) -> Result<Vec<Peer>> {
    let parse = |name: &str, value: String| -> Result<Uuid> { value.trim().parse().with_context(|| format!("{name}: некорректный GUID")) };
    let mut peers = Vec::new();
    for n in 1.. {
        let (my, peer) = (format!("PEER_{n}_MY_ID"), format!("PEER_{n}_PEER_ID"));
        match (get(&my), get(&peer)) {
            (None, None) => break,
            (Some(a), Some(b)) => peers.push(Peer { my_id: parse(&my, a)?, peer_id: parse(&peer, b)? }),
            _ => anyhow::bail!("пир {n}: нужны обе переменные {my} и {peer}"),
        }
    }
    if peers.is_empty() {
        let my = get("MY_ID").context("не задан ни MY_ID, ни PEER_1_MY_ID")?;
        let peer = get("PEER_ID").context("не задан ни PEER_ID, ни PEER_1_PEER_ID")?;
        peers.push(Peer { my_id: parse("MY_ID", my)?, peer_id: parse("PEER_ID", peer)? });
    }
    let mut mine = std::collections::HashSet::new();
    for peer in &peers {
        anyhow::ensure!(mine.insert(peer.my_id), "GUID сервера {} повторяется: у каждого пира он должен быть свой", peer.my_id);
    }
    Ok(peers)
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
    let discoveries = vec![discovery; common.peers.len()];
    tokio::runtime::Runtime::new()?.block_on(serve(discoveries, common))
}

/// Сервер на TUN: поднимает интерфейс и по набору дыр на каждого пира (`discoveries[i]` — способ
/// встречи с `common.peers[i]`), раздаёт адреса и гоняет мост TUN ↔ дыры, пока жив процесс.
pub async fn serve(discoveries: Vec<Discovery>, common: Common) -> Result<()> {
    anyhow::ensure!(discoveries.len() == common.peers.len(), "способов встречи {} на {} пиров", discoveries.len(), common.peers.len());
    let (server_addr, prefix) = common.tun.address.context("у TUN сервера нет адреса")?;
    let book = Arc::new(std::sync::Mutex::new(AddressBook::load(server_addr, prefix, common.address_file.clone())?));
    let tun = hp_tun::Tun::create(&common.tun).context("создание TUN (нужны права root)")?;
    log::info!(
        "сервер: TUN {} {server_addr}/{prefix}, пиров {}, порядок пакетов: ожидание {} мс, дыр для данных: {}",
        tun.name(),
        common.peers.len(),
        common.reorder_wait.as_millis(),
        if common.data_holes == 0 { "все".to_string() } else { common.data_holes.to_string() }
    );
    log::info!("сервер: DNS для клиентов {:?}", common.dns);
    let hub = Arc::new(hp_tun::hub::Hub::start(tun));
    let dns: Arc<Vec<Vec<u8>>> = Arc::new(common.dns.iter().map(|a| a.octets().to_vec()).collect());
    let options = MultiLinkOptions { reorder_wait: common.reorder_wait, data_holes: common.data_holes, ..MultiLinkOptions::default() };
    let mut links = Vec::new();
    for (peer, discovery) in common.peers.iter().zip(discoveries) {
        log::info!("сервер: я {} ищу пира {}", peer.my_id, peer.peer_id);
        let label = if common.peers.len() > 1 { peer_name(&peer.peer_id) } else { String::new() };
        let (link, incoming) = MultiLink::start_discovery(&label, discovery, peer.my_id, peer.peer_id, options).await?;
        let link = Arc::new(link);
        hub.add_link(&link, incoming);
        let control = link.take_control().expect("приёмник служебных сообщений забираем один раз");
        tokio::spawn(serve_addresses(link.clone(), control, hub.clone(), book.clone(), dns.clone()));
        links.push(link);
    }
    let mut last = None;
    loop {
        tokio::time::sleep(STATUS_INTERVAL).await;
        let holes: Vec<usize> = links.iter().map(|l| l.live_count()).collect();
        let now = (holes, hub.stats().snapshot());
        if Some(&now) != last.as_ref() {
            let (to, ordered, from, dropped) = now.1;
            let holes: Vec<String> = now.0.iter().map(|n| format!("{n}/{TARGET_LINKS}")).collect();
            log::info!("дыры {}, к пирам {to} (TCP с номером {ordered}), от пиров {from}, потеряно {dropped}", holes.join(" "));
            last = Some(now);
        }
    }
}

/// Отвечает на запросы адреса пира `link` (и клиентов за ним): выдаёт адрес из книги и
/// закрепляет его в мосту.
async fn serve_addresses(
    link: Arc<MultiLink>,
    mut control: mpsc::Receiver<Control>,
    hub: Arc<hp_tun::hub::Hub>,
    book: Arc<std::sync::Mutex<AddressBook>>,
    dns: Arc<Vec<Vec<u8>>>,
) {
    while let Some(message) = control.recv().await {
        let Control::AddressRequest(request) = message else { continue };
        let Ok(client) = request.client_id.map(u8::try_from).transpose() else {
            log::warn!("запрос адреса с client_id {:?} вне 0..=255", request.client_id);
            continue;
        };
        let kind = AddressKind::try_from(request.kind).unwrap_or(AddressKind::Host);
        let key = client_key(&peer_name(&link.peer_id()), client);
        let assigned = {
            let mut book = book.lock().unwrap();
            book.get_or_assign(&key, kind).map(|address| (address, book.prefix()))
        };
        match assigned {
            Ok((address, prefix)) => {
                hub.assign(address, &link, client);
                let reply = AddressAssign {
                    address: address.octets().to_vec(),
                    prefix: u32::from(prefix),
                    client_id: request.client_id,
                    dns: dns.as_ref().clone(),
                };
                let _ = link.send_control(Control::AddressAssign(reply)).await;
            }
            Err(e) => log::warn!("адрес для {key}: {e:#}"),
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

    fn getter(vars: &[(&str, String)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: std::collections::HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn one_peer_or_a_numbered_list() {
        let (a, b, c, d) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let single = parse_peers(&getter(&[("MY_ID", a.to_string()), ("PEER_ID", b.to_string())])).unwrap();
        assert_eq!(single, vec![Peer { my_id: a, peer_id: b }]);
        let list = parse_peers(&getter(&[
            ("PEER_1_MY_ID", a.to_string()),
            ("PEER_1_PEER_ID", b.to_string()),
            ("PEER_2_MY_ID", c.to_string()),
            ("PEER_2_PEER_ID", d.to_string()),
        ]))
        .unwrap();
        assert_eq!(list, vec![Peer { my_id: a, peer_id: b }, Peer { my_id: c, peer_id: d }]);
        let repeated = getter(&[("PEER_1_MY_ID", a.to_string()), ("PEER_1_PEER_ID", b.to_string()), ("PEER_2_MY_ID", a.to_string()), ("PEER_2_PEER_ID", d.to_string())]);
        assert!(parse_peers(&repeated).is_err(), "GUID сервера у пиров должен различаться");
        assert!(parse_peers(&getter(&[])).is_err());
    }
}
