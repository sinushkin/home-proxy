//! hpctl — консольный клиент протокола управления (без графики: сервер, роутер, скрипты).
//!
//!   hpctl --connect <строка подключения> status
//!   hpctl … pair            — ссылка сопряжения нового телефона (секрет! для QR: qrencode -t ansiutf8)
//!   hpctl … remove <имя>    — удалить сопряжённый телефон
//!
//! Строка подключения — `--connect` или переменная `HP_CONTROL`. На той же машине, что и
//! служба, можно вместо неё указать ключ службы: `--addr ip:порт --key-file control.key`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use hp_control::proto::control_message::Body;
use hp_control::proto::{CreatePairing, GetStatus, RemovePeer};
use hp_control::Client;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn usage() -> ! {
    eprintln!("использование: hpctl [--connect строка | --addr ip:порт --key-file путь] status | pair | remove <имя>");
    std::process::exit(2);
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut connect = std::env::var("HP_CONTROL").ok();
    let mut addr = hp_control::DEFAULT_ADDR.to_string();
    let mut key_file: Option<PathBuf> = None;
    let mut command = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--connect" => connect = Some(args.next().unwrap_or_else(|| usage())),
            "--addr" => addr = args.next().unwrap_or_else(|| usage()),
            "--key-file" => key_file = Some(args.next().unwrap_or_else(|| usage()).into()),
            _ => command.push(arg),
        }
    }
    let (mut client, welcome) = match (key_file, connect) {
        (Some(path), _) => {
            let key = std::fs::read_to_string(&path).with_context(|| format!("не удалось прочитать ключ {}", path.display()))?;
            Client::connect(addr.parse().context("--addr: ожидается ip:порт")?, key.trim()).await?
        }
        (None, Some(text)) => Client::connect_string(&text).await?,
        (None, None) => usage(),
    };
    match command.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["status"] => {
            let Body::Status(s) = client.request(Body::GetStatus(GetStatus {})).await? else { anyhow::bail!("ждали Status") };
            println!("{} {} · режим {} · дыры через {} · работает {} с", welcome.service, welcome.build, s.mode, if s.bind.is_empty() { "любой адрес" } else { &s.bind }, s.uptime_s);
            if let Some(t) = s.traffic {
                println!("пакетов к пирам {} (TCP по порядку {}), от пиров {}, отброшено {}", t.to_peers, t.ordered, t.from_peers, t.dropped);
            }
            for p in s.peers {
                let pending = if p.pending { " (ждёт первого подключения)" } else { "" };
                let reorder = if p.reorder_wait_ms == 0 { "выкл".to_string() } else { format!("{} мс", p.reorder_wait_ms) };
                println!("{} {}{pending}: {}/{} дыр, адреса [{}], порядок {reorder}", p.name, p.state, p.live, p.target, p.addresses.join(", "));
                if let Some(text) = hp_control::registration_text(&p, now_unix_ms()) {
                    println!("  {text}");
                }
                for h in p.holes {
                    let loss = |l: f32| if l < 0.0 { "—".to_string() } else { format!("{:.1}%", l * 100.0) };
                    let state = if h.draining { ", сливается" } else { "" };
                    println!(
                        "  #{} {} возраст {}{state}, отправлено {} получено {} потери ↑{} ↓{}",
                        h.slot,
                        h.peer_addr,
                        hp_control::age_text(h.age_secs),
                        h.sent,
                        h.received,
                        loss(h.loss_out),
                        loss(h.loss_in)
                    );
                }
            }
        }
        ["pair"] => {
            let Body::Pairing(p) = client.request(Body::CreatePairing(CreatePairing {})).await? else { anyhow::bail!("ждали Pairing") };
            eprintln!("телефон {} — ссылка ниже секретна (полные GUID пары), не публикуйте её", p.name);
            println!("{}", p.uri);
        }
        ["remove", name] => {
            client.request(Body::RemovePeer(RemovePeer { name: name.to_string() })).await?;
            println!("удалён {name}");
        }
        _ => usage(),
    }
    Ok(())
}
