//! Замер пропускной способности MultiLink без WireGuard: одна сторона шлёт через
//! `send_data` пакеты фиксированного размера с заданной скоростью, другая считает, что
//! пришло, сколько потерялось и сколько пришло не по порядку.
//!
//! `mlbench <stun> <mqtt> <mqtt_ca> <my_id> <peer_id> recv <секунд>`
//! `mlbench <stun> <mqtt> <mqtt_ca> <my_id> <peer_id> send <секунд> <Мбит/с>[,<Мбит/с>...]`
//! Окружение: `HOLE_PORT_BASE` (первый локальный порт слотов, 0 — любые), `DATA_HOLES`
//! (0 — все дыры), `PAYLOAD` (байт в пакете, по умолчанию 1392 — как пакет WireGuard при MTU 1360),
//! `MIN_HOLES` (сколько живых дыр ждать перед отправкой, по умолчанию 10).

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use connection::multilink::{MultiLink, MultiLinkOptions};
use uuid::Uuid;

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<()> {
    hp_logging::init()?;
    let a: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(a.len() >= 7, "использование: mlbench <stun> <mqtt> <ca> <my_id> <peer_id> recv|send <секунд> [Мбит/с,...]");
    let options = MultiLinkOptions {
        reorder_wait: Duration::ZERO,
        data_holes: env_num("DATA_HOLES", 0u8),
        local_port_base: env_num("HOLE_PORT_BASE", 0u16),
    };
    let (link, mut incoming) = MultiLink::start_with(
        "",
        connection::stun::parse_servers(&a[0])?,
        a[1].parse::<SocketAddr>().context("MQTT")?,
        std::fs::read(&a[2]).context("CA")?,
        a[3].parse::<Uuid>()?,
        a[4].parse::<Uuid>()?,
        options,
    )
    .await?;
    let secs: u64 = a[6].parse().context("секунд")?;
    let payload_len: usize = env_num("PAYLOAD", 1392usize).max(16);

    if a[5] == "recv" {
        // Отчёт по каждой «пачке» (отправитель делает паузу между скоростями): пачка
        // кончается, когда 1,5 с нет пакетов. Отставание переставленного пакета — время от
        // обнаружения пробела (пришёл номер больше) до прихода пропущенного: столько его
        // пришлось бы ждать буферу порядка.
        let deadline = Instant::now() + Duration::from_secs(secs);
        let gap = Duration::from_millis(1500);
        let mut burst: Option<Burst> = None;
        loop {
            let wait = if burst.is_some() { gap } else { deadline.saturating_duration_since(Instant::now()) };
            match tokio::time::timeout(wait, incoming.recv()).await {
                Ok(Some(p)) => {
                    if p.payload.len() < 16 || &p.payload[..4] != b"MLBN" { continue; }
                    let seq = u64::from_le_bytes(p.payload[8..16].try_into().unwrap());
                    burst.get_or_insert_with(|| Burst::new(seq)).add(seq, p.payload.len());
                }
                Ok(None) => break,
                Err(_) => match burst.take() { Some(b) => b.report(), None => break },
            }
            if Instant::now() > deadline { if let Some(b) = burst.take() { b.report(); } break; }
        }
    } else {
        let rates: Vec<f64> = a.get(7).context("скорость, Мбит/с")?.split(',').map(|r| r.parse().unwrap()).collect();
        let min_holes: usize = env_num("MIN_HOLES", 10usize);
        let wait_until = Instant::now() + Duration::from_secs(120);
        while link.live_count() < min_holes && Instant::now() < wait_until { tokio::time::sleep(Duration::from_millis(500)).await; }
        println!("живых дыр перед отправкой: {}", link.live_count());
        tokio::time::sleep(Duration::from_secs(3)).await;
        let mut seq = 0u64;
        for rate in rates {
            let pps = rate * 1e6 / 8.0 / payload_len as f64;
            let start = Instant::now();
            let (mut sent, mut failed) = (0u64, 0u64);
            while start.elapsed() < Duration::from_secs(secs) {
                let due = (start.elapsed().as_secs_f64() * pps) as u64;
                while sent + failed < due {
                    let mut p = vec![0u8; payload_len];
                    p[..4].copy_from_slice(b"MLBN");
                    p[8..16].copy_from_slice(&seq.to_le_bytes());
                    seq += 1;
                    if link.send_data(p).await.is_ok() { sent += 1 } else { failed += 1 }
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            println!("отправка {rate} Мбит/с: отправлено {sent} пакетов ({:.1} Мбит/с), не отправлено {failed}",
                sent as f64 * payload_len as f64 * 8.0 / start.elapsed().as_secs_f64() / 1e6);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(())
}

struct Burst {
    first: u64,
    max: u64,
    count: u64,
    bytes: u64,
    ooo: u64,
    t0: Instant,
    t1: Instant,
    /// Пропущенные номера и момент, когда заметили пробел.
    missing: std::collections::HashMap<u64, Instant>,
    lateness: Vec<Duration>,
}

impl Burst {
    fn new(seq: u64) -> Self {
        let now = Instant::now();
        Self { first: seq, max: seq, count: 0, bytes: 0, ooo: 0, t0: now, t1: now, missing: Default::default(), lateness: Vec::new() }
    }

    fn add(&mut self, seq: u64, len: usize) {
        let now = Instant::now();
        if self.count > 0 && seq > self.max + 1 {
            for gap in self.max + 1..seq {
                self.missing.insert(gap, now);
            }
        }
        if seq < self.max || (self.count > 0 && seq == self.max) {
            self.ooo += 1;
            if let Some(noticed) = self.missing.remove(&seq) {
                self.lateness.push(now - noticed);
            }
        } else {
            self.max = seq;
        }
        self.first = self.first.min(seq);
        self.count += 1;
        self.bytes += len as u64;
        self.t1 = now;
    }

    fn report(mut self) {
        let dur = self.t1.duration_since(self.t0).as_secs_f64().max(0.001);
        let sent = self.max - self.first + 1;
        self.lateness.sort_unstable();
        let pct = |q: f64| -> f64 {
            if self.lateness.is_empty() { return 0.0; }
            let i = ((self.lateness.len() - 1) as f64 * q).round() as usize;
            self.lateness[i].as_secs_f64() * 1000.0
        };
        println!(
            "ИТОГ пачки: {:.1} Мбит/с за {dur:.1} с, {} из ≥{sent}, потеряно {:.1}%, не по порядку {:.1}% | отставание, мс: медиана {:.2}, p90 {:.2}, p99 {:.2}, макс {:.2} (n={})",
            self.bytes as f64 * 8.0 / dur / 1e6,
            self.count,
            100.0 * sent.saturating_sub(self.count) as f64 / sent as f64,
            100.0 * self.ooo as f64 / self.count.max(1) as f64,
            pct(0.5), pct(0.9), pct(0.99), pct(1.0), self.lateness.len()
        );
    }
}
