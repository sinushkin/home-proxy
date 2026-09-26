//! Мосты TUN ↔ дыры целиком, без прав: стороны VPS-режима на loopback, вместо TUN — пары
//! сокетов `SOCK_SEQPACKET` (те же границы «один вызов = один пакет»). Сервер — `Hub` с выдачей
//! адресов (как у `hp-server`), клиент — `Bridge` с `request_address`.

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::Duration;

use connection::multilink::{Control, Discovery, Incoming, MultiLink, MultiLinkOptions};
use connection::proto::{AddressAssign, AddressKind, AddressRequest};
use hp_tun::bridge::{request_address, Bridge};
use hp_tun::hub::Hub;
use hp_tun::packet;
use hp_tun::Tun;
use uuid::Uuid;

type Rx = tokio::sync::mpsc::Receiver<Incoming>;

/// «TUN» для моста и второй конец, которым тест пишет и читает пакеты.
fn fake_tun() -> (Tun, Tun) {
    let mut fds = [0; 2];
    // SAFETY: создаём пару сокетов, дескрипторы проверяем.
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) }, 0);
    // SAFETY: владеем обоими дескрипторами.
    let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    (Tun::from_fd(a).unwrap(), Tun::from_fd(b).unwrap())
}

/// IP-пакет `proto` (6 — TCP, 17 — UDP) от `src` к `dst` с меткой в последнем байте.
fn ip_packet(proto: u8, src: [u8; 4], dst: [u8; 4], marker: u8) -> Vec<u8> {
    let mut out = [0u8; 1500];
    let n = packet::ipv4_udp((src, 40000), (dst, 443), &[marker; 100], &mut out).unwrap();
    let mut p = out[..n].to_vec();
    p[9] = proto;
    p
}

/// VPS-сервер и VPS-клиент на loopback с поднятыми 10 дырами.
async fn vps_pair(ports: std::ops::RangeInclusive<u16>, client_options: MultiLinkOptions) -> (Arc<MultiLink>, Rx, Arc<MultiLink>, Rx) {
    let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
    let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (server, server_rx) = MultiLink::start_discovery(
        "",
        Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap_port, ports },
        server_id,
        client_id,
        MultiLinkOptions::default(),
    )
    .await
    .unwrap();
    let (client, client_rx) = MultiLink::start_discovery(
        "",
        Discovery::VpsClient { server: SocketAddr::from(([127, 0, 0, 1], bootstrap_port)) },
        client_id,
        server_id,
        client_options,
    )
    .await
    .unwrap();
    let (server, client) = (Arc::new(server), Arc::new(client));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    while server.live_count() < 10 || client.live_count() < 10 {
        assert!(tokio::time::Instant::now() < deadline, "дыры не поднялись");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (server, server_rx, client, client_rx)
}

/// Сервер с `Hub` и простой выдачей адресов (как `hp-server`: хост — `10.80.0.2`, телефоны
/// — `10.80.1.<client_id>`).
fn serve(hub: Arc<Hub>, link: Arc<MultiLink>, incoming: Rx) {
    hub.add_link(&link, incoming);
    let mut control = link.take_control().unwrap();
    tokio::spawn(async move {
        while let Some(message) = control.recv().await {
            let Control::AddressRequest(request) = message else { continue };
            let client = request.client_id.map(|c| c as u8);
            let address = match client {
                None => Ipv4Addr::new(10, 80, 0, 2),
                Some(c) => Ipv4Addr::new(10, 80, 1, c),
            };
            hub.assign(address, &link, client);
            let reply = AddressAssign { address: address.octets().to_vec(), prefix: 16, client_id: request.client_id, dns: vec![vec![9, 9, 9, 9]] };
            let _ = link.send_control(Control::AddressAssign(reply)).await;
        }
    });
}

async fn next(app: &Tun) -> Vec<u8> {
    let mut buf = [0u8; 1500];
    let n = tokio::time::timeout(Duration::from_secs(3), app.recv(&mut buf)).await.expect("пакет не дошёл").unwrap();
    buf[..n].to_vec()
}

#[tokio::test]
async fn a_client_gets_an_address_and_its_packets_cross_the_holes_in_order() {
    let (server, server_rx, client, mut client_rx) = vps_pair(47400..=47599, MultiLinkOptions::default()).await;
    let (server_tun, server_app) = fake_tun();
    let hub = Arc::new(Hub::start(server_tun));
    serve(hub.clone(), server.clone(), server_rx);

    let control = client.take_control().unwrap();
    let (assigned, _control) =
        tokio::time::timeout(Duration::from_secs(5), request_address(&client, control, &mut client_rx, AddressKind::Host))
            .await
            .expect("адрес не выдан");
    assert_eq!((assigned.address, assigned.prefix), (Ipv4Addr::new(10, 80, 0, 2), 16));
    assert_eq!(assigned.dns, vec![Ipv4Addr::new(9, 9, 9, 9)]);
    let me = assigned.address.octets();

    let (client_tun, client_app) = fake_tun();
    let _client_bridge = Bridge::start(client_tun, client.clone(), client_rx);

    // 30 TCP одного соединения и 10 UDP: у сервера TCP выходят строго по порядку.
    for i in 0..30u8 {
        client_app.send(&ip_packet(6, me, [1, 1, 1, 1], i)).await.unwrap();
        if i % 3 == 0 {
            client_app.send(&ip_packet(17, me, [8, 8, 8, 8], 200 + i / 3)).await.unwrap();
        }
    }
    let (mut tcp, mut udp) = (Vec::new(), 0);
    while tcp.len() < 30 || udp < 10 {
        let p = next(&server_app).await;
        match p[9] {
            6 => tcp.push(*p.last().unwrap()),
            17 => udp += 1,
            other => panic!("протокол {other}"),
        }
    }
    assert_eq!(tcp, (0..30u8).collect::<Vec<_>>(), "TCP по порядку");

    // Чужой адрес источника не проходит.
    client_app.send(&ip_packet(17, [10, 80, 0, 99], [8, 8, 8, 8], 1)).await.unwrap();
    // Ответ сервера доходит до клиента.
    server_app.send(&ip_packet(6, [1, 1, 1, 1], me, 77)).await.unwrap();
    assert_eq!(*next(&client_app).await.last().unwrap(), 77);
    let mut buf = [0u8; 1500];
    assert!(tokio::time::timeout(Duration::from_millis(300), server_app.recv(&mut buf)).await.is_err(), "пакет с чужим адресом отброшен");
}

/// Роутер перекладывает на VPS запрос адреса телефона и его пакеты (с номерами телефона): VPS
/// выдаёт адрес, восстанавливает порядок, пишет пакеты в TUN и отвечает телефону через роутер.
#[tokio::test]
async fn a_phone_behind_the_router_gets_an_address_and_talks_through_the_vps() {
    let router_options = MultiLinkOptions { reorder_clients: false, ..MultiLinkOptions::default() };
    let (vps, vps_rx, router, mut router_rx) = vps_pair(47600..=47799, router_options).await;
    let (vps_tun, vps_app) = fake_tun();
    let hub = Arc::new(Hub::start(vps_tun));
    serve(hub.clone(), vps.clone(), vps_rx);

    // Запрос телефона 7, пересланный роутером.
    let mut control = router.take_control().unwrap();
    let request = Control::AddressRequest(AddressRequest { kind: AddressKind::Phone as i32, client_id: Some(7) });
    let assign = loop {
        router.send_control(request.clone()).await.unwrap();
        match tokio::time::timeout(Duration::from_millis(500), control.recv()).await {
            Ok(Some(Control::AddressAssign(assign))) => break assign,
            _ => continue,
        }
    };
    assert_eq!(assign.client_id, Some(7));
    assert_eq!(assign.address, vec![10, 80, 1, 7]);
    let phone = [10, 80, 1, 7];

    // Телефон пронумеровал 20 TCP-пакетов одной корзины; по дыре телефон → роутер они пришли
    // вразнобой, роутер отдаёт их VPS как есть.
    let mut order: Vec<u8> = (0..20).collect();
    order.swap(2, 5);
    order.swap(11, 17);
    for i in order {
        router.send_client(7, Some((4, u64::from(i))), &ip_packet(6, phone, [1, 1, 1, 1], i)).await.unwrap();
    }
    let mut got = Vec::new();
    while got.len() < 20 {
        got.push(*next(&vps_app).await.last().unwrap());
    }
    assert_eq!(got, (0..20u8).collect::<Vec<_>>(), "VPS вернул порядок пакетов телефона");

    // Ответ на адрес телефона уходит роутеру обёрнутым для клиента 7, с номером VPS.
    vps_app.send(&ip_packet(6, [1, 1, 1, 1], phone, 99)).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(3), router_rx.recv()).await.unwrap().unwrap();
    assert_eq!(reply.wrapped.map(|w| w.client_id), Some(7));
    let (_, seq) = reply.order.expect("TCP с номером");
    assert_eq!(seq, 0, "у телефона свои счётчики на VPS");
    assert_eq!(reply.payload.last(), Some(&99));

    // Телефон 8 того же роутера не может говорить от адреса телефона 7.
    router.send_client(8, None, &ip_packet(17, phone, [8, 8, 8, 8], 5)).await.unwrap();
    let mut buf = [0u8; 1500];
    assert!(tokio::time::timeout(Duration::from_millis(300), vps_app.recv(&mut buf)).await.is_err());
}
