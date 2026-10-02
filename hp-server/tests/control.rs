//! Протокол управления вживую на loopback: ключ, статус, подписка, ошибки запросов, смена ключа.
//! Служба — без пиров и без сопряжения (сеть не нужна).

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

/// Служба на случайном порту; ключ — во временном файле (его путь и отдаём).
async fn start() -> (std::net::SocketAddr, std::path::PathBuf) {
    let (device, stack) = hp_tun::device::channel_pair();
    // Второй конец канала держим открытым до конца теста: иначе мост сочтёт устройство закрытым.
    tokio::spawn(async move {
        let _stack = stack;
        std::future::pending::<()>().await
    });
    let hub = hp_tun::hub::Hub::start(device);
    let book = AddressBook::load(Ipv4Addr::new(10, 80, 0, 1), 16, None).unwrap();
    let service = Arc::new(Service::new(
        hub,
        book,
        vec![Ipv4Addr::new(8, 8, 8, 8)],
        MultiLinkOptions::default(),
        None,
        None,
        Mode::Netstack,
        None,
        #[cfg(feature = "stats")]
        None,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let key_file = std::env::temp_dir().join(format!("hp-control-test-{}.key", addr.port()));
    std::fs::write(&key_file, "secret\n").unwrap();
    tokio::spawn(hp_control::server::serve(listener, key_file.clone(), service));
    (addr, key_file)
}

#[tokio::test]
async fn wrong_key_is_rejected_and_new_key_works_at_once() {
    let (addr, key_file) = start().await;
    assert!(Client::connect(addr, "guess").await.is_err());
    assert!(Client::connect(addr, "secret").await.is_ok());
    // Новая строка подключения действует без перезапуска службы, старая — больше нет.
    let key = hp_control::replace_key(&key_file).unwrap();
    assert!(Client::connect(addr, "secret").await.is_err());
    let (_, welcome) = Client::connect_string(&hp_control::connection_string(addr, &key)).await.unwrap();
    assert_eq!(welcome.service, "hp-server");
    let _ = std::fs::remove_file(key_file);
}

#[tokio::test]
async fn status_subscription_and_errors() {
    let (addr, key_file) = start().await;
    let (mut client, welcome) = Client::connect(addr, "secret").await.unwrap();
    let _ = std::fs::remove_file(key_file);
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
