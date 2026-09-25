//! Релей роутера: набор дыр к серверу и по набору дыр к каждому телефону,
//! пересылка пакетов между ними.
//!
//! Телефон `k` шлёт роутеру обычную `Data`; роутер оборачивает её в
//! `WrappedData { client_id: k, .. }` и отправляет серверу по дырам общего набора.
//! Сервер отвечает `WrappedData` с тем же `client_id`; роутер по нему находит
//! набор дыр телефона `k`, разворачивает пакет (это делает `MultiLink` при
//! приёме) и отправляет телефону обычной `Data`. Содержимое пакета роутеру не
//! важно: WireGuard на нём не нужен.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::multilink::{Incoming, MultiLink};

/// Куда пересылаем пакеты одного направления.
pub trait Outbound: Send + Sync + 'static {
    /// Отправляет пакет; `Ok(slot)` — по какой дыре ушёл.
    fn send(&self, payload: Vec<u8>) -> impl Future<Output = Result<u8>> + Send;
}

/// Отправка обычной `Data` (к телефону: он про обёртку ничего не знает).
pub struct PlainOut(pub Arc<MultiLink>);

impl Outbound for PlainOut {
    async fn send(&self, payload: Vec<u8>) -> Result<u8> {
        self.0.send_data(payload).await
    }
}

/// Отправка `WrappedData` с номером клиента (к серверу).
pub struct WrappedOut {
    pub link: Arc<MultiLink>,
    pub client_id: u8,
}

impl Outbound for WrappedOut {
    async fn send(&self, payload: Vec<u8>) -> Result<u8> {
        self.link.send_wrapped(self.client_id, payload).await.map(|(slot, _seq)| slot)
    }
}

/// Счётчики одного направления. 32 бита: на 32-битных MIPS 64-битных атомиков нет.
#[derive(Default)]
pub struct DirectionStats {
    pub forwarded: AtomicU32,
    pub dropped: AtomicU32,
}

impl DirectionStats {
    pub fn snapshot(&self) -> (u32, u32) {
        (self.forwarded.load(Ordering::Relaxed), self.dropped.load(Ordering::Relaxed))
    }
}

/// Перекладывает пакеты из `incoming` в `out`, пока канал не закроется.
/// `name` — направление для логов, например `phone1->server`.
pub async fn forward<O: Outbound>(
    name: impl Into<String>,
    mut incoming: mpsc::Receiver<Incoming>,
    out: O,
    stats: Arc<DirectionStats>,
) {
    let name = name.into();
    while let Some(packet) = incoming.recv().await {
        deliver(&name, packet, &out, &stats).await;
    }
    log::warn!("{name}: канал входящих закрыт, пересылка остановлена");
}

/// Раскладывает пакеты от сервера по клиентам: `client_id` из `WrappedData`
/// выбирает выход. Пакет без `client_id` (обычная `Data`) или с неизвестным
/// `client_id` считается потерянным.
pub async fn route_by_client<O: Outbound>(
    name: impl Into<String>,
    mut incoming: mpsc::Receiver<Incoming>,
    clients: HashMap<u8, O>,
    stats: Arc<DirectionStats>,
) {
    let name = name.into();
    while let Some(packet) = incoming.recv().await {
        let Some(wrapped) = packet.wrapped else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            log::debug!("{name}: пришла обычная Data без client_id, некому отдать");
            continue;
        };
        let Some(out) = clients.get(&wrapped.client_id) else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            log::debug!("{name}: неизвестный client_id {}, пакет потерян", wrapped.client_id);
            continue;
        };
        deliver(&name, packet, out, &stats).await;
    }
    log::warn!("{name}: канал входящих закрыт, пересылка остановлена");
}

async fn deliver<O: Outbound>(name: &str, packet: Incoming, out: &O, stats: &DirectionStats) {
    let len = packet.payload.len();
    let from_slot = packet.slot;
    match out.send(packet.payload).await {
        Ok(to_slot) => {
            stats.forwarded.fetch_add(1, Ordering::Relaxed);
            log::trace!("{name}: {len} байт, дыра #{from_slot} -> дыра #{to_slot}");
        }
        Err(e) => {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            log::debug!("{name}: пакет {len} байт потерян: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multilink::WrappedInfo;
    use std::sync::Mutex;

    /// Собирает то, что «отправили», и может притвориться, что живых дыр нет.
    struct Collect {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        fail: bool,
    }

    impl Collect {
        fn new(fail: bool) -> Self {
            Self { sent: Arc::default(), fail }
        }
    }

    impl Outbound for Collect {
        async fn send(&self, payload: Vec<u8>) -> Result<u8> {
            if self.fail {
                anyhow::bail!("нет живых дыр");
            }
            self.sent.lock().unwrap().push(payload);
            Ok(3)
        }
    }

    fn incoming(slot: u8, payload: &[u8]) -> Incoming {
        Incoming { slot, payload: payload.to_vec(), wrapped: None }
    }

    fn wrapped(client_id: u8, payload: &[u8]) -> Incoming {
        Incoming {
            slot: 0,
            payload: payload.to_vec(),
            wrapped: Some(WrappedInfo { client_id, seq: 0 }),
        }
    }

    #[tokio::test]
    async fn forwards_every_packet_in_order_and_counts_them() {
        let (tx, rx) = mpsc::channel(8);
        let out = Collect::new(false);
        let sent = out.sent.clone();
        let stats = Arc::new(DirectionStats::default());
        let task = tokio::spawn(forward("test", rx, out, stats.clone()));

        for (slot, body) in [(1u8, &b"aa"[..]), (7, b"bbb"), (2, b"c")] {
            tx.send(incoming(slot, body)).await.unwrap();
        }
        drop(tx);
        task.await.unwrap();

        assert_eq!(*sent.lock().unwrap(), vec![b"aa".to_vec(), b"bbb".to_vec(), b"c".to_vec()]);
        assert_eq!(stats.snapshot(), (3, 0));
    }

    #[tokio::test]
    async fn counts_dropped_packets_when_there_is_nowhere_to_send() {
        let (tx, rx) = mpsc::channel(8);
        let stats = Arc::new(DirectionStats::default());
        let task = tokio::spawn(forward("test", rx, Collect::new(true), stats.clone()));

        tx.send(incoming(0, b"x")).await.unwrap();
        tx.send(incoming(0, b"y")).await.unwrap();
        drop(tx);
        task.await.unwrap();

        assert_eq!(stats.snapshot(), (0, 2));
    }

    #[tokio::test]
    async fn routes_server_packets_to_the_client_they_belong_to() {
        let (tx, rx) = mpsc::channel(8);
        let (phone1, phone2) = (Collect::new(false), Collect::new(false));
        let (sent1, sent2) = (phone1.sent.clone(), phone2.sent.clone());
        let stats = Arc::new(DirectionStats::default());
        let clients = HashMap::from([(1u8, phone1), (2u8, phone2)]);
        let task = tokio::spawn(route_by_client("test", rx, clients, stats.clone()));

        for (client_id, body) in [(2u8, &b"for-2-a"[..]), (1, b"for-1"), (2, b"for-2-b")] {
            tx.send(wrapped(client_id, body)).await.unwrap();
        }
        drop(tx);
        task.await.unwrap();

        assert_eq!(*sent1.lock().unwrap(), vec![b"for-1".to_vec()]);
        assert_eq!(*sent2.lock().unwrap(), vec![b"for-2-a".to_vec(), b"for-2-b".to_vec()]);
        assert_eq!(stats.snapshot(), (3, 0));
    }

    #[tokio::test]
    async fn unknown_client_and_plain_data_from_server_are_dropped() {
        let (tx, rx) = mpsc::channel(8);
        let phone = Collect::new(false);
        let sent = phone.sent.clone();
        let stats = Arc::new(DirectionStats::default());
        let task = tokio::spawn(route_by_client("test", rx, HashMap::from([(1u8, phone)]), stats.clone()));

        tx.send(wrapped(9, b"unknown client")).await.unwrap();
        tx.send(incoming(0, b"plain")).await.unwrap();
        tx.send(wrapped(1, b"ok")).await.unwrap();
        drop(tx);
        task.await.unwrap();

        assert_eq!(*sent.lock().unwrap(), vec![b"ok".to_vec()]);
        assert_eq!(stats.snapshot(), (1, 2));
    }
}
