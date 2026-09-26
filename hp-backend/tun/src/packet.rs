//! Разбор заголовков IP-пакетов из TUN: версия, протокол, адреса, порты и хэш соединения.
//! Всё без выделения памяти, по ссылке на буфер пакета.

use std::net::IpAddr;

/// Протокол транспортного уровня.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
    Icmp,
    Other(u8),
}

impl Proto {
    fn from_number(n: u8) -> Self {
        match n {
            6 => Proto::Tcp,
            17 => Proto::Udp,
            1 | 58 => Proto::Icmp,
            other => Proto::Other(other),
        }
    }
}

/// Сведения о пакете.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub version: u8,
    pub proto: Proto,
    pub src: IpAddr,
    pub dst: IpAddr,
    /// Порты TCP/UDP (у фрагментов IPv4 без первого и у прочих протоколов — `None`).
    pub ports: Option<(u16, u16)>,
    /// Длина пакета по заголовку IP.
    pub len: usize,
}

impl Info {
    /// Хэш соединения (протокол, адреса, порты) — одинаков у всех пакетов одного потока в одну
    /// сторону. FNV-1a, 32 бита.
    pub fn flow_hash(&self) -> u32 {
        let mut h: u32 = 0x811c_9dc5;
        let mut eat = |bytes: &[u8]| {
            for &b in bytes {
                h ^= u32::from(b);
                h = h.wrapping_mul(0x0100_0193);
            }
        };
        eat(&[match self.proto {
            Proto::Tcp => 6,
            Proto::Udp => 17,
            Proto::Icmp => 1,
            Proto::Other(n) => n,
        }]);
        match (self.src, self.dst) {
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                eat(&s.octets());
                eat(&d.octets());
            }
            (s, d) => {
                eat(&ip_bytes16(s));
                eat(&ip_bytes16(d));
            }
        }
        if let Some((sp, dp)) = self.ports {
            eat(&sp.to_be_bytes());
            eat(&dp.to_be_bytes());
        }
        h
    }
}

fn ip_bytes16(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    }
}

fn ports_at(packet: &[u8], offset: usize) -> Option<(u16, u16)> {
    let p = packet.get(offset..offset + 4)?;
    Some((u16::from_be_bytes([p[0], p[1]]), u16::from_be_bytes([p[2], p[3]])))
}

/// Разбирает заголовок IP. `None` — не IPv4/IPv6 или пакет короче заголовка.
pub fn inspect(packet: &[u8]) -> Option<Info> {
    match packet.first()? >> 4 {
        4 => {
            let ihl = usize::from(packet[0] & 0x0f) * 4;
            if ihl < 20 || packet.len() < ihl {
                return None;
            }
            let proto = Proto::from_number(packet[9]);
            let fragment_offset = u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff;
            let ports = match proto {
                Proto::Tcp | Proto::Udp if fragment_offset == 0 => ports_at(packet, ihl),
                _ => None,
            };
            Some(Info {
                version: 4,
                proto,
                src: IpAddr::from(<[u8; 4]>::try_from(&packet[12..16]).ok()?),
                dst: IpAddr::from(<[u8; 4]>::try_from(&packet[16..20]).ok()?),
                ports,
                len: usize::from(u16::from_be_bytes([packet[2], packet[3]])),
            })
        }
        6 => {
            if packet.len() < 40 {
                return None;
            }
            // Заголовки-расширения не разбираем: такой пакет считаем «прочим» (без портов).
            let proto = Proto::from_number(packet[6]);
            let ports = match proto {
                Proto::Tcp | Proto::Udp => ports_at(packet, 40),
                _ => None,
            };
            Some(Info {
                version: 6,
                proto,
                src: IpAddr::from(<[u8; 16]>::try_from(&packet[8..24]).ok()?),
                dst: IpAddr::from(<[u8; 16]>::try_from(&packet[24..40]).ok()?),
                ports,
                len: 40 + usize::from(u16::from_be_bytes([packet[4], packet[5]])),
            })
        }
        _ => None,
    }
}

/// Контрольная сумма интернета (RFC 1071).
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u32::from(u16::from_be_bytes([c[0], c[1]]));
    }
    if let [last] = chunks.remainder() {
        sum += u32::from(*last) << 8;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Ответ на ICMPv4 echo request (ping): меняет адреса местами, тип 0, пересчитывает суммы.
/// Длина ответа или `None`, если это не echo request IPv4 / не влезает в `out`.
pub fn icmp_echo_reply(request: &[u8], out: &mut [u8]) -> Option<usize> {
    let info = inspect(request)?;
    let ihl = usize::from(request[0] & 0x0f) * 4;
    if info.version != 4 || info.proto != Proto::Icmp || request.len() < ihl + 8 || request[ihl] != 8 {
        return None;
    }
    let len = info.len.min(request.len());
    let reply = out.get_mut(..len)?;
    reply.copy_from_slice(&request[..len]);
    reply[12..16].copy_from_slice(&request[16..20]);
    reply[16..20].copy_from_slice(&request[12..16]);
    reply[8] = 64; // TTL
    reply[10..12].copy_from_slice(&[0, 0]);
    let ip_sum = checksum(&reply[..ihl]);
    reply[10..12].copy_from_slice(&ip_sum.to_be_bytes());
    reply[ihl] = 0; // echo reply
    reply[ihl + 2..ihl + 4].copy_from_slice(&[0, 0]);
    let icmp_sum = checksum(&reply[ihl..len]);
    reply[ihl + 2..ihl + 4].copy_from_slice(&icmp_sum.to_be_bytes());
    Some(len)
}

/// IPv4 + UDP-пакет (сумма UDP = 0: в IPv4 это «не посчитана», допустимо). Для тестов и проверок.
pub fn ipv4_udp(src: ([u8; 4], u16), dst: ([u8; 4], u16), payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let len = 28 + payload.len();
    let p = out.get_mut(..len)?;
    p[..20].copy_from_slice(&[0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 17, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    p[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    p[12..16].copy_from_slice(&src.0);
    p[16..20].copy_from_slice(&dst.0);
    let sum = checksum(&p[..20]);
    p[10..12].copy_from_slice(&sum.to_be_bytes());
    p[20..22].copy_from_slice(&src.1.to_be_bytes());
    p[22..24].copy_from_slice(&dst.1.to_be_bytes());
    p[24..26].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    p[26..28].copy_from_slice(&[0, 0]);
    p[28..].copy_from_slice(payload);
    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn udp(src: [u8; 4], sp: u16, dst: [u8; 4], dp: u16, payload: &[u8]) -> Vec<u8> {
        let mut buf = [0u8; 1500];
        let n = ipv4_udp((src, sp), (dst, dp), payload, &mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn icmp_echo(src: [u8; 4], dst: [u8; 4]) -> Vec<u8> {
        let mut p = vec![0x45, 0, 0, 36, 0, 1, 0, 0, 64, 1, 0, 0];
        p.extend_from_slice(&src);
        p.extend_from_slice(&dst);
        let sum = checksum(&p[..20]);
        p[10..12].copy_from_slice(&sum.to_be_bytes());
        let mut icmp = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1, b'p', b'i', b'n', b'g', 1, 2, 3, 4];
        let s = checksum(&icmp);
        icmp[2..4].copy_from_slice(&s.to_be_bytes());
        p.extend_from_slice(&icmp);
        p
    }

    #[test]
    fn ipv4_udp_is_parsed() {
        let p = udp([10, 0, 0, 1], 5000, [10, 0, 0, 2], 53, b"hello");
        let info = inspect(&p).unwrap();
        assert_eq!(info.version, 4);
        assert_eq!(info.proto, Proto::Udp);
        assert_eq!(info.src, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(info.dst, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
        assert_eq!(info.ports, Some((5000, 53)));
        assert_eq!(info.len, p.len());
        assert_eq!(checksum(&p[..20]), 0, "сумма заголовка IPv4 верна");
    }

    #[test]
    fn ipv4_tcp_and_fragments() {
        let mut p = udp([1, 2, 3, 4], 443, [5, 6, 7, 8], 50000, b"x");
        p[9] = 6;
        let info = inspect(&p).unwrap();
        assert_eq!((info.proto, info.ports), (Proto::Tcp, Some((443, 50000))));
        p[6] = 0x00;
        p[7] = 0x10; // не первый фрагмент: портов в нём нет
        assert_eq!(inspect(&p).unwrap().ports, None);
    }

    #[test]
    fn ipv6_tcp_is_parsed() {
        let mut p = vec![0u8; 60];
        p[0] = 0x60;
        p[4..6].copy_from_slice(&20u16.to_be_bytes());
        p[6] = 6;
        p[8..24].copy_from_slice(&Ipv6Addr::LOCALHOST.octets());
        p[24..40].copy_from_slice(&Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).octets());
        p[40..44].copy_from_slice(&[0x1f, 0x90, 0xc3, 0x50]);
        let info = inspect(&p).unwrap();
        assert_eq!(info.version, 6);
        assert_eq!(info.proto, Proto::Tcp);
        assert_eq!(info.ports, Some((8080, 50000)));
        assert_eq!(info.len, 60);
    }

    #[test]
    fn garbage_is_not_an_ip_packet() {
        assert_eq!(inspect(&[]), None);
        assert_eq!(inspect(&[0x45, 0, 0]), None);
        assert_eq!(inspect(&[0x12; 64]), None);
        assert_eq!(inspect(&[0x60; 20]), None);
    }

    #[test]
    fn flow_hash_separates_connections_and_keeps_one_stable() {
        let a = inspect(&udp([10, 0, 0, 1], 1000, [10, 0, 0, 2], 80, b"a")).unwrap();
        let a2 = inspect(&udp([10, 0, 0, 1], 1000, [10, 0, 0, 2], 80, b"another payload")).unwrap();
        let b = inspect(&udp([10, 0, 0, 1], 1001, [10, 0, 0, 2], 80, b"a")).unwrap();
        assert_eq!(a.flow_hash(), a2.flow_hash());
        assert_ne!(a.flow_hash(), b.flow_hash());
    }

    #[test]
    fn ping_gets_a_valid_reply() {
        let request = icmp_echo([10, 0, 0, 1], [10, 0, 0, 2]);
        let mut out = [0u8; 1500];
        let n = icmp_echo_reply(&request, &mut out).unwrap();
        let reply = &out[..n];
        let info = inspect(reply).unwrap();
        assert_eq!(info.src, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)));
        assert_eq!(info.dst, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(reply[20], 0, "echo reply");
        assert_eq!(checksum(&reply[..20]), 0);
        assert_eq!(checksum(&reply[20..]), 0);
        assert_eq!(&reply[28..], &request[28..], "данные ping возвращаются как есть");
        assert_eq!(icmp_echo_reply(&udp([1, 1, 1, 1], 1, [2, 2, 2, 2], 2, b""), &mut out), None);
    }
}
