//! Мост TUN ↔ дыры (`MultiLink`) без WireGuard: IP-пакеты идут по дырам как есть. Это мост
//! клиентского конца — телефона, хоста (`vps-client`), роутера: одна связь с сервером. Сервер с
//! многими пирами и выдачей адресов — `hub`.
//!
//! Из TUN: пакет TCP получает корзину потока (`flow_hash % FLOW_BUCKETS`) и номер в ней и уходит
//! как `Ordered` — получатель вернёт порядок внутри корзины (буфер порядка), так что потеря в одном
//! соединении не задерживает остальные. UDP, ICMP и прочее уходят обычным `Data` и у получателя
//! отдаются сразу: им задержка хуже перестановки (QUIC, DNS, звонки сами с ней справляются).
//! В TUN: всё, что пришло из дыр, пишется как есть.
//!
//! Адрес в туннеле клиент не выбирает сам, а получает от сервера: `request_address`.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use connection::multilink::{Control, Incoming, MultiLink, MAX_DATA_LEN};
use connection::pool::PACKET_CAP;
use connection::proto::{AddressAssign, AddressKind, AddressRequest};
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
        self.route_inspected(packet.len(), packet::inspect(packet).as_ref())
    }

    /// То же по уже разобранному заголовку (`packet::inspect`) и длине пакета.
    pub fn route_inspected(&mut self, len: usize, info: Option<&packet::Info>) -> Route {
        if len > MAX_DATA_LEN {
            return Route::Drop;
        }
        match info {
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

/// Отправляет IP-пакет пиру по маршруту `route`: своему пиру напрямую или (`client` задан) —
/// клиенту за роутером обёрнутым. Общая часть моста и `hub`.
pub(crate) async fn send_routed(link: &MultiLink, client: Option<u8>, route: Route, packet: &[u8], stats: &BridgeStats) {
    let sent = match (route, client) {
        (Route::Ordered { flow, seq }, None) => {
            stats.ordered.fetch_add(1, Ordering::Relaxed);
            link.send_ordered(flow, seq, packet).await
        }
        (Route::Ordered { flow, seq }, Some(client)) => {
            stats.ordered.fetch_add(1, Ordering::Relaxed);
            link.send_client(client, Some((flow, seq)), packet).await
        }
        (Route::Plain, None) => link.send_data(packet).await,
        (Route::Plain, Some(client)) => link.send_client(client, None, packet).await,
        (Route::Drop, _) => {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            log::debug!("TUN: пакет {} байт не отправлен (не IP или больше {MAX_DATA_LEN})", packet.len());
            return;
        }
    };
    match sent {
        Ok(_) => {
            stats.to_peer.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            log::trace!("TUN: пакет {} байт потерян: {e:#}", packet.len());
        }
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
    /// Клиентский конец: всё от сервера пишется в TUN.
    pub fn start(tun: Tun, link: Arc<MultiLink>, incoming: mpsc::Receiver<Incoming>) -> Self {
        Self::spawn(tun, link, incoming, None)
    }

    /// Роутер: свой трафик — через TUN, как обычно, а пакеты клиентов (`WrappedData` от VPS)
    /// в TUN не пишутся, а уходят в `clients` — их перекладывают телефонам как есть.
    pub fn start_relay(
        tun: Tun,
        link: Arc<MultiLink>,
        incoming: mpsc::Receiver<Incoming>,
        clients: mpsc::Sender<Incoming>,
    ) -> Self {
        Self::spawn(tun, link, incoming, Some(clients))
    }

    fn spawn(
        tun: Tun,
        link: Arc<MultiLink>,
        incoming: mpsc::Receiver<Incoming>,
        relay: Option<mpsc::Sender<Incoming>>,
    ) -> Self {
        let tun = Arc::new(tun);
        let stats = Arc::new(BridgeStats::default());
        let up = tokio::spawn(uplink(tun.clone(), link, stats.clone()));
        let down = tokio::spawn(downlink(tun, incoming, relay, stats.clone()));
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
        let route = sequencer.route(packet);
        send_routed(&link, None, route, packet, &stats).await;
    }
}

async fn downlink(tun: Arc<Tun>, mut incoming: mpsc::Receiver<Incoming>, relay: Option<mpsc::Sender<Incoming>>, stats: Arc<BridgeStats>) {
    while let Some(packet) = incoming.recv().await {
        if packet.wrapped.is_some() {
            match &relay {
                Some(relay) => {
                    if relay.send(packet).await.is_err() {
                        log::warn!("TUN: ретрансляция клиентам остановлена");
                        return;
                    }
                }
                None => {
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            continue;
        }
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

/// Повторяет запрос адреса раз в `ADDRESS_REFRESH`, пока задачу не остановят (ответы читает
/// тот, кто держит приёмник служебных сообщений; им можно пренебречь).
pub fn spawn_address_refresh(link: Arc<MultiLink>, kind: AddressKind, client_id: Option<u32>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let request = Control::AddressRequest(AddressRequest { kind: kind as i32, client_id });
        let mut ticker = tokio::time::interval(ADDRESS_REFRESH);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let _ = link.send_control(request.clone()).await;
        }
    })
}

/// Выданный сервером адрес в туннеле и DNS для клиента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assigned {
    pub address: Ipv4Addr,
    pub prefix: u8,
    /// DNS от сервера по порядку (его резолверы, затем запасные); некорректные записи пропущены.
    pub dns: Vec<Ipv4Addr>,
}

impl Assigned {
    /// Из ответа сервера; `None`, если ответ некорректный.
    pub fn from_proto(assign: &AddressAssign) -> Option<Self> {
        let octets: [u8; 4] = assign.address.as_slice().try_into().ok()?;
        let prefix = u8::try_from(assign.prefix).ok().filter(|p| *p <= 32)?;
        let dns = assign.dns.iter().filter_map(|d| <[u8; 4]>::try_from(d.as_slice()).ok()).map(Ipv4Addr::from).collect();
        Some(Self { address: Ipv4Addr::from(octets), prefix, dns })
    }
}

/// Как часто повторять запрос адреса, пока нет ответа.
const ADDRESS_RETRY: Duration = Duration::from_secs(1);
/// Как часто повторять запрос, когда адрес уже есть: сервер после перезапуска восстановит по нему
/// маршрут к нам (адрес у него записан в книге, выдастся тот же).
pub const ADDRESS_REFRESH: Duration = Duration::from_secs(30);

/// Просит адрес в туннеле у сервера по дырам `link` и ждёт ответа (запрос повторяется раз в
/// секунду: дыр может ещё не быть, ответ может потеряться). `control` — приёмник служебных
/// сообщений этого `MultiLink` (`take_control`); ответы для клиентов за роутером (`client_id`)
/// здесь пропускаются. Возвращает приёмник обратно: по нему дальше могут прийти ещё сообщения.
/// Данные от сервера (`incoming`), пришедшие до адреса, отбрасываются: TUN ещё нет, а
/// непрочитанный канал данных остановил бы и приём служебных сообщений.
pub async fn request_address(
    link: &MultiLink,
    mut control: mpsc::Receiver<Control>,
    incoming: &mut mpsc::Receiver<Incoming>,
    kind: AddressKind,
) -> (Assigned, mpsc::Receiver<Control>) {
    let request = Control::AddressRequest(AddressRequest { kind: kind as i32, client_id: None });
    let mut retry = tokio::time::interval(ADDRESS_RETRY);
    loop {
        tokio::select! {
            _ = retry.tick() => {
                let _ = link.send_control(request.clone()).await;
            }
            message = control.recv() => match message {
                Some(Control::AddressAssign(assign)) if assign.client_id.is_none() => {
                    if let Some(assigned) = Assigned::from_proto(&assign) {
                        log::info!("адрес в туннеле: {}/{}, DNS {:?}", assigned.address, assigned.prefix, assigned.dns);
                        return (assigned, control);
                    }
                    log::warn!("некорректный адрес от сервера: {assign:?}");
                }
                Some(_) => {}
                None => std::future::pending::<()>().await,
            },
            Some(_) = incoming.recv() => {}
        }
    }
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
