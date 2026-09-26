use std::ffi::CStr;
use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

/// Настройки нового интерфейса.
#[derive(Clone, Debug)]
pub struct TunConfig {
    /// Имя (`hp0`) или шаблон ядра (`hp%d`); пусто — выберет ядро (`tun0`, `tun1`, …).
    pub name: String,
    /// Адрес IPv4 и длина префикса (`10.79.0.1/24`); `None` — не задавать.
    pub address: Option<(Ipv4Addr, u8)>,
    /// MTU; `None` — оставить по умолчанию (1500).
    pub mtu: Option<u16>,
    /// Поднять интерфейс (`IFF_UP`).
    pub up: bool,
}

impl Default for TunConfig {
    fn default() -> Self {
        Self { name: String::new(), address: None, mtu: None, up: true }
    }
}

/// TUN-интерфейс: чтение и запись по одному IP-пакету, асинхронно (tokio).
pub struct Tun {
    fd: AsyncFd<OwnedFd>,
    name: String,
}

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret) }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl над своим открытым дескриптором.
    unsafe {
        let flags = check(libc::fcntl(fd, libc::F_GETFL))?;
        check(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
    }
    Ok(())
}

fn ifreq_named(name: &str) -> io::Result<libc::ifreq> {
    // SAFETY: ifreq — POD, нули — корректное начальное значение.
    let mut ifr: libc::ifreq = unsafe { mem::zeroed() };
    let bytes = name.as_bytes();
    if bytes.len() >= libc::IFNAMSIZ || bytes.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("имя интерфейса «{name}» некорректно")));
    }
    for (dst, src) in ifr.ifr_name.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    Ok(ifr)
}

fn sockaddr_v4(ip: Ipv4Addr) -> libc::sockaddr {
    // SAFETY: sockaddr_in и sockaddr одного размера и представления для AF_INET.
    unsafe {
        let mut sin: libc::sockaddr_in = mem::zeroed();
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(ip.octets()) };
        mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin)
    }
}

fn prefix_mask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
    Ipv4Addr::from(bits)
}

/// `ioctl` настройки интерфейса через обычный сокет.
fn if_ioctl(socket: RawFd, request: libc::Ioctl, ifr: &mut libc::ifreq) -> io::Result<()> {
    // SAFETY: ifr — корректный ifreq с именем интерфейса.
    check(unsafe { libc::ioctl(socket, request, ifr as *mut libc::ifreq) })?;
    Ok(())
}

impl Tun {
    /// Создаёт TUN-интерфейс (Linux: OpenWrt, WSL2, обычный Linux). Нужны права `CAP_NET_ADMIN`.
    pub fn create(config: &TunConfig) -> io::Result<Self> {
        // SAFETY: открытие устройства; результат проверяется.
        let raw = check(unsafe {
            libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC)
        })?;
        // SAFETY: raw — только что открытый дескриптор, владеем им.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let mut ifr = ifreq_named(&config.name)?;
        ifr.ifr_ifru.ifru_flags = (libc::IFF_TUN | libc::IFF_NO_PI) as libc::c_short;
        // SAFETY: TUNSETIFF с корректным ifreq.
        check(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TUNSETIFF as libc::Ioctl, &mut ifr as *mut libc::ifreq) })?;
        // SAFETY: ядро заполнило ifr_name строкой с нулём в конце.
        let name = unsafe { CStr::from_ptr(ifr.ifr_name.as_ptr()) }.to_string_lossy().into_owned();

        if config.address.is_some() || config.mtu.is_some() || config.up {
            // SAFETY: обычный UDP-сокет для ioctl настройки интерфейса.
            let raw_socket = check(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
            // SAFETY: владеем только что созданным сокетом.
            let socket = unsafe { OwnedFd::from_raw_fd(raw_socket) };
            let s = socket.as_raw_fd();
            if let Some(mtu) = config.mtu {
                let mut ifr = ifreq_named(&name)?;
                ifr.ifr_ifru.ifru_mtu = libc::c_int::from(mtu);
                if_ioctl(s, libc::SIOCSIFMTU as libc::Ioctl, &mut ifr)?;
            }
            if let Some((ip, prefix)) = config.address {
                let mut ifr = ifreq_named(&name)?;
                ifr.ifr_ifru.ifru_addr = sockaddr_v4(ip);
                if_ioctl(s, libc::SIOCSIFADDR as libc::Ioctl, &mut ifr)?;
                let mut ifr = ifreq_named(&name)?;
                ifr.ifr_ifru.ifru_netmask = sockaddr_v4(prefix_mask(prefix));
                if_ioctl(s, libc::SIOCSIFNETMASK as libc::Ioctl, &mut ifr)?;
            }
            if config.up {
                let mut ifr = ifreq_named(&name)?;
                if_ioctl(s, libc::SIOCGIFFLAGS as libc::Ioctl, &mut ifr)?;
                // SAFETY: после SIOCGIFFLAGS в объединении лежат флаги.
                unsafe { ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short };
                if_ioctl(s, libc::SIOCSIFFLAGS as libc::Ioctl, &mut ifr)?;
            }
        }
        Ok(Self { fd: AsyncFd::with_interest(fd, Interest::READABLE | Interest::WRITABLE)?, name })
    }

    /// Готовый дескриптор TUN (Android: `VpnService.Builder.establish()`; Linux: открытый
    /// заранее). Дескриптор переводится в неблокирующий режим, владение переходит к `Tun`.
    pub fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        set_nonblocking(fd.as_raw_fd())?;
        Ok(Self { fd: AsyncFd::with_interest(fd, Interest::READABLE | Interest::WRITABLE)?, name: String::new() })
    }

    /// Имя интерфейса (пусто для `from_fd`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Читает один IP-пакет в `buf`; длина пакета.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            // SAFETY: чтение в свой буфер не длиннее buf.len().
            let result = guard.try_io(|fd| {
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            });
            if let Ok(result) = result {
                return result;
            }
        }
    }

    /// Пишет один IP-пакет.
    pub async fn send(&self, packet: &[u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.fd.writable().await?;
            // SAFETY: запись из своего буфера длины packet.len().
            let result = guard.try_io(|fd| {
                let n = unsafe { libc::write(fd.as_raw_fd(), packet.as_ptr().cast(), packet.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            });
            if let Ok(result) = result {
                return result;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_masks() {
        assert_eq!(prefix_mask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(prefix_mask(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(prefix_mask(0), Ipv4Addr::new(0, 0, 0, 0));
        assert_eq!(prefix_mask(20), Ipv4Addr::new(255, 255, 240, 0));
    }

    #[test]
    fn interface_names_are_validated() {
        assert!(ifreq_named("hp0").is_ok());
        assert!(ifreq_named("hp%d").is_ok());
        assert!(ifreq_named("a-name-that-is-too-long").is_err());
        assert!(ifreq_named("bad\0name").is_err());
    }

    #[tokio::test]
    async fn from_fd_reads_and_writes_whole_datagrams() {
        // Вместо TUN — пара сокетов SOCK_SEQPACKET: те же границы «один вызов = один пакет».
        let mut fds = [0; 2];
        // SAFETY: создаём пару сокетов, дескрипторы проверяем.
        check(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) }).unwrap();
        // SAFETY: владеем обоими дескрипторами.
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let (a, b) = (Tun::from_fd(a).unwrap(), Tun::from_fd(b).unwrap());
        assert_eq!(a.send(b"packet-one").await.unwrap(), 10);
        a.send(b"two").await.unwrap();
        let mut buf = [0u8; 1500];
        let n = b.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"packet-one");
        let n = b.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"two");
    }
}
