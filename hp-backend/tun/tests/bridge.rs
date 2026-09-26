//! Мост TUN ↔ дыры целиком, без прав: две стороны VPS-режима на loopback, вместо TUN — пары
//! сокетов `SOCK_SEQPACKET` (те же границы «один вызов = один пакет»).

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::net::SocketAddr;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use connection::multilink::{Discovery, MultiLink, MultiLinkOptions};
use hp_tun::bridge::Bridge;
use hp_tun::packet::{self, Proto};
use hp_tun::Tun;
use uuid::Uuid;

/// «TUN» для моста и второй конец, которым тест пишет и читает пакеты.
fn fake_tun() -> (Tun, Tun) {
    let mut fds = [0; 2];
    // SAFETY: создаём пару сокетов, дескрипторы проверяем.
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) }, 0);
    // SAFETY: владеем обоими дескрипторами.
    let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    (Tun::from_fd(a).unwrap(), Tun::from_fd(b).unwrap())
}

fn ip_packet(proto: u8, sport: u16, marker: u8) -> Vec<u8> {
    let mut out = [0u8; 1500];
    let n = packet::ipv4_udp(([10, 80, 0, 2], sport), ([1, 1, 1, 1], 443), &[marker; 100], &mut out).unwrap();
    let mut p = out[..n].to_vec();
    p[9] = proto;
    p
}

#[tokio::test]
async fn ip_packets_cross_the_holes_tcp_in_order() {
    let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
    let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let options = MultiLinkOptions::default();
    let (server, server_rx) = MultiLink::start_discovery(
        "",
        Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap_port, ports: 47400..=47599 },
        server_id,
        client_id,
        options,
    )
    .await
    .unwrap();
    let (client, client_rx) = MultiLink::start_discovery(
        "",
        Discovery::VpsClient { server: SocketAddr::from(([127, 0, 0, 1], bootstrap_port)) },
        client_id,
        server_id,
        options,
    )
    .await
    .unwrap();
    let (server, client) = (Arc::new(server), Arc::new(client));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while server.live_count() < 10 || client.live_count() < 10 {
        assert!(tokio::time::Instant::now() < deadline, "дыры не поднялись");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let (client_tun, client_app) = fake_tun();
    let (server_tun, server_app) = fake_tun();
    let _client_bridge = Bridge::start(client_tun, client.clone(), client_rx);
    let server_bridge = Bridge::start(server_tun, server.clone(), server_rx);

    // 30 TCP одного соединения и 10 UDP: у сервера TCP выходят строго по порядку.
    for i in 0..30u8 {
        client_app.send(&ip_packet(6, 5555, i)).await.unwrap();
        if i % 3 == 0 {
            client_app.send(&ip_packet(17, 6666, 200 + i / 3)).await.unwrap();
        }
    }
    let (mut tcp, mut udp) = (Vec::new(), 0);
    let mut buf = [0u8; 1500];
    while tcp.len() < 30 || udp < 10 {
        let n = tokio::time::timeout(Duration::from_secs(3), server_app.recv(&mut buf)).await.expect("пакет не дошёл").unwrap();
        let info = packet::inspect(&buf[..n]).unwrap();
        match info.proto {
            Proto::Tcp => tcp.push(buf[n - 1]),
            Proto::Udp => udp += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(tcp, (0..30u8).collect::<Vec<_>>(), "TCP по порядку");
    let (to_peer, ordered, from_peer, dropped) = server_bridge.stats().snapshot();
    assert_eq!((from_peer, dropped), (40, 0));
    assert_eq!((to_peer, ordered), (0, 0));

    // Обратно: ответ сервера доходит до клиента.
    server_app.send(&ip_packet(6, 443, 77)).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(3), client_app.recv(&mut buf)).await.unwrap().unwrap();
    assert_eq!(buf[n - 1], 77);
}
