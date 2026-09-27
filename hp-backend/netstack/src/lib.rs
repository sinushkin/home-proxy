//! Свой сетевой стек процесса: IP-пакеты телефона завершаются внутри `hp-server`, а наружу уходят
//! обычными сокетами программы. Так не нужны ни интерфейс TUN, ни NAT, ни права администратора —
//! это путь для Windows, где TUN — отдельный драйвер, а NAT капризный (`New-NetNat`, ICS).
//!
//! Соединения открывает сам процесс, поэтому система ведёт их по своей таблице маршрутов — через
//! «любимый» домашний VPN, если он есть. Дыры же `hp-server` привязывает к адресу физического
//! адаптера (`BIND_ADDR`), и они идут мимо VPN, с настоящего домашнего адреса.
//!
//! - TCP: поток телефона (`ipstack`) ↔ `TcpStream` к тому же адресу назначения;
//! - UDP: поток телефона ↔ свой `UdpSocket` на тот же адрес, поток закрывается по простою;
//! - ICMP echo (ping): отвечаем сами (задержка — только до ПК);
//! - остальное отбрасывается.
//!
//! Стек подключается к мосту (`hp_tun::hub::Hub`) каналом в памяти (`hp_tun::device::channel_pair`).

use std::net::SocketAddr;
use std::time::Duration;

use hp_tun::device::StackEnd;
use ipstack::{IpNumber, IpStack, IpStackConfig, IpStackStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::task::JoinHandle;

/// MTU пакетов телефона (как у его TUN).
pub const MTU: u16 = 1400;
/// Сколько ждать соединения с адресом назначения.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// UDP-поток без пакетов столько — закрывается.
const UDP_IDLE: Duration = Duration::from_secs(60);

/// Запускает стек на конце канала `end`; дроп `JoinHandle` его не останавливает — `abort()`.
pub fn start(end: StackEnd) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut config = IpStackConfig::default();
        config.mtu_unchecked(MTU);
        config.udp_timeout(UDP_IDLE);
        // По умолчанию у ipstack «в полёте» не больше 16 КБ — при задержке до телефона 80 мс это
        // ~200 КБ/с. Масштабирования окна в нём нет, так что потолок — 64 КБ на соединение.
        let mut tcp = ipstack::TcpConfig::default();
        tcp.max_unacked_bytes = u32::from(u16::MAX);
        config.with_tcp_config(tcp);
        let mut stack = IpStack::new(config, end);
        loop {
            match stack.accept().await {
                Ok(IpStackStream::Tcp(tcp)) => {
                    tokio::spawn(relay_tcp(tcp));
                }
                Ok(IpStackStream::Udp(udp)) => {
                    tokio::spawn(relay_udp(udp));
                }
                Ok(IpStackStream::UnknownTransport(packet)) => answer_ping(packet),
                Ok(IpStackStream::UnknownNetwork(_)) => {}
                Err(e) => {
                    log::warn!("сетевой стек остановлен: {e}");
                    return;
                }
            }
        }
    })
}

async fn relay_tcp(mut phone: ipstack::IpStackTcpStream) {
    let target = phone.peer_addr();
    let outside = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            log::debug!("TCP {} -> {target}: {e}", phone.local_addr());
            return;
        }
        Err(_) => {
            log::debug!("TCP {} -> {target}: нет ответа за {CONNECT_TIMEOUT:?}", phone.local_addr());
            return;
        }
    };
    let _ = outside.set_nodelay(true);
    let mut outside = outside;
    let result = tokio::io::copy_bidirectional(&mut phone, &mut outside).await;
    log::trace!("TCP {} -> {target} закрыто: {result:?}", phone.local_addr());
    let _ = outside.shutdown().await;
    let _ = phone.shutdown().await;
}

async fn relay_udp(mut phone: ipstack::IpStackUdpStream) {
    let target = phone.peer_addr();
    let bind: SocketAddr = if target.is_ipv4() { ([0, 0, 0, 0], 0).into() } else { ([0u16; 8], 0).into() };
    let outside = match UdpSocket::bind(bind).await {
        Ok(socket) => socket,
        Err(e) => {
            log::debug!("UDP {target}: {e}");
            return;
        }
    };
    if let Err(e) = outside.connect(target).await {
        log::debug!("UDP {target}: {e}");
        return;
    }
    let mut from_phone = vec![0u8; usize::from(MTU)];
    let mut from_outside = vec![0u8; 65536];
    loop {
        tokio::select! {
            read = phone.read(&mut from_phone) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = outside.send(&from_phone[..n]).await;
                }
            },
            received = outside.recv(&mut from_outside) => match received {
                Ok(n) => {
                    if phone.write_all(&from_outside[..n]).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::trace!("UDP {target}: {e}");
                }
            },
        }
    }
}

/// Отвечает на ICMPv4 echo сам: телефон видит, что туннель до ПК живой.
fn answer_ping(packet: ipstack::IpStackUnknownTransport) {
    if !packet.src_addr().is_ipv4() || packet.ip_protocol() != IpNumber::ICMP {
        return;
    }
    let Ok((header, payload)) = etherparse::Icmpv4Header::from_slice(packet.payload()) else { return };
    if let etherparse::Icmpv4Type::EchoRequest(echo) = header.icmp_type {
        let mut reply = etherparse::Icmpv4Header::new(etherparse::Icmpv4Type::EchoReply(echo));
        reply.update_checksum(payload);
        let mut bytes = reply.to_bytes().to_vec();
        bytes.extend_from_slice(payload);
        let _ = packet.send(bytes);
    }
}
