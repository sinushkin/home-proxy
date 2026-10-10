//! P2P-режим целиком на loopback: настоящие STUN-ответы (поддельный сервер) и пробив, первая запись —
//! через канал в памяти вместо MQTT-брокера.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use super::{start_with, Bootstrap, Config};
use crate::multilink::{Incoming, ModeStarted, MultiLink, MultiLinkOptions};
use crate::rendezvous::PeerSession;
use crate::test_support::{active_ids, all_ids, quick_options, wait_until};

/// Конец канала знакомства в памяти: куда писать свои записи и откуда читать записи пира.
#[derive(Clone)]
struct MemoryBusEnd {
    tx: mpsc::Sender<PeerSession>,
    rx: Arc<Mutex<Option<mpsc::Receiver<PeerSession>>>>,
}

impl MemoryBusEnd {
    /// Две стороны одного канала: что публикует одна, получает другая.
    fn pair() -> (Self, Self) {
        let (a_tx, b_rx) = mpsc::channel(64);
        let (b_tx, a_rx) = mpsc::channel(64);
        (Self { tx: a_tx, rx: Arc::new(Mutex::new(Some(a_rx))) }, Self { tx: b_tx, rx: Arc::new(Mutex::new(Some(b_rx))) })
    }
}

/// Запускает P2P-набор с обменом первой записью через `bus` вместо MQTT.
async fn start_memory(
    stun: SocketAddr,
    bus: MemoryBusEnd,
    my_peer_id: Uuid,
    peer_id: Uuid,
    options: MultiLinkOptions,
) -> (MultiLink, mpsc::Receiver<Incoming>) {
    MultiLink::start_mode("", my_peer_id, peer_id, options, move |ctx| async move {
        let rx = bus.rx.lock().unwrap().take().expect("канал знакомства берут один раз");
        let config = Config {
            stun_addrs: vec![stun],
            mqtt_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            mqtt_ca_pem: Vec::new(),
            my_peer_id: ctx.my_peer_id,
            peer_id: ctx.peer_id,
            pair: ctx.pair.clone(),
            bind_ip: ctx.bind_ip,
            bind_ifindex: ctx.options.bind_ifindex,
            local_port_base: ctx.options.local_port_base,
            pool: ctx.options.pool,
            hole_age: ctx.options.hole_age,
        };
        let started = start_with(&ctx.label, config, ctx.factory.clone(), Bootstrap::Memory(bus.tx.clone()), rx, None);
        Ok(ModeStarted { tasks: started.tasks, hole_records: Some(started.hole_records), registration: None })
    })
    .await
    .unwrap()
}

/// P2P-режим целиком на loopback (настоящие STUN-ответы и пробив, первая запись — через канал в
/// памяти вместо MQTT): ролей нет, обе стороны держат набор, дыры стареют и заменяются, слив идёт
/// по подтверждению, а пакеты в обе стороны не теряются.
#[tokio::test]
async fn p2p_dynamic_holes_rotate_without_losing_packets() {
    let stun = super::stun::fake::spawn().await;
    let (end_a, end_b) = MemoryBusEnd::pair();
    let (id_a, id_b) = (Uuid::new_v4(), Uuid::new_v4());
    let options = quick_options(3, 8, (Duration::from_secs(3), Duration::from_secs(4)));
    let (a, rx_a) = start_memory(stun, end_a, id_a, id_b, options).await;
    let (b, rx_b) = start_memory(stun, end_b, id_b, id_a, options).await;

    wait_until("не меньше 4 дыр с обеих сторон", 60, || a.live_count() >= 4 && b.live_count() >= 4).await;
    let first = all_ids(&a);
    assert_eq!({ let mut x = all_ids(&a); x.sort(); x }, { let mut x = all_ids(&b); x.sort(); x }, "обе стороны держат одни и те же дыры");

    const COUNT: u32 = 400;
    let up = async {
        for i in 0..COUNT {
            a.send_data(&i.to_be_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    };
    let down = async {
        for i in 0..COUNT {
            b.send_data(&i.to_be_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    };
    let watch = async {
        let mut worst = usize::MAX;
        for _ in 0..120 {
            worst = worst.min(active_ids(&a).len()).min(active_ids(&b).len());
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
    let (_, _, worst, at_b, at_a) = tokio::join!(up, down, watch, collect(rx_b), collect(rx_a));
    assert_eq!(at_b, COUNT as usize, "от первого ко второму дошло не всё");
    assert_eq!(at_a, COUNT as usize, "от второго к первому дошло не всё");
    // Ролей нет: обе стороны сливают просроченные дыры по своим часам, и две просьбы могут разойтись
    // в одном такте — тогда набор в работе на мгновение меньше минимума на одну дыру.
    assert!(worst >= 2, "в работе было меньше min_active - 1 дыр: {worst}");
    let now = all_ids(&a);
    let replaced = first.iter().filter(|id| !now.contains(id)).count();
    assert!(replaced >= 2, "за 12 с должны смениться хотя бы две дыры из {first:?}, сейчас {now:?}");
    assert!(now.len() <= 10, "дыр не больше максимума: {now:?}");
}
