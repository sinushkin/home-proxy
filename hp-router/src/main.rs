//! `hp-router`: роутер OpenWrt с двумя ролями в одном процессе (один бинарник — меньше места на
//! флеше).
//!
//! 1. Шлюз дома: TUN (`hp0`) ↔ 10 дыр к VPS с белым IP (как `vps-client`). Весь трафик LAN и
//!    Wi-Fi уходит к VPS как есть, без WireGuard; TCP с номером в потоке, VPS возвращает порядок.
//! 2. Пир для телефонов: на каждый телефон свой набор из 10 P2P-дыр (STUN + MQTT, как у
//!    `hp-server`). Пакеты телефона роутер не разбирает и порядок им не восстанавливает — только
//!    перекладывает в дыры VPS как `WrappedData { client_id }` с номерами телефона; ответы VPS для
//!    этого `client_id` — обратно телефону. Порядок возвращает конечный получатель (VPS или
//!    телефон), TUN и стек ядра роутера пакеты телефонов не проходят.
//!
//! Запуск: `hp-router [--config router.env]` (без `--config` — `router.env` рядом с бинарником,
//! если есть, иначе только окружение). Настройки:
//!   VPS_SERVER — адрес VPS `ip[:порт знакомства]` (порт по умолчанию 40000);
//!   VPS_MY_ID / VPS_PEER_ID — GUID роутера и VPS-сервера;
//!   TUN_ADDR — адрес роутера в туннеле (`10.80.0.2/24`), TUN_NAME (`hp0`), TUN_MTU (1400);
//!   STUN_ADDR (один или несколько через запятую), MQTT_ADDR, MQTT_CA — для телефонов;
//!   PHONE_<n>_MY_ID / PHONE_<n>_PEER_ID — GUID роутера и телефона n, n = 1, 2, 3 … подряд
//!   (до 255); `n` и есть `client_id`. У каждого набора свой GUID роутера: брокер различает
//!   клиентов по нему. Телефонов может не быть — тогда только шлюз;
//!   REORDER_WAIT_MS, DATA_HOLES — как у `vps-client`; RUNTIME=multi — многопоточный tokio
//!   (по умолчанию однопоточный: на одноядерном роутере так быстрее);
//!   RUST_LOG, LOG_FILE — логи.
//! Сокеты дыр телефонов, STUN и MQTT должны ходить мимо `hp0` (напрямую через WAN): маршрут по
//! умолчанию в `hp0` — только для LAN (правило по источнику), см. `OpenWRT/Router.md`.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use connection::multilink::{Discovery, Incoming, MultiLink, MultiLinkOptions, DEFAULT_REORDER_WAIT, TARGET_LINKS};
use hp_server::settings::Settings;
use tokio::sync::mpsc;
use uuid::Uuid;

const STATUS_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
struct Phone {
    /// Номер телефона (`n` из `PHONE_<n>_*`), он же `client_id` в `WrappedData`.
    client_id: u8,
    my_id: Uuid,
    peer_id: Uuid,
}

/// Общее для наборов дыр к телефонам.
struct PhoneDiscovery {
    stun_addrs: Vec<SocketAddr>,
    mqtt_addr: SocketAddr,
    mqtt_ca: PathBuf,
}

struct Config {
    vps_server: SocketAddr,
    vps_my_id: Uuid,
    vps_peer_id: Uuid,
    tun_addr: (Ipv4Addr, u8),
    tun_name: String,
    tun_mtu: u16,
    reorder_wait: Duration,
    data_holes: u8,
    phones: Vec<Phone>,
    /// `None`, если телефонов нет.
    phone_discovery: Option<PhoneDiscovery>,
}

impl Config {
    fn from_settings(settings: &Settings) -> Result<Self> {
        let get = |name: &str| settings.get(name);
        let need = |name: &str| get(name).with_context(|| format!("не задана переменная {name}"));
        let parse_id = |name: &str| -> Result<Uuid> { need(name)?.trim().parse().with_context(|| format!("{name}: некорректный GUID")) };
        let number = |name: &str, default: u64| -> Result<u64> {
            get(name).map_or(Ok(default), |v| v.trim().parse().with_context(|| format!("{name}: ожидается число")))
        };

        let phones = parse_phones(&get)?;
        let phone_discovery = if phones.is_empty() {
            None
        } else {
            Some(PhoneDiscovery {
                stun_addrs: connection::stun::parse_servers(&need("STUN_ADDR")?).context("STUN_ADDR")?,
                mqtt_addr: need("MQTT_ADDR")?.trim().parse().context("MQTT_ADDR: ожидается ip:порт")?,
                mqtt_ca: settings.resolve(need("MQTT_CA")?.trim()),
            })
        };
        let config = Self {
            vps_server: connection::vps::parse_server(&need("VPS_SERVER")?).context("VPS_SERVER: ожидается ip или ip:порт")?,
            vps_my_id: parse_id("VPS_MY_ID")?,
            vps_peer_id: parse_id("VPS_PEER_ID")?,
            tun_addr: hp_tun::parse_cidr(&need("TUN_ADDR")?).context("TUN_ADDR: ожидается ip/префикс")?,
            tun_name: get("TUN_NAME").unwrap_or_else(|| "hp0".into()),
            tun_mtu: u16::try_from(number("TUN_MTU", 1400)?).context("TUN_MTU")?,
            reorder_wait: Duration::from_millis(number("REORDER_WAIT_MS", DEFAULT_REORDER_WAIT.as_millis() as u64)?),
            data_holes: u8::try_from(number("DATA_HOLES", 0)?).context("DATA_HOLES")?,
            phones,
            phone_discovery,
        };

        let mut router_ids = HashSet::from([config.vps_my_id]);
        for phone in &config.phones {
            anyhow::ensure!(
                router_ids.insert(phone.my_id),
                "GUID роутера {} встречается дважды: у каждого набора дыр он должен быть свой",
                phone.my_id
            );
        }
        Ok(config)
    }
}

/// `PHONE_<n>_MY_ID` / `PHONE_<n>_PEER_ID`, n = 1, 2, 3 … подряд.
fn parse_phones(get: &impl Fn(&str) -> Option<String>) -> Result<Vec<Phone>> {
    let mut phones = Vec::new();
    let mut phone_ids = HashSet::new();
    for client_id in 1..=u8::MAX {
        let my_name = format!("PHONE_{client_id}_MY_ID");
        let peer_name = format!("PHONE_{client_id}_PEER_ID");
        let (my, peer) = match (get(&my_name), get(&peer_name)) {
            (None, None) => break,
            (Some(my), Some(peer)) => (my, peer),
            _ => anyhow::bail!("телефон {client_id}: нужны обе переменные {my_name} и {peer_name}"),
        };
        let phone = Phone {
            client_id,
            my_id: my.trim().parse().with_context(|| format!("{my_name}: некорректный GUID"))?,
            peer_id: peer.trim().parse().with_context(|| format!("{peer_name}: некорректный GUID"))?,
        };
        anyhow::ensure!(phone_ids.insert(phone.peer_id), "GUID телефона {} указан дважды", phone.peer_id);
        phones.push(phone);
    }
    Ok(phones)
}

fn config_path() -> Result<Option<PathBuf>> {
    let mut args = std::env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (None, _) => {
            let default = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("router.env")));
            Ok(default.filter(|p| p.is_file()))
        }
        (Some("--config"), Some(path)) => Ok(Some(path.into())),
        _ => anyhow::bail!("использование: hp-router [--config router.env]"),
    }
}

fn main() -> Result<()> {
    let settings = Settings::load(config_path()?.as_deref())?;
    hp_server::init_logging(&settings)?;
    let config = Config::from_settings(&settings)?;
    let runtime = if settings.get("RUNTIME").as_deref() == Some("multi") {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    } else {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?
    };
    runtime.block_on(run(config))
}

/// Счётчики одного направления пересылки (32 бита: на MIPS32 64-битных атомиков нет).
#[derive(Default)]
struct DirectionStats {
    forwarded: std::sync::atomic::AtomicU32,
    dropped: std::sync::atomic::AtomicU32,
}

impl DirectionStats {
    fn snapshot(&self) -> (u32, u32) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.forwarded.load(Relaxed), self.dropped.load(Relaxed))
    }
}

/// Счётчики пересылки телефонов.
#[derive(Default)]
struct RelayStats {
    to_vps: DirectionStats,
    to_phones: DirectionStats,
}

async fn run(config: Config) -> Result<()> {
    let tun_config = hp_tun::TunConfig {
        name: config.tun_name.clone(),
        address: Some(config.tun_addr),
        mtu: Some(config.tun_mtu),
        up: true,
    };
    let tun = hp_tun::Tun::create(&tun_config).context("создание TUN (нужны права root)")?;
    log::info!(
        "hp-router: VPS {} (я {}), TUN {} {}/{}, телефонов {}",
        config.vps_server,
        config.vps_my_id,
        tun.name(),
        config.tun_addr.0,
        config.tun_addr.1,
        config.phones.len()
    );

    // Пакеты телефонов роутер перекладывает насквозь: порядок им вернёт VPS или сам телефон.
    let vps_options = MultiLinkOptions {
        reorder_wait: config.reorder_wait,
        data_holes: config.data_holes,
        reorder_clients: false,
        ..MultiLinkOptions::default()
    };
    let (vps, vps_rx) = MultiLink::start_discovery(
        "vps",
        Discovery::VpsClient { server: config.vps_server },
        config.vps_my_id,
        config.vps_peer_id,
        vps_options,
    )
    .await?;
    let vps = Arc::new(vps);
    let (to_phones_tx, to_phones_rx) = mpsc::channel::<Incoming>(256);
    let bridge = hp_tun::bridge::Bridge::start_relay(tun, vps.clone(), vps_rx, to_phones_tx);

    let stats = Arc::new(RelayStats::default());
    let mut phones = HashMap::new();
    if let Some(discovery) = &config.phone_discovery {
        let ca_pem = std::fs::read(&discovery.mqtt_ca)
            .with_context(|| format!("не удалось прочитать CA-сертификат {}", discovery.mqtt_ca.display()))?;
        let phone_options = MultiLinkOptions {
            reorder_wait: Duration::ZERO,
            data_holes: config.data_holes,
            ..MultiLinkOptions::default()
        };
        for phone in &config.phones {
            let (link, rx) = MultiLink::start_with(
                &format!("phone{}", phone.client_id),
                discovery.stun_addrs.clone(),
                discovery.mqtt_addr,
                ca_pem.clone(),
                phone.my_id,
                phone.peer_id,
                phone_options,
            )
            .await
            .with_context(|| format!("телефон {}", phone.client_id))?;
            let link = Arc::new(link);
            tokio::spawn(phone_to_vps(phone.client_id, rx, vps.clone(), stats.clone()));
            phones.insert(phone.client_id, link);
        }
    }
    let phones = Arc::new(phones);
    tokio::spawn(vps_to_phones(to_phones_rx, phones.clone(), stats.clone()));

    let mut last = None;
    loop {
        tokio::time::sleep(STATUS_INTERVAL).await;
        let phone_holes: Vec<(u8, usize)> = {
            let mut v: Vec<_> = phones.iter().map(|(id, link)| (*id, link.live_count())).collect();
            v.sort_unstable();
            v
        };
        let now = (vps.live_count(), bridge.stats().snapshot(), stats.to_vps.snapshot(), stats.to_phones.snapshot(), phone_holes);
        if Some(&now) != last.as_ref() {
            let (to, ordered, from, dropped) = now.1;
            let holes: Vec<String> = now.4.iter().map(|(id, n)| format!("{id}:{n}")).collect();
            log::info!(
                "VPS: дыры {}/{TARGET_LINKS}, из TUN {to} (TCP с номером {ordered}), в TUN {from}, потеряно {dropped}; \
                 телефоны (дыры {}): к VPS {}/{} потеряно, к телефонам {}/{} потеряно",
                now.0,
                if holes.is_empty() { "нет".to_string() } else { holes.join(" ") },
                now.2 .0,
                now.2 .1,
                now.3 .0,
                now.3 .1,
            );
            last = Some(now);
        }
    }
}

/// Телефон → VPS: пакет как пришёл, с номерами телефона (`Ordered`) или без (`Data`).
async fn phone_to_vps(client_id: u8, mut rx: mpsc::Receiver<Incoming>, vps: Arc<MultiLink>, stats: Arc<RelayStats>) {
    use std::sync::atomic::Ordering::Relaxed;
    while let Some(packet) = rx.recv().await {
        if packet.wrapped.is_some() {
            stats.to_vps.dropped.fetch_add(1, Relaxed);
            continue;
        }
        match vps.send_client(client_id, packet.order, &packet.payload).await {
            Ok(_) => stats.to_vps.forwarded.fetch_add(1, Relaxed),
            Err(e) => {
                log::trace!("телефон {client_id} -> VPS: {e:#}");
                stats.to_vps.dropped.fetch_add(1, Relaxed)
            }
        };
    }
    log::warn!("телефон {client_id}: канал входящих закрыт");
}

/// VPS → телефон по `client_id`: номера VPS сохраняются (`Ordered`), телефон вернёт порядок сам.
async fn vps_to_phones(mut rx: mpsc::Receiver<Incoming>, phones: Arc<HashMap<u8, Arc<MultiLink>>>, stats: Arc<RelayStats>) {
    use std::sync::atomic::Ordering::Relaxed;
    while let Some(packet) = rx.recv().await {
        let Some(client_id) = packet.wrapped.map(|w| w.client_id) else { continue };
        let Some(phone) = phones.get(&client_id) else {
            log::debug!("VPS прислал пакет неизвестному телефону {client_id}");
            stats.to_phones.dropped.fetch_add(1, Relaxed);
            continue;
        };
        let sent = match packet.order {
            Some((flow, seq)) => phone.send_ordered(flow, seq, &packet.payload).await,
            None => phone.send_data(&packet.payload).await,
        };
        match sent {
            Ok(_) => stats.to_phones.forwarded.fetch_add(1, Relaxed),
            Err(e) => {
                log::trace!("VPS -> телефон {client_id}: {e:#}");
                stats.to_phones.dropped.fetch_add(1, Relaxed)
            }
        };
    }
    log::warn!("ретрансляция телефонам остановлена");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn getter(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn phones_are_numbered_consecutively() {
        let (a, b, c, d) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let phones = parse_phones(&getter(&[
            ("PHONE_1_MY_ID", &a.to_string()),
            ("PHONE_1_PEER_ID", &b.to_string()),
            ("PHONE_2_MY_ID", &c.to_string()),
            ("PHONE_2_PEER_ID", &d.to_string()),
            ("PHONE_4_MY_ID", &a.to_string()),
        ]))
        .unwrap();
        assert_eq!(
            phones,
            vec![Phone { client_id: 1, my_id: a, peer_id: b }, Phone { client_id: 2, my_id: c, peer_id: d }],
            "после пропуска номера (3) дальше не читаем"
        );
        assert!(parse_phones(&getter(&[])).unwrap().is_empty(), "без телефонов — только шлюз");
    }

    #[test]
    fn half_configured_or_repeated_phones_are_rejected() {
        let (a, b) = (Uuid::new_v4().to_string(), Uuid::new_v4().to_string());
        assert!(parse_phones(&getter(&[("PHONE_1_MY_ID", &a)])).is_err());
        let repeated = getter(&[("PHONE_1_MY_ID", &a), ("PHONE_1_PEER_ID", &b), ("PHONE_2_MY_ID", &a), ("PHONE_2_PEER_ID", &b)]);
        assert!(parse_phones(&repeated).is_err(), "один телефон дважды");
    }
}
