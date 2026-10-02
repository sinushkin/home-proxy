//! Минимальный разбор заголовка IP-пакета: только протокол и порты (для записи в CSV,
//! `hp-stats`, см. PLAN-ML.md). Независимая от `hp-tun` копия нужного подмножества
//! `hp_tun::packet::inspect`: `connection` (где стоит тап на отправке) не может зависеть от
//! `hp-tun` — `hp-tun` сам зависит от `connection`, это был бы цикл.

/// Протокол транспортного уровня внутри туннеля.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
    Icmp,
    /// Не IPv4/IPv6, слишком короткий пакет или протокол, который не разбираем отдельно.
    Other,
}

impl Proto {
    fn from_number(n: u8) -> Self {
        match n {
            6 => Proto::Tcp,
            17 => Proto::Udp,
            1 | 58 => Proto::Icmp,
            _ => Proto::Other,
        }
    }

    /// Имя для столбца CSV.
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
            Proto::Icmp => "icmp",
            Proto::Other => "other",
        }
    }
}

/// Протокол и порты (если есть) внутреннего IP-пакета.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InnerInfo {
    pub proto: Proto,
    /// `(src, dst)`; `None` — не TCP/UDP, фрагмент не первый или пакет короче заголовков.
    pub ports: Option<(u16, u16)>,
}

const UNKNOWN: InnerInfo = InnerInfo { proto: Proto::Other, ports: None };

fn ports_at(packet: &[u8], offset: usize) -> Option<(u16, u16)> {
    let p = packet.get(offset..offset + 4)?;
    Some((u16::from_be_bytes([p[0], p[1]]), u16::from_be_bytes([p[2], p[3]])))
}

impl InnerInfo {
    /// Разбирает заголовок пакета, который уходит в дыру (`Data`/`WrappedData`/`Ordered.payload`).
    /// Не выделяет память, не падает на мусоре — непонятный пакет даёт `Other`/`None`.
    pub fn parse(packet: &[u8]) -> Self {
        let Some(&first) = packet.first() else { return UNKNOWN };
        match first >> 4 {
            4 => {
                if packet.len() < 20 {
                    return UNKNOWN;
                }
                let ihl = usize::from(packet[0] & 0x0f) * 4;
                if ihl < 20 || packet.len() < ihl {
                    return UNKNOWN;
                }
                let proto = Proto::from_number(packet[9]);
                let fragment_offset = u16::from_be_bytes([packet[6], packet[7]]) & 0x1fff;
                let ports = match proto {
                    Proto::Tcp | Proto::Udp if fragment_offset == 0 => ports_at(packet, ihl),
                    _ => None,
                };
                Self { proto, ports }
            }
            6 => {
                if packet.len() < 40 {
                    return UNKNOWN;
                }
                let proto = Proto::from_number(packet[6]);
                let ports = match proto {
                    Proto::Tcp | Proto::Udp => ports_at(packet, 40),
                    _ => None,
                };
                Self { proto, ports }
            }
            _ => UNKNOWN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4(proto: u8, sport: u16, dport: u16) -> Vec<u8> {
        let mut p = vec![0u8; 20 + 4];
        p[0] = 0x45; // версия 4, IHL 5 (20 байт)
        p[9] = proto;
        p[20..22].copy_from_slice(&sport.to_be_bytes());
        p[22..24].copy_from_slice(&dport.to_be_bytes());
        p
    }

    #[test]
    fn parses_tcp_and_udp_ports() {
        assert_eq!(InnerInfo::parse(&ipv4(6, 443, 51000)), InnerInfo { proto: Proto::Tcp, ports: Some((443, 51000)) });
        assert_eq!(InnerInfo::parse(&ipv4(17, 53, 60000)), InnerInfo { proto: Proto::Udp, ports: Some((53, 60000)) });
    }

    #[test]
    fn icmp_has_no_ports() {
        assert_eq!(InnerInfo::parse(&ipv4(1, 0, 0)), InnerInfo { proto: Proto::Icmp, ports: None });
    }

    #[test]
    fn garbage_and_short_buffers_are_other() {
        assert_eq!(InnerInfo::parse(&[]), UNKNOWN);
        assert_eq!(InnerInfo::parse(&[0xff; 10]), UNKNOWN);
        assert_eq!(InnerInfo::parse(&[0x45]), UNKNOWN, "заявлен IHL 5, но пакет короче заголовка");
    }

    #[test]
    fn non_first_fragment_has_no_ports() {
        let mut p = ipv4(6, 443, 51000);
        p[6] = 0x00;
        p[7] = 0x01; // fragment_offset = 1 (не первый фрагмент)
        assert_eq!(InnerInfo::parse(&p).ports, None);
    }

    #[test]
    fn proto_names_for_csv() {
        assert_eq!(Proto::Tcp.as_str(), "tcp");
        assert_eq!(Proto::Udp.as_str(), "udp");
        assert_eq!(Proto::Icmp.as_str(), "icmp");
        assert_eq!(Proto::Other.as_str(), "other");
    }
}
