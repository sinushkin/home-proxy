//! Мост TUN ↔ дыры (`MultiLink`) без WireGuard: IP-пакеты идут по дырам как есть.
//!
//! Из TUN: пакет TCP получает корзину потока (`flow_hash % FLOW_BUCKETS`) и номер в ней и уходит
//! как `Ordered` — получатель вернёт порядок внутри корзины (буфер порядка), так что потеря в одном
//! соединении не задерживает остальные. UDP, ICMP и прочее уходят обычным `Data` и у получателя
//! отдаются сразу: им задержка хуже перестановки (QUIC, DNS, звонки сами с ней справляются).
//! В TUN: всё, что пришло из дыр, пишется как есть.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use connection::multilink::{Incoming, MultiLink, MAX_DATA_LEN};
use connection::pool::PACKET_CAP;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::packet::{self, Proto};
use crate::Tun;

/// Сколько корзин (счётчиков номеров) у TCP-потоков.
pub const FLOW_BUCKETS: u32 = 16;

/// Куда отправить пакет из TUN.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// TCP: с номером `seq` в корзине `flow`.
    Ordered { flow: u32, seq: u64 },
    /// Всё остальное: без номера.
    Plain,
    /// Не IP-пакет или длиннее, чем влезает в дыру.
    Drop,
}

/// Счётчики номеров по корзинам (одно направление, один отправитель).
#[derive(Default)]
pub struct Sequencer {
    next: [u64; FLOW_BUCKETS as usize],
}

impl Sequencer {
    pub fn route(&mut self, packet: &[u8]) -> Route {
        if packet.len() > MAX_DATA_LEN {
            return Route::Drop;
        }
        match packet::inspect(packet) {
            Some(info) if info.proto == Proto::Tcp => {
                let flow = info.flow_hash() % FLOW_BUCKETS;
                let seq = self.next[flow as usize];
                self.next[flow as usize] += 1;
                Route::Ordered { flow, seq }
            }
            Some(_) => Route::Plain,
            None => Route::Drop,
        }
    }
}

/// Счётчики моста (32 бита: на MIPS32 64-битных атомиков нет).
#[derive(Default)]
pub struct BridgeStats {
    pub to_peer: AtomicU32,
    pub ordered: AtomicU32,
    pub from_peer: AtomicU32,
    pub dropped: AtomicU32,
}

impl BridgeStats {
    /// (к пиру, из них с номером, от пира, потеряно).
    pub fn snapshot(&self) -> (u32, u32, u32, u32) {
        (
            self.to_peer.load(Ordering::Relaxed),
            self.ordered.load(Ordering::Relaxed),
            self.from_peer.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
        )
    }
}

/// Запущенный мост; дроп останавливает его задачи.
pub struct Bridge {
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
    pub fn start(tun: Tun, link: Arc<MultiLink>, incoming: mpsc::Receiver<Incoming>) -> Self {
        let tun = Arc::new(tun);
        let stats = Arc::new(BridgeStats::default());
        let up = tokio::spawn(uplink(tun.clone(), link, stats.clone()));
        let down = tokio::spawn(downlink(tun, incoming, stats.clone()));
        Self { stats, tasks: [up, down] }
    }

    pub fn stats(&self) -> &BridgeStats {
        &self.stats
    }
}

async fn uplink(tun: Arc<Tun>, link: Arc<MultiLink>, stats: Arc<BridgeStats>) {
    let mut buf = [0u8; PACKET_CAP];
    let mut sequencer = Sequencer::default();
    loop {
        let n = match tun.recv(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                log::warn!("TUN: ошибка чтения: {e}");
                return;
            }
        };
        let packet = &buf[..n];
        let sent = match sequencer.route(packet) {
            Route::Ordered { flow, seq } => {
                stats.ordered.fetch_add(1, Ordering::Relaxed);
                link.send_ordered(flow, seq, packet).await
            }
            Route::Plain => link.send_data(packet).await,
            Route::Drop => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                log::debug!("TUN: пакет {n} байт не отправлен (не IP или больше {MAX_DATA_LEN})");
                continue;
            }
        };
        match sent {
            Ok(_) => {
                stats.to_peer.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                log::trace!("TUN: пакет {n} байт потерян: {e:#}");
            }
        }
    }
}

async fn downlink(tun: Arc<Tun>, mut incoming: mpsc::Receiver<Incoming>, stats: Arc<BridgeStats>) {
    while let Some(packet) = incoming.recv().await {
        match tun.send(&packet.payload).await {
            Ok(_) => {
                stats.from_peer.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                log::debug!("TUN: запись {} байт не удалась: {e}", packet.payload.len());
            }
        }
    }
    log::warn!("TUN: канал входящих закрыт");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(sport: u16) -> Vec<u8> {
        let mut out = [0u8; 1500];
        let n = packet::ipv4_udp(([10, 0, 0, 1], sport), ([1, 1, 1, 1], 443), b"payload", &mut out).unwrap();
        let mut p = out[..n].to_vec();
        p[9] = 6;
        p
    }

    fn udp(sport: u16) -> Vec<u8> {
        let mut out = [0u8; 1500];
        let n = packet::ipv4_udp(([10, 0, 0, 1], sport), ([8, 8, 8, 8], 53), b"q", &mut out).unwrap();
        out[..n].to_vec()
    }

    #[test]
    fn tcp_is_numbered_per_flow_and_the_rest_is_plain() {
        let mut s = Sequencer::default();
        let first = s.route(&tcp(1000));
        let Route::Ordered { flow, seq: 0 } = first else { panic!("{first:?}") };
        assert_eq!(s.route(&tcp(1000)), Route::Ordered { flow, seq: 1 }, "тот же поток — следующий номер");
        assert_eq!(s.route(&udp(1000)), Route::Plain);
        assert_eq!(s.route(&tcp(1000)), Route::Ordered { flow, seq: 2 }, "UDP номер не тратит");
        assert!(flow < FLOW_BUCKETS);
    }

    #[test]
    fn different_connections_usually_land_in_different_buckets() {
        let mut s = Sequencer::default();
        let flows: std::collections::HashSet<u32> = (0..64u16)
            .filter_map(|port| match s.route(&tcp(2000 + port)) {
                Route::Ordered { flow, .. } => Some(flow),
                _ => None,
            })
            .collect();
        assert!(flows.len() > FLOW_BUCKETS as usize / 2, "корзины используются: {flows:?}");
    }

    #[test]
    fn garbage_and_oversized_packets_are_dropped() {
        let mut s = Sequencer::default();
        assert_eq!(s.route(&[0u8; 20]), Route::Drop);
        let mut big = udp(1);
        big.resize(MAX_DATA_LEN + 1, 0);
        assert_eq!(s.route(&big), Route::Drop);
    }
}
