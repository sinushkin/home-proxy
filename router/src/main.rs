//! Роутер-релей: держит набор из 10 дыр к серверу и по набору к каждому телефону
//! и пересылает пакеты между ними. От телефона `k` принимает обычную `Data`,
//! оборачивает в `WrappedData { client_id: k }` и шлёт серверу; от сервера
//! принимает `WrappedData`, по `client_id` находит телефон, разворачивает и шлёт
//! ему обычной `Data`. WireGuard на роутере не нужен.
//!
//! Настройки — переменные окружения (удобно через `run.sh` и `.env`):
//!   STUN_ADDR, MQTT_ADDR, MQTT_CA — общие для всех наборов (STUN_ADDR — один или
//!   несколько серверов через запятую);
//!   SERVER_MY_ID / SERVER_PEER_ID — GUID роутера и сервера (набор «server»);
//!   PHONE_<n>_MY_ID / PHONE_<n>_PEER_ID — GUID роутера и телефона n для набора
//!   этого телефона, n = 1, 2, 3 … подряд (максимум 255); `n` и есть `client_id`.
//! Все GUID роутера (`SERVER_MY_ID` и каждый `PHONE_<n>_MY_ID`) должны быть
//! разными: брокер различает клиентов по `client_id` (= наш GUID), одинаковые
//! вытеснят друг друга, а слоты одного GUID затирали бы друг друга.
//!
//! Логи: `RUST_LOG` (по умолчанию `info`), `LOG_TARGET=syslog` — в syslog.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use connection::multilink::{MultiLink, TARGET_LINKS};
use connection::relay::{DirectionStats, PlainOut, WrappedOut, forward, route_by_client};
use uuid::Uuid;

const STATUS_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq, Eq)]
struct Phone {
    /// Номер телефона (`n` из `PHONE_<n>_*`), он же `client_id` в `WrappedData`.
    client_id: u8,
    my_id: Uuid,
    peer_id: Uuid,
}

struct Config {
    stun_addrs: Vec<SocketAddr>,
    mqtt_addr: SocketAddr,
    mqtt_ca: String,
    server_my_id: Uuid,
    server_peer_id: Uuid,
    phones: Vec<Phone>,
}

impl Config {
    fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let need = |name: &str| get(name).with_context(|| format!("не задана переменная {name}"));
        let parse_id = |name: &str, value: String| -> Result<Uuid> {
            value.parse().with_context(|| format!("{name}: некорректный GUID"))
        };

        let mut phones = Vec::new();
        for client_id in 1..=u8::MAX {
            let my_name = format!("PHONE_{client_id}_MY_ID");
            let peer_name = format!("PHONE_{client_id}_PEER_ID");
            match (get(&my_name), get(&peer_name)) {
                (None, None) => break,
                (Some(my), Some(peer)) => phones.push(Phone {
                    client_id,
                    my_id: parse_id(&my_name, my)?,
                    peer_id: parse_id(&peer_name, peer)?,
                }),
                _ => anyhow::bail!("телефон {client_id}: нужны обе переменные {my_name} и {peer_name}"),
            }
        }
        anyhow::ensure!(
            !phones.is_empty(),
            "не задан ни один телефон: нужны PHONE_1_MY_ID и PHONE_1_PEER_ID"
        );

        let config = Self {
            stun_addrs: connection::stun::parse_servers(&need("STUN_ADDR")?).context("STUN_ADDR")?,
            mqtt_addr: need("MQTT_ADDR")?.parse().context("MQTT_ADDR: ожидается ip:порт")?,
            mqtt_ca: need("MQTT_CA")?,
            server_my_id: parse_id("SERVER_MY_ID", need("SERVER_MY_ID")?)?,
            server_peer_id: parse_id("SERVER_PEER_ID", need("SERVER_PEER_ID")?)?,
            phones,
        };

        let mut router_ids = HashSet::from([config.server_my_id]);
        for phone in &config.phones {
            anyhow::ensure!(
                router_ids.insert(phone.my_id),
                "GUID роутера {} встречается дважды: у каждого набора дыр он должен быть свой \
                 (брокер вытеснит клиента с тем же client_id)",
                phone.my_id
            );
        }
        let mut phone_ids = HashSet::new();
        for phone in &config.phones {
            anyhow::ensure!(
                phone_ids.insert(phone.peer_id),
                "GUID телефона {} указан дважды",
                phone.peer_id
            );
        }
        Ok(config)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    hp_logging::init()?;
    let config = Config::from_env(|name| std::env::var(name).ok())?;
    let ca_pem = std::fs::read(&config.mqtt_ca)
        .with_context(|| format!("не удалось прочитать CA-сертификат {}", config.mqtt_ca))?;

    log::info!(
        "роутер: сервер {} (я {}), телефонов {}, по {TARGET_LINKS} дыр в каждый набор",
        config.server_peer_id,
        config.server_my_id,
        config.phones.len()
    );

    let (server, server_rx) = MultiLink::start(
        "server",
        config.stun_addrs.clone(),
        config.mqtt_addr,
        ca_pem.clone(),
        config.server_my_id,
        config.server_peer_id,
    )
    .await?;
    let server = Arc::new(server);

    let up = Arc::new(DirectionStats::default());
    let down = Arc::new(DirectionStats::default());
    let mut phone_links: Vec<(u8, Arc<MultiLink>)> = Vec::new();
    let mut to_phones: HashMap<u8, PlainOut> = HashMap::new();
    for phone in &config.phones {
        let label = format!("phone{}", phone.client_id);
        log::info!("{label}: телефон {} (я {})", phone.peer_id, phone.my_id);
        let (link, phone_rx) = MultiLink::start(
            &label,
            config.stun_addrs.clone(),
            config.mqtt_addr,
            ca_pem.clone(),
            phone.my_id,
            phone.peer_id,
        )
        .await?;
        let link = Arc::new(link);
        let out = WrappedOut { link: server.clone(), client_id: phone.client_id };
        tokio::spawn(forward(format!("{label}->server"), phone_rx, out, up.clone()));
        to_phones.insert(phone.client_id, PlainOut(link.clone()));
        phone_links.push((phone.client_id, link));
    }
    tokio::spawn(route_by_client("server->phones", server_rx, to_phones, down.clone()));

    let mut status = tokio::time::interval(STATUS_INTERVAL);
    let mut last = String::new();
    loop {
        status.tick().await;
        let phones = phone_links
            .iter()
            .map(|(client_id, link)| format!("{client_id}:{}/{TARGET_LINKS}", link.live_count()))
            .collect::<Vec<_>>()
            .join(" ");
        let (up_ok, up_lost) = up.snapshot();
        let (down_ok, down_lost) = down.snapshot();
        let line = format!(
            "дыры: сервер {}/{TARGET_LINKS}, телефоны [{phones}]; телефоны->сервер {up_ok} \
             (потеряно {up_lost}), сервер->телефоны {down_ok} (потеряно {down_lost})",
            server.live_count()
        );
        // Строку пишем, только если что-то изменилось: на простое роутер молчит.
        if line != last {
            log::info!("{line}");
            last = line;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| map.get(name).cloned()
    }

    fn id(n: u8) -> String {
        Uuid::from_bytes([n; 16]).to_string()
    }

    fn base(phone_count: u8) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("STUN_ADDR".to_string(), "127.0.0.1:3478".to_string()),
            ("MQTT_ADDR".to_string(), "127.0.0.1:8883".to_string()),
            ("MQTT_CA".to_string(), "ca.crt".to_string()),
            ("SERVER_MY_ID".to_string(), id(1)),
            ("SERVER_PEER_ID".to_string(), id(2)),
        ];
        for n in 1..=phone_count {
            pairs.push((format!("PHONE_{n}_MY_ID"), id(10 + n)));
            pairs.push((format!("PHONE_{n}_PEER_ID"), id(20 + n)));
        }
        pairs
    }

    fn parse(pairs: &[(String, String)]) -> Result<Config> {
        let refs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        Config::from_env(env(&refs))
    }

    #[test]
    fn several_phones_get_client_ids_from_their_numbers() {
        let config = parse(&base(3)).unwrap();
        let ids: Vec<u8> = config.phones.iter().map(|p| p.client_id).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert_eq!(config.phones[1].my_id.to_string(), id(12));
        assert_eq!(config.phones[2].peer_id.to_string(), id(23));
    }

    #[test]
    fn at_least_one_phone_is_required() {
        let error = parse(&base(0)).err().unwrap();
        assert!(format!("{error:#}").contains("PHONE_1_MY_ID"), "{error:#}");
    }

    #[test]
    fn half_specified_phone_is_reported() {
        let mut pairs = base(2);
        pairs.retain(|(k, _)| k != "PHONE_2_PEER_ID");
        let error = parse(&pairs).err().unwrap();
        assert!(format!("{error:#}").contains("PHONE_2_PEER_ID"), "{error:#}");
    }

    #[test]
    fn missing_common_variable_is_named_in_the_error() {
        let mut pairs = base(1);
        pairs.retain(|(k, _)| k != "SERVER_PEER_ID");
        let error = parse(&pairs).err().unwrap();
        assert!(format!("{error:#}").contains("SERVER_PEER_ID"), "{error:#}");
    }

    #[test]
    fn router_guid_reused_between_sets_is_rejected() {
        let mut pairs = base(2);
        pairs.iter_mut().find(|(k, _)| k == "PHONE_2_MY_ID").unwrap().1 = id(11);
        let error = parse(&pairs).err().unwrap();
        assert!(format!("{error:#}").contains("дважды"), "{error:#}");

        let mut pairs = base(1);
        pairs.iter_mut().find(|(k, _)| k == "PHONE_1_MY_ID").unwrap().1 = id(1);
        assert!(parse(&pairs).is_err(), "GUID сервера-набора не должен совпадать с GUID телефона-набора");
    }

    #[test]
    fn same_phone_guid_twice_is_rejected() {
        let mut pairs = base(2);
        pairs.iter_mut().find(|(k, _)| k == "PHONE_2_PEER_ID").unwrap().1 = id(21);
        let error = parse(&pairs).err().unwrap();
        assert!(format!("{error:#}").contains("указан дважды"), "{error:#}");
    }
}
