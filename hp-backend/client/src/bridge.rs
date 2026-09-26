//! Локальный UDP-мост телефона: WireGuard <-> дыры к роутеру.
//!
//! WireGuard на телефоне настроен так, что его «сервер» (endpoint) — это
//! `127.0.0.1:<порт моста>`. Датаграмма, пришедшая на мост, уходит в дыры к
//! роутеру обычной `Data`; `Data` от роутера отправляется обратно с того же
//! сокета на адрес, с которого WireGuard последний раз писал.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use connection::multilink::{Incoming, MAX_DATA_LEN};
use connection::relay::Outbound;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const RECV_BUFFER: usize = 2048;

/// Счётчики моста. 32 бита: на 32-битных ARM/MIPS не рассчитываем на 64-битные атомики.
#[derive(Default)]
pub struct BridgeStats {
    pub to_router: AtomicU32,
    pub from_router: AtomicU32,
    pub dropped: AtomicU32,
}

impl BridgeStats {
    /// (в роутер, от роутера, потеряно)
    pub fn snapshot(&self) -> (u32, u32, u32) {
        (
            self.to_router.load(Ordering::Relaxed),
            self.from_router.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
        )
    }
}

/// Запущенный мост: две задачи (WireGuard -> роутер и роутер -> WireGuard),
/// при дропе обе останавливаются.
pub struct Bridge {
    local_addr: SocketAddr,
    stats: Arc<BridgeStats>,
    tasks: [JoinHandle<()>; 2],
}

impl Drop for Bridge {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Bridge {
    /// Запускает мост на уже привязанном сокете: датаграммы с него уходят в
    /// `out`, а то, что пришло в `incoming`, возвращается WireGuard'у.
    pub fn start<O: Outbound>(
        socket: UdpSocket,
        out: O,
        incoming: mpsc::Receiver<Incoming>,
    ) -> std::io::Result<Self> {
        let local_addr = socket.local_addr()?;
        let socket = Arc::new(socket);
        let wireguard: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
        let stats = Arc::new(BridgeStats::default());
        let uplink = tokio::spawn(uplink(socket.clone(), wireguard.clone(), out, stats.clone()));
        let downlink = tokio::spawn(downlink(socket, wireguard, incoming, stats.clone()));
        Ok(Self { local_addr, stats, tasks: [uplink, downlink] })
    }

    /// Адрес, на который смотрит WireGuard.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn stats(&self) -> &BridgeStats {
        &self.stats
    }
}

/// WireGuard -> роутер.
async fn uplink<O: Outbound>(
    socket: Arc<UdpSocket>,
    wireguard: Arc<Mutex<Option<SocketAddr>>>,
    out: O,
    stats: Arc<BridgeStats>,
) {
    let mut buf = vec![0u8; RECV_BUFFER];
    loop {
        let (len, from) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(e) => {
                log::warn!("мост: ошибка приёма от WireGuard: {e}");
                return;
            }
        };
        *wireguard.lock().unwrap() = Some(from);
        if len > MAX_DATA_LEN {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            log::debug!("мост: датаграмма {len} байт длиннее лимита {MAX_DATA_LEN}, снижайте MTU WireGuard");
            continue;
        }
        match out.send(&buf[..len]).await {
            Ok(slot) => {
                stats.to_router.fetch_add(1, Ordering::Relaxed);
                log::trace!("мост: WireGuard -> роутер {len} байт по дыре #{slot}");
            }
            Err(e) => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                log::debug!("мост: датаграмма {len} байт потеряна: {e:#}");
            }
        }
    }
}

/// Роутер -> WireGuard.
async fn downlink(
    socket: Arc<UdpSocket>,
    wireguard: Arc<Mutex<Option<SocketAddr>>>,
    mut incoming: mpsc::Receiver<Incoming>,
    stats: Arc<BridgeStats>,
) {
    while let Some(packet) = incoming.recv().await {
        let target = *wireguard.lock().unwrap();
        let Some(target) = target else {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            log::debug!("мост: WireGuard ещё ничего не присылал, ответ роутера потерян");
            continue;
        };
        match socket.send_to(&packet.payload, target).await {
            Ok(_) => {
                stats.from_router.fetch_add(1, Ordering::Relaxed);
                log::trace!("мост: роутер -> WireGuard {} байт, дыра #{}", packet.payload.len(), packet.slot);
            }
            Err(e) => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                log::debug!("мост: не удалось отправить WireGuard'у: {e}");
            }
        }
    }
    log::warn!("мост: канал входящих от роутера закрыт");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// Собирает то, что мост отправил бы роутеру.
    struct Collect(mpsc::UnboundedSender<Vec<u8>>);

    impl Outbound for Collect {
        async fn send(&self, payload: &[u8]) -> anyhow::Result<u8> {
            let _ = self.0.send(payload.to_vec());
            Ok(4)
        }
    }

    struct Down;

    impl Outbound for Down {
        async fn send(&self, _payload: &[u8]) -> anyhow::Result<u8> {
            anyhow::bail!("нет живых дыр")
        }
    }

    async fn bridge_with_collector() -> (
        Bridge,
        mpsc::UnboundedReceiver<Vec<u8>>,
        mpsc::Sender<Incoming>,
    ) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::channel(8);
        (Bridge::start(socket, Collect(out_tx), in_rx).unwrap(), out_rx, in_tx)
    }

    fn from_router(payload: &[u8]) -> Incoming {
        Incoming { slot: 2, payload: connection::pool::Packet::copy_from(payload).unwrap(), wrapped: None, order: None }
    }

    #[tokio::test]
    async fn wireguard_datagrams_go_to_the_router_and_replies_come_back() {
        let (bridge, mut to_router, from_router_tx) = bridge_with_collector().await;
        let wireguard = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        wireguard.send_to(b"handshake", bridge.local_addr()).await.unwrap();
        let got = timeout(Duration::from_secs(2), to_router.recv()).await.unwrap().unwrap();
        assert_eq!(got, b"handshake");

        from_router_tx.send(from_router(b"response")).await.unwrap();
        let mut buf = [0u8; 64];
        let (len, from) = timeout(Duration::from_secs(2), wireguard.recv_from(&mut buf))
            .await
            .expect("ответ роутера не дошёл до WireGuard")
            .unwrap();
        assert_eq!(&buf[..len], b"response");
        assert_eq!(from, bridge.local_addr(), "ответ должен прийти с адреса моста");
        assert_eq!(bridge.stats().snapshot(), (1, 1, 0));
    }

    #[tokio::test]
    async fn reply_goes_to_the_latest_wireguard_source() {
        let (bridge, mut to_router, from_router_tx) = bridge_with_collector().await;
        let first = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let second = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        first.send_to(b"a", bridge.local_addr()).await.unwrap();
        to_router.recv().await.unwrap();
        second.send_to(b"b", bridge.local_addr()).await.unwrap();
        to_router.recv().await.unwrap();

        from_router_tx.send(from_router(b"reply")).await.unwrap();
        let mut buf = [0u8; 64];
        let (len, _) = timeout(Duration::from_secs(2), second.recv_from(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..len], b"reply");
        assert!(
            timeout(Duration::from_millis(150), first.recv_from(&mut buf)).await.is_err(),
            "прежний источник WireGuard не должен получать ответ"
        );
    }

    #[tokio::test]
    async fn router_packets_before_any_wireguard_traffic_are_dropped() {
        let (bridge, _to_router, from_router_tx) = bridge_with_collector().await;

        from_router_tx.send(from_router(b"too early")).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(bridge.stats().snapshot(), (0, 0, 1));
    }

    #[tokio::test]
    async fn oversized_datagrams_and_dead_uplink_are_counted_as_dropped() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (_in_tx, in_rx) = mpsc::channel(1);
        let bridge = Bridge::start(socket, Down, in_rx).unwrap();
        let wireguard = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        wireguard.send_to(&vec![0u8; MAX_DATA_LEN + 1], bridge.local_addr()).await.unwrap();
        wireguard.send_to(b"small", bridge.local_addr()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(bridge.stats().snapshot(), (0, 0, 2));
    }
}
