//! Замер пропускной способности MultiLink (голые дыры, без TUN): одна сторона шлёт через
//! `send_data` пакеты фиксированного размера с заданной скоростью, другая считает, что
//! пришло, сколько потерялось и сколько пришло не по порядку.
//!
//! `mlbench <stun> <mqtt> <mqtt_ca> <my_id> <peer_id> recv <секунд>`
//! `mlbench <stun> <mqtt> <mqtt_ca> <my_id> <peer_id> send <секунд> <Мбит/с>[,<Мбит/с>...]`
//! Окружение: `HOLE_PORT_BASE` (первый локальный порт слотов, 0 — любые), `DATA_HOLES`
//! (0 — все дыры), `PAYLOAD` (байт в пакете, по умолчанию 1392 — как пакет WireGuard при MTU 1360, для сравнения с прежними замерами),
//! `MIN_HOLES` (сколько живых дыр ждать перед отправкой, по умолчанию 10), `RUNTIME=current`
//! (однопоточный tokio).
//!
//! VPS-режим (без STUN и MQTT; `<stun> <mqtt> <mqtt_ca>` тогда игнорируются, можно `-`):
//! `VPS_SERVER=ip:порт` — клиент; `VPS_PUBLIC_IP` (+ `VPS_BOOTSTRAP_PORT`, `VPS_PORTS=a-b`) — сервер.
//!
//! `mlbench codec [итераций]` — стоимость кодека на этой машине без сети: XOR, protobuf,
//! encode/decode пакета `Data` из `PAYLOAD` байт.
//!
//! «Голый» UDP без MultiLink и tokio (потолок ядра и сети):
//! `mlbench udp-send <порт> <секунд> <Мбит/с>[,...]` — ждёт приветствие и шлёт его отправителю;
//! `mlbench udp-recv <ip:порт> <секунд>` — стучится первым (открывает NAT) и принимает.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use connection::codec;
use connection::multilink::{Discovery, MultiLink, MultiLinkOptions};
use connection::proto::{lite, peer_message, Data, Lite, PeerMessage};
use uuid::Uuid;

fn env_num<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn discovery(a: &[String]) -> Result<Discovery> {
    if let Ok(server) = std::env::var("VPS_SERVER") {
        return Ok(Discovery::VpsClient { server: server.parse().context("VPS_SERVER: ip:порт")? });
    }
    if let Ok(ip) = std::env::var("VPS_PUBLIC_IP") {
        let ports = std::env::var("VPS_PORTS").unwrap_or_else(|_| "40001-49999".into());
        let (low, high) = ports.split_once('-').context("VPS_PORTS: a-b")?;
        return Ok(Discovery::VpsServer {
            public_ip: ip.parse().context("VPS_PUBLIC_IP")?,
            bootstrap_port: env_num("VPS_BOOTSTRAP_PORT", connection::vps::DEFAULT_BOOTSTRAP_PORT),
            ports: low.parse()?..=high.parse()?,
        });
    }
    Ok(Discovery::StunMqtt {
        stun_addrs: connection::stun::parse_servers(&a[0])?,
        mqtt_addr: a[1].parse::<SocketAddr>().context("MQTT")?,
        mqtt_ca_pem: std::fs::read(&a[2]).context("CA")?,
    })
}

/// Сколько стоит кодек на пакет: XOR префикса, подпись и её проверка, protobuf-кодирование и
/// разбор.
fn codec_bench(iterations: u32, payload_len: usize) {
    use connection::auth::{Opener, PairSecret, RecvKeys, AUTH_LEN};
    let pair = PairSecret::new(uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let (me, peer) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let (send, _) = pair.link_keys(me, peer);
    // Один и тот же пакет разбираем много раз — окно против повтора отключаем.
    let recv = || RecvKeys { xor: send.xor, opener: Opener::without_replay_window(&pair.link_key(me, peer)) };
    let key = send.xor;
    let message = PeerMessage {
        body: Some(peer_message::Body::Lite(Lite {
            slot: 3,
            payload: Some(lite::Payload::Data(Data { payload: vec![0xa5; payload_len] })),
        })),
    };
    let encoded = codec::encode(&message, &send);
    let per_op = |start: Instant| start.elapsed().as_nanos() as f64 / f64::from(iterations) / 1000.0;

    let start = Instant::now();
    let mut buf = encoded.clone();
    for _ in 0..iterations {
        connection::xor::xor_in_place(std::hint::black_box(&mut buf[..codec::MASKED_PREFIX]), &key);
    }
    let xor64 = per_op(start);
    let start = Instant::now();
    for _ in 0..iterations {
        connection::xor::xor_in_place(std::hint::black_box(&mut buf[..]), &key);
    }
    let xor_all = per_op(start);
    let start = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(codec::encode(std::hint::black_box(&message), &send));
    }
    let encode = per_op(start);
    let mut rx_keys = recv();
    let start = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(codec::decode(std::hint::black_box(encoded.clone()), &mut rx_keys).unwrap());
    }
    let decode = per_op(start);
    let mut sealed = encoded.clone();
    codec::mask(&mut sealed, &key);
    let message_len = sealed.len() - AUTH_LEN;
    let start = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(send.sealer.seal(std::hint::black_box(&mut buf[..]), message_len));
    }
    let seal = per_op(start);
    let start = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(rx_keys.opener.open(std::hint::black_box(&sealed)).unwrap());
    }
    let open = per_op(start);
    let start = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(std::hint::black_box(&encoded).clone());
    }
    let copy = per_op(start);
    println!(
        "кодек, пакет {} байт, мкс на пакет: XOR {} байт {xor64:.2}, XOR всего пакета {xor_all:.2}, encode {encode:.2}, decode {decode:.2} (в т.ч. копия буфера {copy:.2})",
        encoded.len(),
        codec::MASKED_PREFIX
    );
    println!("  подпись (ChaCha20-Poly1305, метка по первым 128 байт и длине): поставить {seal:.2}, проверить {open:.2}");
    let payload = vec![0xa5u8; payload_len];
    let mut out = [0u8; connection::pool::PACKET_CAP];
    let start = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(connection::wire::encode_data(3, std::hint::black_box(&payload), &send, &mut out));
    }
    let fast_encode = per_op(start);
    let n = connection::wire::encode_data(3, &payload, &send, &mut out).unwrap();
    let start = Instant::now();
    for _ in 0..iterations {
        let mut rx = out;
        codec::mask(&mut rx[..n], &key);
        let Some(len) = rx_keys.opener.open(&rx[..n]) else { continue };
        if let Some(connection::wire::Fast::Data { payload, .. }) = connection::wire::parse(&rx[AUTH_LEN..AUTH_LEN + len]) {
            std::hint::black_box(connection::pool::Packet::copy_from(payload));
        }
    }
    let fast_decode = per_op(start);
    println!(
        "  быстрый путь (wire + банк буферов): encode {fast_encode:.2}, decode {fast_decode:.2} (в decode входит копия 1500 байт стекового буфера для теста)"
    );
    println!(
        "  это потолок только кодека: encode+decode ≈ {:.0} пакетов/с ≈ {:.0} Мбит/с",
        1e6 / (encode + decode),
        1e6 / (encode + decode) * payload_len as f64 * 8.0 / 1e6
    );
}

/// `RUNTIME=current` — однопоточный tokio (на одноядерном роутере нет пробуждений между
/// потоками), иначе многопоточный по умолчанию.
fn main() -> Result<()> {
    let runtime = if std::env::var("RUNTIME").as_deref() == Ok("current") {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?
    } else {
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?
    };
    runtime.block_on(run())
}

async fn run() -> Result<()> {
    hp_logging::init()?;
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        Some("udp-send") => return udp_send(&a[1..]),
        Some("udp-recv") => return udp_recv(&a[1..]),
        _ => {}
    }
    if a.first().map(String::as_str) == Some("codec") {
        let iterations = a.get(1).and_then(|n| n.parse().ok()).unwrap_or(20_000);
        codec_bench(iterations, env_num("PAYLOAD", 1392usize));
        return Ok(());
    }
    anyhow::ensure!(a.len() >= 7, "использование: mlbench <stun> <mqtt> <ca> <my_id> <peer_id> recv|send <секунд> [Мбит/с,...]");
    let options = MultiLinkOptions {
        reorder_wait: Duration::ZERO,
        data_holes: env_num("DATA_HOLES", 0u8),
        local_port_base: env_num("HOLE_PORT_BASE", 0u16),
        ..MultiLinkOptions::default()
    };
    let (link, mut incoming) =
        MultiLink::start_discovery("", discovery(&a)?, a[3].parse::<Uuid>()?, a[4].parse::<Uuid>()?, options).await?;
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
        let mut p = vec![0u8; payload_len];
        p[..4].copy_from_slice(b"MLBN");
        for rate in rates {
            let pps = rate * 1e6 / 8.0 / payload_len as f64;
            let start = Instant::now();
            let (mut sent, mut failed) = (0u64, 0u64);
            while start.elapsed() < Duration::from_secs(secs) {
                let due = (start.elapsed().as_secs_f64() * pps) as u64;
                while sent + failed < due {
                    p[8..16].copy_from_slice(&seq.to_le_bytes());
                    seq += 1;
                    if link.send_data(&p).await.is_ok() { sent += 1 } else { failed += 1 }
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

/// «Голый» UDP: ждёт приветствие получателя и шлёт ему пакеты с заданной скоростью.
fn udp_send(a: &[String]) -> Result<()> {
    anyhow::ensure!(a.len() == 3, "mlbench udp-send <порт> <секунд> <Мбит/с>[,...]");
    let socket = std::net::UdpSocket::bind(("0.0.0.0", a[0].parse::<u16>()?))?;
    let secs: u64 = a[1].parse()?;
    let payload_len: usize = env_num("PAYLOAD", 1403usize);
    let mut buf = [0u8; 64];
    let (_, peer) = socket.recv_from(&mut buf)?;
    println!("получатель {peer}");
    let mut packet = vec![0u8; payload_len];
    let mut seq = 0u64;
    for rate in a[2].split(',') {
        let rate: f64 = rate.parse()?;
        let pps = rate * 1e6 / 8.0 / payload_len as f64;
        let start = Instant::now();
        let mut sent = 0u64;
        while start.elapsed() < Duration::from_secs(secs) {
            let due = (start.elapsed().as_secs_f64() * pps) as u64;
            while sent < due {
                packet[..8].copy_from_slice(&seq.to_le_bytes());
                socket.send_to(&packet, peer)?;
                sent += 1;
                seq += 1;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        println!("отправка {rate} Мбит/с: {sent} пакетов");
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}

/// «Голый» UDP: стучится отправителю и считает принятое по секундам (без tokio).
fn udp_recv(a: &[String]) -> Result<()> {
    anyhow::ensure!(a.len() == 2, "mlbench udp-recv <ip:порт> <секунд>");
    let sender: SocketAddr = a[0].parse()?;
    let secs: u64 = a[1].parse()?;
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0))?;
    socket.set_read_timeout(Some(Duration::from_millis(1500)))?;
    for _ in 0..5 {
        socket.send_to(b"hello", sender)?;
    }
    let mut buf = vec![0u8; 2048];
    let deadline = Instant::now() + Duration::from_secs(secs);
    let (mut count, mut bytes, mut first_seq, mut max_seq, mut t0, mut t1) = (0u64, 0u64, u64::MAX, 0u64, None, Instant::now());
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) if n >= 8 => {
                let seq = u64::from_le_bytes(buf[..8].try_into().unwrap());
                let now = Instant::now();
                t0.get_or_insert(now);
                t1 = now;
                first_seq = first_seq.min(seq);
                max_seq = max_seq.max(seq);
                count += 1;
                bytes += n as u64;
            }
            Ok(_) => {}
            Err(_) => {
                if let Some(start) = t0.take() {
                    let dur = t1.duration_since(start).as_secs_f64().max(0.001);
                    let sent = max_seq - first_seq + 1;
                    println!(
                        "ИТОГ пачки UDP: {:.1} Мбит/с, {count} из {sent} пакетов, потеряно {:.1}%",
                        bytes as f64 * 8.0 / dur / 1e6,
                        100.0 * sent.saturating_sub(count) as f64 / sent as f64
                    );
                    (count, bytes, first_seq, max_seq) = (0, 0, u64::MAX, 0);
                }
            }
        }
    }
    Ok(())
}
