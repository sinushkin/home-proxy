//! CLI-обвязка над `connection`: поднимает менеджер набора дыр (`MultiLink`)
//! до заданного пира и держит его, логируя число живых дыр.
//!
//! Оба GUID оператор знает заранее (по ним пиры и находят друг друга) — здесь
//! их никто не генерирует и не выясняет.
//!
//! Использование: peer <stun_addr[,stun2_addr]> <mqtt_addr> <mqtt_ca> <my_peer_id> <peer_id>
//! (STUN-серверов можно указать несколько через запятую: `ip:порт,ip2:порт`).
//! (`mqtt_ca` — путь к PEM с CA-сертификатом MQTT-брокера; к брокеру ходим
//! только по TLS).
//!
//! Текст, набранный в терминале, по Enter уходит пиру по одной из живых дыр
//! (случайной, без повторов, пока не побывали все); сообщения пира печатаются
//! как `[#N] текст`, где N — номер дыры. С `WRAP=1` текст уходит обёрнутым
//! (`WrappedData` с номером клиента `CLIENT_ID`, по умолчанию 1), как
//! отправляет сервер роутеру; принятое обёрнутое печатается с номером клиента
//! и порядковым номером.
//!
//! Подробность логов задаётся через `RUST_LOG` (по умолчанию `info`), вывод —
//! в stderr или (`LOG_TARGET=syslog`) в syslog, см. крейт `hp-logging`.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt, BufReader};
use connection::holes::PoolPolicy;
use connection::multilink::{MultiLink, MultiLinkOptions, DEFAULT_HOLE_AGE, TARGET_LINKS};
use uuid::Uuid;

const STATUS_INTERVAL: Duration = Duration::from_secs(10);

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> anyhow::Result<T> {
    match std::env::var(name) {
        Ok(value) => value.trim().parse().map_err(|_| anyhow::anyhow!("{name}: некорректное число")),
        Err(_) => Ok(default),
    }
}

/// Диапазон секунд `мин-макс` (например `60-180`) или одно число.
fn env_age_range(name: &str, default: (Duration, Duration)) -> anyhow::Result<(Duration, Duration)> {
    let Ok(value) = std::env::var(name) else { return Ok(default) };
    let bad = || anyhow::anyhow!("{name}: ожидается «мин-макс» в секундах, например 60-180");
    let (min, max) = value.trim().split_once('-').unwrap_or((value.trim(), value.trim()));
    let (min, max): (u64, u64) = (min.trim().parse().map_err(|_| bad())?, max.trim().parse().map_err(|_| bad())?);
    anyhow::ensure!(min >= 1 && min <= max, bad());
    Ok((Duration::from_secs(min), Duration::from_secs(max)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    hp_logging::init()?;
    let wrap = std::env::var("WRAP").as_deref() == Ok("1");
    let client_id: u8 = match std::env::var("CLIENT_ID") {
        Ok(value) => value.parse().context("CLIENT_ID: ожидается число 0..=255")?,
        Err(_) => 1,
    };

    let args: Vec<String> = std::env::args().collect();
    let [_, stun_addr, mqtt_addr, mqtt_ca, my_peer_id, peer_id] = args.as_slice() else {
        eprintln!("usage: peer <stun_addr[,stun2_addr]> <mqtt_addr> <mqtt_ca> <my_peer_id> <peer_id>");
        std::process::exit(2);
    };

    let stun_addrs = connection::stun::parse_servers(stun_addr)?;
    let mqtt_addr: SocketAddr = mqtt_addr.parse()?;
    let mqtt_ca_pem = std::fs::read(mqtt_ca)
        .with_context(|| format!("не удалось прочитать CA-сертификат {mqtt_ca}"))?;
    let my_peer_id: Uuid = my_peer_id.parse()?;
    let peer_id: Uuid = peer_id.parse()?;

    log::info!("my peer_id={my_peer_id} looking for peer_id={peer_id}, target {TARGET_LINKS} holes");

    // Набор дыр динамический; для проверок его можно ускорить: HOLES_MIN, HOLES_MAX, HOLE_AGE=«мин-макс» (с).
    let pool = PoolPolicy {
        min_active: env_number("HOLES_MIN", PoolPolicy::default().min_active)?,
        max_total: env_number("HOLES_MAX", PoolPolicy::default().max_total)?,
        ..PoolPolicy::default()
    };
    anyhow::ensure!(
        pool.min_active >= 1 && pool.min_active <= pool.max_total && pool.max_total <= TARGET_LINKS as usize,
        "HOLES_MIN должно быть от 1 до HOLES_MAX, а HOLES_MAX — не больше {TARGET_LINKS}"
    );
    let options = MultiLinkOptions { pool, hole_age: env_age_range("HOLE_AGE", DEFAULT_HOLE_AGE)?, ..MultiLinkOptions::default() };
    let (multilink, mut incoming) =
        MultiLink::start_with("", stun_addrs, mqtt_addr, mqtt_ca_pem, my_peer_id, peer_id, options).await?;

    log::info!("введите текст и нажмите Enter: он уйдёт пиру по одной из живых дыр");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    let mut status = tokio::time::interval(STATUS_INTERVAL);
    loop {
        tokio::select! {
            _ = status.tick() => {
                log::debug!(
                    "live holes: {}/{} (me={} peer={})",
                    multilink.live_count(),
                    TARGET_LINKS,
                    multilink.my_peer_id(),
                    multilink.peer_id()
                );
            }
            message = incoming.recv() => {
                let Some(message) = message else {
                    log::warn!("канал входящих закрыт, выходим");
                    return Ok(());
                };
                let text = String::from_utf8_lossy(&message.payload);
                match message.wrapped {
                    Some(info) => log::info!(
                        "[#{}] (обёрнуто, клиент {}, seq={}) {text}",
                        message.slot,
                        info.client_id,
                        info.seq
                    ),
                    None => log::info!("[#{}] {text}", message.slot),
                }
            }
            line = lines.next_line(), if stdin_open => match line {
                Ok(Some(line)) => {
                    let text = line.trim_end();
                    if text.is_empty() {
                        continue;
                    }
                    let payload = text.as_bytes();
                    if wrap {
                        match multilink.send_client(client_id, None, payload).await {
                            Ok(slot) => log::info!("отправлено по дыре [#{slot}] (обёрнуто, клиент {client_id}): {text}"),
                            Err(e) => log::warn!("не отправлено: {e:#}"),
                        }
                    } else {
                        match multilink.send_data(payload).await {
                            Ok(slot) => log::info!("отправлено по дыре [#{slot}]: {text}"),
                            Err(e) => log::warn!("не отправлено: {e:#}"),
                        }
                    }
                }
                Ok(None) => {
                    log::debug!("stdin закрыт, ввод с клавиатуры отключён");
                    stdin_open = false;
                }
                Err(e) => {
                    log::warn!("ошибка чтения stdin: {e}; ввод отключён");
                    stdin_open = false;
                }
            },
        }
    }
}
