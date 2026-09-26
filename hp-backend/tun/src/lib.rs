//! TUN-интерфейс для home-proxy: IP-пакеты напрямую из ядра и в ядро, без WireGuard.
//!
//! Своя лёгкая обёртка над `/dev/net/tun` (`libc` + `tokio::io::unix::AsyncFd`) вместо крейтов
//! `tun`/`tun-rs`/`tokio-tun`: на Linux, OpenWrt и WSL2 это один `ioctl(TUNSETIFF)`, на Android
//! TUN создаёт система (`VpnService`), нам нужен только готовый дескриптор. Так нет лишних
//! зависимостей, которые пришлось бы проверять на MIPS32 (роутеры): 64-битных атомиков здесь нет.
//!
//! - [`Tun::create`] — Linux (в т.ч. OpenWrt, WSL2): создаёт интерфейс, при желании задаёт адрес
//!   IPv4, MTU и поднимает его. Нужны права `CAP_NET_ADMIN` (root) и `/dev/net/tun` (на OpenWrt —
//!   модуль `kmod-tun`).
//! - [`Tun::from_fd`] — Android (и Linux): дескриптор от `VpnService.Builder.establish()`.
//! - [`Tun::recv`] / [`Tun::send`] — один IP-пакет за вызов (без префикса `IFF_NO_PI`).
//! - [`packet`] — разбор заголовков IP (протокол, адреса, порты, хэш соединения).
//! - [`bridge`] — мост TUN ↔ дыры (`MultiLink`): TCP с номером в потоке (`Ordered`, получатель
//!   восстанавливает порядок), остальное (UDP, ICMP…) — сразу.

pub mod packet;

use std::net::Ipv4Addr;

/// Адрес интерфейса `ip/префикс` (`10.80.0.2/24`); `None`, если записано не так.
pub fn parse_cidr(value: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip, prefix) = value.trim().split_once('/')?;
    let prefix: u8 = prefix.parse().ok().filter(|p| *p <= 32)?;
    Some((ip.parse().ok()?, prefix))
}

#[cfg(test)]
mod tests {
    #[test]
    fn cidr_parses() {
        assert_eq!(super::parse_cidr("10.80.0.2/24"), Some(("10.80.0.2".parse().unwrap(), 24)));
        assert_eq!(super::parse_cidr("10.80.0.2"), None);
        assert_eq!(super::parse_cidr("10.80.0.2/33"), None);
        assert_eq!(super::parse_cidr("vps/24"), None);
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod bridge;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub use imp::{Tun, TunConfig};
