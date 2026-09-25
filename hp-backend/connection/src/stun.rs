//! Минимальный STUN-клиент по RFC 5389: ровно столько, чтобы отправить
//! Binding Request и прочитать свой публичный `SocketAddr` из
//! XOR-MAPPED-ADDRESS. Только IPv4.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::UdpSocket;

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_RESPONSE: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

/// Разбирает список STUN-серверов `ip:порт,ip:порт` (через запятую, пробелы
/// вокруг игнорируются). Нужен хотя бы один; повторы выбрасываются.
pub fn parse_servers(list: &str) -> anyhow::Result<Vec<SocketAddr>> {
    let mut servers = Vec::new();
    for part in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let addr: SocketAddr = part
            .parse()
            .map_err(|_| anyhow::anyhow!("STUN: «{part}» — ожидается ip:порт"))?;
        if !servers.contains(&addr) {
            servers.push(addr);
        }
    }
    anyhow::ensure!(!servers.is_empty(), "STUN: не указан ни один сервер (ожидается ip:порт[,ip:порт])");
    Ok(servers)
}

/// Что показывают ответы нескольких STUN-серверов для одного сокета (пары
/// «сервер, увиденный адрес»): `None`, если всё согласовано или ответил один.
pub fn describe_nat(observed: &[(SocketAddr, SocketAddr)]) -> Option<String> {
    let (_, first) = *observed.first()?;
    let differing: Vec<&(SocketAddr, SocketAddr)> = observed.iter().filter(|(_, seen)| *seen != first).collect();
    if differing.is_empty() {
        return None;
    }
    let same_ip = differing.iter().all(|(_, seen)| seen.ip() == first.ip());
    let kind = if same_ip {
        "порт зависит от адресата (вероятно, симметричный NAT): пробив может не сработать"
    } else {
        "внешний адрес зависит от маршрута до сервера: пир будет стучаться по всем"
    };
    let list = observed
        .iter()
        .map(|(server, seen)| format!("{server} -> {seen}"))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("{kind} ({list})"))
}

/// Отправляет Binding Request на `stun_addr` через `socket` и возвращает
/// публичный `SocketAddr`, который сервер увидел у нас.
pub async fn query(socket: &UdpSocket, stun_addr: SocketAddr) -> io::Result<SocketAddr> {
    let txn_id = transaction_id();

    let mut request = Vec::with_capacity(20);
    request.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    request.extend_from_slice(&0u16.to_be_bytes()); // без атрибутов
    request.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    request.extend_from_slice(&txn_id);

    socket.send_to(&request, stun_addr).await?;

    // На сокет слота одновременно могут приходить чужие пакеты (пир уже стучится
    // на этот порт), поэтому ждём именно ответ нашего сервера с нашим
    // transaction id, а остальное молча пропускаем. Общий таймаут — у вызывающего.
    let mut buf = [0u8; 512];
    loop {
        let (n, from) = socket.recv_from(&mut buf).await?;
        if from != stun_addr {
            continue;
        }
        if let Ok(mapped) = parse_binding_response(&buf[..n], &txn_id) {
            return Ok(mapped);
        }
    }
}

fn transaction_id() -> [u8; 12] {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut id = [0u8; 12];
    for (i, b) in id.iter_mut().enumerate() {
        *b = (seed >> (i * 8)) as u8;
    }
    id
}

fn parse_binding_response(data: &[u8], txn_id: &[u8; 12]) -> io::Result<SocketAddr> {
    if data.len() < 20 {
        return Err(invalid("STUN response shorter than a header"));
    }
    if u16::from_be_bytes([data[0], data[1]]) != BINDING_RESPONSE {
        return Err(invalid("not a STUN binding response"));
    }
    if &data[8..20] != txn_id {
        return Err(invalid("STUN transaction id mismatch"));
    }

    let mut offset = 20;
    while offset + 4 <= data.len() {
        let attr_type = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let attr_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        let value_start = offset + 4;
        let value_end = value_start + attr_len;
        if value_end > data.len() {
            break;
        }
        if attr_type == XOR_MAPPED_ADDRESS {
            return parse_xor_mapped_address(&data[value_start..value_end]);
        }
        offset = value_start + attr_len.div_ceil(4) * 4;
    }
    Err(invalid("no XOR-MAPPED-ADDRESS attribute in STUN response"))
}

fn parse_xor_mapped_address(value: &[u8]) -> io::Result<SocketAddr> {
    if value.len() < 8 {
        return Err(invalid("XOR-MAPPED-ADDRESS attribute too short"));
    }
    if value[1] != 0x01 {
        return Err(invalid("only IPv4 XOR-MAPPED-ADDRESS is supported"));
    }
    let xor_port = u16::from_be_bytes([value[2], value[3]]);
    let port = xor_port ^ (MAGIC_COOKIE >> 16) as u16;

    let mut xor_ip = [0u8; 4];
    xor_ip.copy_from_slice(&value[4..8]);
    let ip = u32::from_be_bytes(xor_ip) ^ MAGIC_COOKIE;

    Ok(SocketAddr::new(Ipv4Addr::from(ip).into(), port))
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Собирает готовый Binding Response с `addr` в XOR-MAPPED-ADDRESS — так,
    /// как это сделал бы настоящий STUN-сервер.
    fn canned_response(txn_id: &[u8; 12], addr: SocketAddr) -> Vec<u8> {
        let SocketAddr::V4(addr) = addr else {
            panic!("test only supports IPv4")
        };

        let xor_port = addr.port() ^ (MAGIC_COOKIE >> 16) as u16;
        let xor_ip = u32::from_be_bytes(addr.ip().octets()) ^ MAGIC_COOKIE;

        let mut attr_value = Vec::new();
        attr_value.push(0x00);
        attr_value.push(0x01);
        attr_value.extend_from_slice(&xor_port.to_be_bytes());
        attr_value.extend_from_slice(&xor_ip.to_be_bytes());

        let mut msg = Vec::new();
        msg.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
        msg.extend_from_slice(&(attr_value.len() as u16 + 4).to_be_bytes());
        msg.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        msg.extend_from_slice(txn_id);
        msg.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
        msg.extend_from_slice(&(attr_value.len() as u16).to_be_bytes());
        msg.extend_from_slice(&attr_value);
        msg
    }

    #[test]
    fn parses_xor_mapped_address() {
        let txn_id = [7u8; 12];
        let addr: SocketAddr = "203.0.113.42:54321".parse().unwrap();
        let response = canned_response(&txn_id, addr);

        let parsed = parse_binding_response(&response, &txn_id).unwrap();
        assert_eq!(parsed, addr);
    }

    #[test]
    fn rejects_mismatched_transaction_id() {
        let txn_id = [7u8; 12];
        let other_txn_id = [9u8; 12];
        let addr: SocketAddr = "203.0.113.42:54321".parse().unwrap();
        let response = canned_response(&txn_id, addr);

        assert!(parse_binding_response(&response, &other_txn_id).is_err());
    }

    /// Ответ ждём среди чужого трафика: мусор, ответ с другим transaction id и
    /// пакеты не от сервера не должны ломать запрос.
    #[tokio::test]
    async fn query_skips_stray_packets_before_the_real_response() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();
        let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mapped: SocketAddr = "203.0.113.42:54321".parse().unwrap();

        let serve = async {
            let mut buf = [0u8; 64];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 20);
            let mut txn_id = [0u8; 12];
            txn_id.copy_from_slice(&buf[8..20]);
            // Сначала чужие пакеты: мусор, чужой отправитель, ответ с другим id.
            stranger.send_to(&canned_response(&txn_id, "198.51.100.1:1".parse().unwrap()), client_addr).await.unwrap();
            server.send_to(b"garbage that is not stun", from).await.unwrap();
            server.send_to(&canned_response(&[9u8; 12], "198.51.100.2:2".parse().unwrap()), from).await.unwrap();
            // И настоящий ответ.
            server.send_to(&canned_response(&txn_id, mapped), from).await.unwrap();
        };

        let (result, ()) = tokio::join!(query(&client, server_addr), serve);
        assert_eq!(result.unwrap(), mapped);
    }
}

#[cfg(test)]
mod list_tests {
    use super::*;

    #[test]
    fn server_list_accepts_one_or_many_and_drops_duplicates() {
        assert_eq!(parse_servers("1.2.3.4:3499").unwrap().len(), 1);
        let two = parse_servers(" 1.2.3.4:3499 , 5.6.7.8:19302,1.2.3.4:3499").unwrap();
        assert_eq!(two, vec!["1.2.3.4:3499".parse().unwrap(), "5.6.7.8:19302".parse().unwrap()]);
    }

    #[test]
    fn server_list_rejects_empty_and_bad_entries() {
        assert!(parse_servers("").is_err());
        assert!(parse_servers(" , ").is_err());
        let error = parse_servers("1.2.3.4:1,nope").unwrap_err().to_string();
        assert!(error.contains("nope"), "{error}");
    }

    #[test]
    fn nat_hint_only_when_servers_disagree() {
        let (a, b): (SocketAddr, SocketAddr) = ("1.1.1.1:1".parse().unwrap(), "2.2.2.2:2".parse().unwrap());
        let seen = |s: &str| -> SocketAddr { s.parse().unwrap() };
        assert_eq!(describe_nat(&[]), None);
        assert_eq!(describe_nat(&[(a, seen("9.9.9.9:40000"))]), None);
        assert_eq!(describe_nat(&[(a, seen("9.9.9.9:40000")), (b, seen("9.9.9.9:40000"))]), None);
        let port_differs = describe_nat(&[(a, seen("9.9.9.9:40000")), (b, seen("9.9.9.9:40001"))]).unwrap();
        assert!(port_differs.contains("симметричный"), "{port_differs}");
        let ip_differs = describe_nat(&[(a, seen("9.9.9.9:40000")), (b, seen("8.8.8.8:40000"))]).unwrap();
        assert!(ip_differs.contains("маршрут"), "{ip_differs}");
    }
}
