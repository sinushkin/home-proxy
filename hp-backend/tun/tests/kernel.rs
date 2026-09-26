//! Настоящий TUN в ядре. Нужны права `CAP_NET_ADMIN`; без них тест пропускается. Без root:
//! `unshare -rn cargo test -p hp-tun` (своё сетевое пространство имён).

#![cfg(target_os = "linux")]

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use hp_tun::packet::{self, Proto};
use hp_tun::{Tun, TunConfig};
use tokio::net::UdpSocket;
use tokio::time::timeout;

fn create() -> Option<Tun> {
    let config = TunConfig { name: "hptest%d".into(), address: Some((Ipv4Addr::new(10, 99, 7, 1), 24)), mtu: Some(1400), up: true };
    match Tun::create(&config) {
        Ok(tun) => Some(tun),
        Err(e) => {
            eprintln!("пропуск: TUN не создать ({e}); запустите через `unshare -rn` или от root");
            None
        }
    }
}

#[tokio::test]
async fn packets_flow_between_the_kernel_and_the_tun() {
    let Some(tun) = create() else { return };
    assert!(tun.name().starts_with("hptest"), "{}", tun.name());

    // Ядро -> TUN: UDP на адрес из подсети туннеля уходит в интерфейс.
    let socket = UdpSocket::bind("10.99.7.1:0").await.unwrap();
    let local = socket.local_addr().unwrap();
    socket.send_to(b"to-tun", "10.99.7.2:9999").await.unwrap();
    let mut buf = [0u8; 1500];
    let packet = loop {
        let n = timeout(Duration::from_secs(2), tun.recv(&mut buf)).await.expect("пакет не пришёл в TUN").unwrap();
        let info = packet::inspect(&buf[..n]).unwrap();
        // Ядро может прислать и своё (IPv6 router solicitation и т.п.) — ждём наш UDP.
        if info.proto == Proto::Udp && info.ports == Some((local.port(), 9999)) {
            break buf[..n].to_vec();
        }
    };
    let info = packet::inspect(&packet).unwrap();
    assert_eq!(info.dst, std::net::IpAddr::V4(Ipv4Addr::new(10, 99, 7, 2)));
    assert_eq!(&packet[28..], b"to-tun");

    // TUN -> ядро: пакет, записанный в интерфейс, доходит до сокета.
    let mut out = [0u8; 1500];
    let n = packet::ipv4_udp(([10, 99, 7, 2], 9999), ([10, 99, 7, 1], local.port()), b"from-tun", &mut out).unwrap();
    tun.send(&out[..n]).await.unwrap();
    let mut rx = [0u8; 64];
    let (len, from) = timeout(Duration::from_secs(2), socket.recv_from(&mut rx)).await.expect("ядро не отдало пакет").unwrap();
    assert_eq!(&rx[..len], b"from-tun");
    assert_eq!(from, SocketAddr::from(([10, 99, 7, 2], 9999)));
}
