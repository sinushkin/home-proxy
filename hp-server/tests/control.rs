//! Протокол управления вживую на loopback: токен, статус, подписка, ошибки запросов. Служба —
//! без пиров и без сопряжения (сеть не нужна).

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use connection::multilink::MultiLinkOptions;
use hp_control::proto::control_message::Body;
use hp_control::proto::{GetStatus, RemovePeer, Subscribe};
use hp_control::{Client, PROTOCOL_VERSION};
use hp_server::addresses::AddressBook;
use hp_server::service::Service;
use hp_server::Mode;

async fn start() -> std::net::SocketAddr {
    let (device, stack) = hp_tun::device::channel_pair();
    // Второй конец канала держим открытым до конца теста: иначе мост сочтёт устройство закрытым.
    tokio::spawn(async move {
        let _stack = stack;
        std::future::pending::<()>().await
    });
    let hub = hp_tun::hub::Hub::start(device);
    let book = AddressBook::load(Ipv4Addr::new(10, 80, 0, 1), 16, None).unwrap();
    let service = Arc::new(Service::new(hub, book, vec![Ipv4Addr::new(8, 8, 8, 8)], MultiLinkOptions::default(), None, None, Mode::Netstack, None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(hp_server::control::serve(listener, Arc::new("secret".to_string()), service));
    addr
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let addr = start().await;
    assert!(Client::connect(addr, "guess").await.is_err());
}

#[tokio::test]
async fn status_subscription_and_errors() {
    let addr = start().await;
    let (mut client, welcome) = Client::connect(addr, "secret").await.unwrap();
    assert_eq!(welcome.version, PROTOCOL_VERSION);
    assert_eq!(welcome.service, "hp-server");

    let Body::Status(status) = client.request(Body::GetStatus(GetStatus {})).await.unwrap() else { panic!("ждали Status") };
    assert!(status.peers.is_empty());
    assert_eq!(status.mode, "netstack");
    assert!(!status.pairing_supported);

    let err = client.request(Body::RemovePeer(RemovePeer { name: "0badf00d".into() })).await.unwrap_err();
    assert!(err.to_string().contains("нет пира"), "{err}");
    let err = client.request(Body::CreatePairing(Default::default())).await.unwrap_err();
    assert!(err.to_string().contains("STUN + MQTT"), "{err}");

    client.send(Body::Subscribe(Subscribe { interval_ms: 200 })).await.unwrap();
    for _ in 0..3 {
        let body = tokio::time::timeout(Duration::from_secs(2), client.recv()).await.expect("статус по подписке").unwrap();
        assert!(matches!(body, Body::Status(_)));
    }
}
