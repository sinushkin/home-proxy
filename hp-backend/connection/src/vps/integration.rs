//! VPS-режим целиком на loopback: знакомство через порт сервера, динамический набор дыр без STUN
//! и MQTT, ротация, несколько клиентов на одном порту, перезапуск клиента.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use super::Bootstrap;
use crate::discovery::Discovery;
use crate::holes::HoleState;
use crate::multilink::{Incoming, MultiLink, MultiLinkOptions};
use crate::test_support::{active_ids, all_ids, quick_options, wait_until, LONG_AGE};

async fn start_pair(bootstrap: &Bootstrap, port: u16, server_id: Uuid, client_id: Uuid, options: MultiLinkOptions) -> ((MultiLink, mpsc::Receiver<Incoming>), (MultiLink, mpsc::Receiver<Incoming>)) {
    let server = MultiLink::start_discovery(
        "",
        Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap: bootstrap.clone() },
        server_id,
        client_id,
        options,
    )
    .await
    .unwrap();
    let client = MultiLink::start_discovery(
        "",
        Discovery::VpsClient { server: SocketAddr::from(([127, 0, 0, 1], port)) },
        client_id,
        server_id,
        options,
    )
    .await
    .unwrap();
    (server, client)
}

/// VPS-режим целиком на loopback: знакомство через порт сервера, набор дыр растёт до максимума
/// без STUN и MQTT, данные в обе стороны, возраст дыры идёт, слитая дыра заменяется новой.
#[tokio::test]
async fn vps_dynamic_set_grows_links_and_replaces_a_retired_hole() {
    let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
    let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let bootstrap = Bootstrap::bind(bootstrap_port, 47100..=47299).await.unwrap();
    let ((server, mut server_rx), (client, mut client_rx)) =
        start_pair(&bootstrap, bootstrap_port, server_id, client_id, quick_options(3, 5, LONG_AGE)).await;

    wait_until("5 дыр с обеих сторон", 40, || server.live_count() == 5 && client.live_count() == 5).await;
    for (_, addr) in client.live_links() {
        let port = addr.unwrap().port();
        assert!((47100..=47299).contains(&port), "клиент ходит на порт из диапазона сервера: {port}");
    }

    client.send_data(b"ping").await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(2), server_rx.recv()).await.unwrap().unwrap();
    assert_eq!(got.payload, b"ping");
    server.send_data(b"pong").await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(2), client_rx.recv()).await.unwrap().unwrap();
    assert_eq!(got.payload, b"pong");

    // TUN-режим: пакеты с номером в потоке выходят у получателя по порядку.
    for seq in 0..20u64 {
        let mut packet = vec![0x45u8; 60];
        packet[59] = seq as u8;
        server.send_ordered(5, seq, &packet).await.unwrap();
    }
    let mut got = Vec::new();
    while got.len() < 20 {
        let p = tokio::time::timeout(Duration::from_secs(2), client_rx.recv()).await.unwrap().unwrap();
        got.push(p.order.unwrap());
    }
    assert_eq!(got, (0..20u64).map(|s| (5u32, s)).collect::<Vec<_>>());

    // Возраст дыры идёт; все дыры в работе.
    let before = client.status().holes;
    assert!(before.iter().all(|h| h.state == HoleState::Active));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let after = client.status().holes;
    for h in &before {
        let later = after.iter().find(|a| a.slot == h.slot).expect("дыра жива");
        assert!(later.age >= h.age + Duration::from_secs(1), "возраст растёт: {:?} -> {:?}", h.age, later.age);
    }

    // Сервер сливает дыру: оба конца её закрывают, клиент открывает замену под новым номером.
    let victim = all_ids(&client)[2];
    server.move_slot(victim).await;
    wait_until("дыра слита и заменена", 40, || {
        !all_ids(&client).contains(&victim) && !all_ids(&server).contains(&victim) && client.live_count() == 5 && server.live_count() == 5
    })
    .await;
}

/// Дыры стареют и заменяются, а пакеты при этом не теряются: слив отдаёт последние пакеты,
/// отправка по сливаемой дыре прекращается, а в работе всегда не меньше `min_active` дыр.
#[tokio::test]
async fn vps_holes_rotate_by_age_without_losing_packets() {
    let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
    let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let bootstrap = Bootstrap::bind(bootstrap_port, 47900..=48099).await.unwrap();
    let options = quick_options(3, 4, (Duration::from_secs(3), Duration::from_secs(4)));
    let ((server, server_rx), (client, client_rx)) = start_pair(&bootstrap, bootstrap_port, server_id, client_id, options).await;
    wait_until("4 дыры с обеих сторон", 40, || server.live_count() == 4 && client.live_count() == 4).await;
    let first = all_ids(&client);

    const COUNT: u32 = 500;
    let up = async {
        for i in 0..COUNT {
            client.send_data(&i.to_be_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    };
    let down = async {
        for i in 0..COUNT {
            server.send_data(&i.to_be_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    };
    // Следим, что в работе всегда не меньше `min_active` дыр на обеих сторонах.
    let watch = async {
        let mut worst = usize::MAX;
        for _ in 0..140 {
            worst = worst.min(active_ids(&client).len()).min(active_ids(&server).len());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        worst
    };
    let collect = |mut rx: mpsc::Receiver<Incoming>| async move {
        let mut seen = std::collections::HashSet::new();
        while seen.len() < COUNT as usize {
            let Ok(Some(p)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await else { break };
            seen.insert(u32::from_be_bytes(p.payload[..4].try_into().unwrap()));
        }
        seen.len()
    };
    let (_, _, worst, at_server, at_client) = tokio::join!(up, down, watch, collect(server_rx), collect(client_rx));
    assert_eq!(at_server, COUNT as usize, "вверх дошло не всё");
    assert_eq!(at_client, COUNT as usize, "вниз дошло не всё");
    assert!(worst >= 3, "в работе было меньше min_active дыр: {worst}");

    let now = all_ids(&client);
    let replaced = first.iter().filter(|id| !now.contains(id)).count();
    assert!(replaced >= 2, "за 14 с должны смениться хотя бы две дыры из {first:?}, сейчас {now:?}");
    let server_now = all_ids(&server);
    assert!(server_now.iter().all(|id| now.contains(id)) || server_now.len() <= 4, "сервер держит лишнее: {server_now:?} против {now:?}");
}

/// Два клиента на одном порту знакомства (один `Bootstrap` на процесс, общий GUID сервера):
/// оба набирают дыры, а остановка одного не трогает другого и освобождает его имя.
#[tokio::test]
async fn vps_server_serves_two_clients_on_one_port() {
    let server_id = Uuid::new_v4();
    let (client_a, client_b) = (Uuid::new_v4(), Uuid::new_v4());
    let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let bootstrap = Bootstrap::bind(bootstrap_port, 47300..=47499).await.unwrap();
    let server_discovery = || Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap: bootstrap.clone() };
    let client_discovery = || Discovery::VpsClient { server: SocketAddr::from(([127, 0, 0, 1], bootstrap_port)) };
    let options = quick_options(3, 4, LONG_AGE);

    let (server_a, _rx_a) = MultiLink::start_discovery("", server_discovery(), server_id, client_a, options).await.unwrap();
    let (server_b, _rx_b) = MultiLink::start_discovery("", server_discovery(), server_id, client_b, options).await.unwrap();
    let (peer_a, _peer_a_rx) = MultiLink::start_discovery("", client_discovery(), client_a, server_id, options).await.unwrap();
    let (peer_b, _peer_b_rx) = MultiLink::start_discovery("", client_discovery(), client_b, server_id, options).await.unwrap();
    wait_until("оба клиента: по 4 дыры", 40, || {
        server_a.live_count() == 4 && server_b.live_count() == 4 && peer_a.live_count() == 4 && peer_b.live_count() == 4
    })
    .await;

    // Остановили клиента B: его наборы дыр уходят, клиент A продолжает работать.
    drop(peer_b);
    drop(server_b);
    wait_until("A остался с 4 дырами", 20, || server_a.live_count() == 4 && peer_a.live_count() == 4).await;

    // Клиент B вернулся: его имя снова занято только им, знакомство проходит.
    let (server_b, _rx_b) = MultiLink::start_discovery("", server_discovery(), server_id, client_b, options).await.unwrap();
    let (peer_b, _peer_b_rx) = MultiLink::start_discovery("", client_discovery(), client_b, server_id, options).await.unwrap();
    wait_until("B снова с 4 дырами", 40, || server_b.live_count() == 4 && peer_b.live_count() == 4).await;
}

/// Клиент пропал (процесс убит) и пришёл снова, а сервер своих дыр ещё не потерял: сервер
/// узнаёт перезапуск по времени запуска в запросе знакомства, закрывает прошлые дыры клиента
/// и принимает нового сразу, не дожидаясь тайм-аута потери (15 с).
#[tokio::test]
async fn vps_server_accepts_a_restarted_client_at_once() {
    let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
    let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let bootstrap = Bootstrap::bind(bootstrap_port, 47600..=47799).await.unwrap();
    let server_discovery = || Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap: bootstrap.clone() };
    let client_discovery = || Discovery::VpsClient { server: SocketAddr::from(([127, 0, 0, 1], bootstrap_port)) };
    let options = quick_options(3, 4, LONG_AGE);
    let (server, _server_rx) = MultiLink::start_discovery("", server_discovery(), server_id, client_id, options).await.unwrap();

    let (first, _first_rx) = MultiLink::start_discovery("", client_discovery(), client_id, server_id, options).await.unwrap();
    wait_until("первый клиент: 4 дыры", 20, || server.live_count() == 4 && first.live_count() == 4).await;
    let old = all_ids(&server);
    drop(first);
    // Время запуска в миллисекундах: второй клиент должен стартовать позже первого.
    tokio::time::sleep(Duration::from_millis(5)).await;

    let (second, mut second_rx) = MultiLink::start_discovery("", client_discovery(), client_id, server_id, options).await.unwrap();
    wait_until("сервер держит только дыры нового клиента", 10, || {
        let now = all_ids(&server);
        now.len() == 4 && now.iter().all(|id| !old.contains(id)) && second.live_count() == 4
    })
    .await;
    server.send_data(b"hello").await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(2), second_rx.recv()).await.unwrap().unwrap();
    assert_eq!(got.payload, b"hello");
}

/// hp-stats целиком на loopback (PLAN-ML.md): сервер нумерует пакеты вниз, клиент (сам
/// синхронизировавшись по часам через `TimeProbe`/`TimeEcho`) шлёт обратно `PidReport`,
/// сборщик сопоставляет и пишет в CSV строку `delivered=1` с разумным `flight_ms`. Проверяет
/// сквозную проводку всех частей модуля, а не только их по отдельности (остальные тесты).
#[cfg(feature = "stats")]
#[tokio::test]
async fn stats_pipeline_records_a_delivered_packet_end_to_end() {
    let (server_id, client_id) = (Uuid::new_v4(), Uuid::new_v4());
    let bootstrap_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let bootstrap = Bootstrap::bind(bootstrap_port, 47700..=47899).await.unwrap();
    let options = MultiLinkOptions::default();
    let (server, _server_rx) = MultiLink::start_discovery(
        "",
        Discovery::VpsServer { public_ip: "127.0.0.1".parse().unwrap(), bootstrap: bootstrap.clone() },
        server_id,
        client_id,
        options,
    )
    .await
    .unwrap();
    let (client, _client_rx) = MultiLink::start_discovery(
        "",
        Discovery::VpsClient { server: SocketAddr::from(([127, 0, 0, 1], bootstrap_port)) },
        client_id,
        server_id,
        options,
    )
    .await
    .unwrap();
    wait_until("дыры с обеих сторон", 40, || server.live_count() >= 4 && client.live_count() >= 4).await;

    let dir = std::env::temp_dir().join(format!("hp-stats-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (handle, _collector_task) = hp_stats::spawn(hp_stats::CollectorConfig {
        out_dir: dir.clone(),
        loss_timeout: Duration::from_secs(2),
        sweep_interval: Duration::from_millis(50),
        ..hp_stats::CollectorConfig::default()
    })
    .unwrap();
    server.attach_stats(handle);

    // Несколько пакетов вниз — каждый получает номер и попадёт в CSV после PidReport клиента.
    for i in 0..5u8 {
        server.send_data(&[i; 10]).await.unwrap();
    }

    // PidReport клиента уходит раз в ~50 мс; даём время обратной связи дойти и записаться
    // (плюс начальный раунд синхронизации часов клиента — восемь проб по 500 мс).
    wait_until("CSV получил хотя бы одну доставленную строку", 15, || {
        read_csv_rows(&dir).iter().any(|r| r.get(2).map(String::as_str) == Some("1"))
    })
    .await;

    let rows = read_csv_rows(&dir);
    let delivered: Vec<&Vec<String>> = rows.iter().filter(|r| r[2] == "1").collect();
    assert!(!delivered.is_empty(), "хотя бы один пакет должен быть зафиксирован как доставленный");
    for row in &delivered {
        let flight_ms: i64 = row[3].parse().expect("flight_ms должен быть числом у доставленного пакета");
        assert!(flight_ms.abs() < 2000, "задержка на loopback должна быть разумной: {flight_ms} мс, строка {row:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Читает все строки (без заголовка) из всех CSV в каталоге сборщика.
#[cfg(feature = "stats")]
fn read_csv_rows(dir: &std::path::Path) -> Vec<Vec<String>> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut rows = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let Ok(content) = std::fs::read_to_string(entry.path()) else { continue };
        for line in content.lines().skip(1) {
            if !line.is_empty() {
                rows.push(line.split(',').map(str::to_string).collect());
            }
        }
    }
    rows
}
