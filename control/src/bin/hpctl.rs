//! hpctl — консольный клиент протокола управления (без графики: сервер, роутер, скрипты).
//!
//!   hpctl --connect <строка подключения> status
//!   hpctl … events [N]      — последние события дыр по пирам (из памяти службы, по умолчанию 20)
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

fn hole_text(h: &hp_control::proto::HoleStatus) -> String {
    let loss = |l: f32| if l < 0.0 { "—".to_string() } else { format!("{:.1}%", l * 100.0) };
    let state = if h.draining { ", сливается" } else { "" };
    let via = if h.via.is_empty() { String::new() } else { format!(", через релей {}", h.via) };
    format!(
        "#{} {} возраст {}{state}{via}, отправлено {} получено {} потери ↑{} ↓{}",
        h.slot,
        h.peer_addr,
        hp_control::age_text(h.age_secs),
        h.sent,
        h.received,
        loss(h.loss_out),
        loss(h.loss_in)
    )
}

fn counters_text(c: &hp_control::proto::HoleCounters) -> String {
    format!(
        "с запуска: открыто {}, слито нами {}, слито пиром {}, потеряно {}, плохих {}, не подтверждено {}",
        c.opened, c.drained_by_us, c.drained_by_peer, c.lost, c.bad, c.not_confirmed
    )
}

fn event_text(kind: &str) -> &str {
    match kind {
        "opened" => "открыта",
        "lost" => "потеряна",
        "drained_by_us" => "сливаем",
        "drained_by_peer" => "сливает пир",
        "bad" => "плохая",
        "not_confirmed" => "не подтверждена пиром",
        other => other,
    }
}

/// Время события по часам этой машины, ЧЧ:ММ:СС UTC.
fn time_text(unix_ms: u64) -> String {
    let secs = unix_ms / 1000 % 86_400;
    format!("{:02}:{:02}:{:02}Z", secs / 3600, secs / 60 % 60, secs % 60)
}

fn usage() -> ! {
    eprintln!("использование: hpctl [--connect строка | --addr ip:порт --key-file путь] status | events [N] | pair | remove <имя>");
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
                if let Some(c) = &p.counters {
                    println!("  {}", counters_text(c));
                }
                for h in &p.holes {
                    println!("  {}", hole_text(h));
                }
            }
            for r in s.relays {
                let addr = if r.addr.is_empty() { "порт ещё не сообщил".to_string() } else { r.addr.clone() };
                let alive = if r.alive { "на связи" } else { "НЕ на связи" };
                println!("релей {} {alive}: {addr}, дыр к серверу {}", r.name, r.live);
                if let Some(c) = &r.counters {
                    println!("  {}", counters_text(c));
                }
                for h in &r.holes {
                    println!("  {}", hole_text(h));
                }
            }
        }
        ["events"] | ["events", _] => {
            let limit: usize = command.get(1).map_or(Ok(20), |n| n.parse()).context("events: ожидается число")?;
            let Body::Status(s) = client.request(Body::GetStatus(GetStatus {})).await? else { anyhow::bail!("ждали Status") };
            for p in s.peers {
                println!("{}:", p.name);
                let skip = p.events.len().saturating_sub(limit);
                for e in &p.events[skip..] {
                    let via = if e.via.is_empty() { String::new() } else { format!(" (через {})", e.via) };
                    println!("  {} дыра {} {}{via}", time_text(e.at_unix_ms), e.slot, event_text(&e.kind));
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
