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
use connection::multilink::{MultiLink, TARGET_LINKS};
use uuid::Uuid;

const STATUS_INTERVAL: Duration = Duration::from_secs(10);

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

    let (multilink, mut incoming) =
        MultiLink::start("", stun_addrs, mqtt_addr, mqtt_ca_pem, my_peer_id, peer_id).await?;

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
                    let payload = text.as_bytes().to_vec();
                    if wrap {
                        match multilink.send_wrapped(client_id, payload).await {
                            Ok((slot, seq)) => log::info!(
                                "отправлено по дыре [#{slot}] (обёрнуто, клиент {client_id}, seq={seq}): {text}"
                            ),
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
