//! UDP-сокет, привязанный к адресу и, на Linux, к интерфейсу.
//!
//! Чтобы дыры и STUN шли мимо VPN этой машины, сокет привязывают к физическому адаптеру. На
//! Windows хватает адреса: пакет уходит с интерфейса, которому принадлежит адрес источника. На
//! Linux маршрут выбирается по адресу назначения, и сокет с адресом `eth0` всё равно ушёл бы в
//! VPN (у которого маршрут по умолчанию), поэтому сокет ещё и привязывается к самому интерфейсу
//! (`SO_BINDTOIFINDEX`; прав не нужно, ядро 5.7+).

use std::io;
use std::net::IpAddr;

use tokio::net::UdpSocket;

/// Сокет на `ip:port`; `ifindex` — номер интерфейса, с которого только и отправлять (Linux).
pub async fn udp(ip: IpAddr, port: u16, ifindex: Option<u32>) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind((ip, port)).await?;
    if let Some(index) = ifindex {
        to_interface(&socket, index)?;
    }
    Ok(socket)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn to_interface(socket: &UdpSocket, index: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let value = index as libc::c_int;
    // SAFETY: дескриптор живой (сокет заимствован), значение — c_int на стеке с верным размером.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTOIFINDEX,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Не Linux: достаточно адреса (Windows отправляет с интерфейса адреса источника).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn to_interface(_socket: &UdpSocket, _index: u32) -> io::Result<()> {
    Ok(())
}
