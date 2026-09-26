//! Проверка TUN на живой машине (роутер OpenWrt, WSL2, Linux): поднимает интерфейс, отвечает на
//! ping адресов подсети (кроме своего) и раз в секунду печатает, какие пакеты пришли.
//!
//! `hp-tun-check <имя> <адрес/префикс> [секунд]`, например `hp-tun-check hp0 10.79.0.1/24 30`,
//! затем с этой же машины `ping 10.79.0.2`. Нужны права root (`CAP_NET_ADMIN`).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hp_tun::packet::{self, Proto};
use hp_tun::{Tun, TunConfig};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() >= 2, "использование: hp-tun-check <имя> <адрес/префикс> [секунд]");
    let (ip, prefix) = args[1].split_once('/').context("адрес: ip/префикс")?;
    let secs: u64 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(30);
    let config = TunConfig { name: args[0].clone(), address: Some((ip.parse()?, prefix.parse()?)), mtu: Some(1400), up: true };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async move {
        let tun = Tun::create(&config).context("создание TUN")?;
        println!("интерфейс {} поднят: {}/{prefix}, MTU 1400", tun.name(), ip);
        let (mut buf, mut out) = ([0u8; 1500], [0u8; 1500]);
        let deadline = Instant::now() + Duration::from_secs(secs);
        let (mut tcp, mut udp, mut icmp, mut other, mut replied) = (0u32, 0u32, 0u32, 0u32, 0u32);
        let mut tick = Instant::now();
        while Instant::now() < deadline {
            let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(500), tun.recv(&mut buf)).await else { continue };
            match packet::inspect(&buf[..n]).map(|i| i.proto) {
                Some(Proto::Tcp) => tcp += 1,
                Some(Proto::Udp) => udp += 1,
                Some(Proto::Icmp) => icmp += 1,
                _ => other += 1,
            }
            if let Some(len) = packet::icmp_echo_reply(&buf[..n], &mut out) {
                tun.send(&out[..len]).await?;
                replied += 1;
            }
            if tick.elapsed() >= Duration::from_secs(1) {
                println!("TCP {tcp}, UDP {udp}, ICMP {icmp}, прочих {other}; ответов на ping {replied}");
                tick = Instant::now();
            }
        }
        println!("итого: TCP {tcp}, UDP {udp}, ICMP {icmp}, прочих {other}; ответов на ping {replied}");
        Ok(())
    })
}
