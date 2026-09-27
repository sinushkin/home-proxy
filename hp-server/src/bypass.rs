//! `BIND_ADDR`: к какому адаптеру привязать сокеты дыр и STUN, чтобы они шли мимо VPN этой машины.
//!
//! `auto` (по умолчанию): STUN-запрос с обычного сокета (маршрут по умолчанию) сравнивается с
//! запросами с сокетов, привязанных к каждому адресу машины (на Linux — ещё и к его интерфейсу).
//! Внешние адреса совпали — VPN дыры не перехватывает (или его нет), привязка не нужна. Через
//! какой-то адаптер внешний адрес другой, или по маршруту по умолчанию STUN не отвечает вовсе —
//! маршрут по умолчанию ведёт в VPN, и сокеты дыр привязываются к этому адаптеру. Проверка — один
//! раз при запуске: VPN, включённый позже, потребует перезапуска.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result};
use connection::stun;

/// Сколько ждать ответ STUN на одну попытку и сколько попыток на сервер.
const STUN_WAIT: Duration = Duration::from_millis(800);
const STUN_ATTEMPTS: usize = 2;

/// Значение `BIND_ADDR`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindSetting {
    /// Найти адаптер мимо VPN самим (STUN).
    Auto,
    /// Не привязывать: сокеты на `0.0.0.0`.
    Off,
    /// Этот адрес (и, на Linux, его интерфейс).
    Addr(IpAddr),
}

impl BindSetting {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.map(str::trim) {
            None | Some("") | Some("auto") => Ok(Self::Auto),
            Some("off") | Some("any") | Some("0.0.0.0") => Ok(Self::Off),
            Some(ip) => Ok(Self::Addr(ip.parse().context("BIND_ADDR: ожидается IP-адрес, auto или off")?)),
        }
    }
}

/// К чему привязаны сокеты дыр.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Binding {
    pub ip: Option<IpAddr>,
    pub ifindex: Option<u32>,
}

/// Адрес машины, через который можно отправить пакет.
#[derive(Clone, Debug)]
struct Candidate {
    name: String,
    ip: Ipv4Addr,
    index: Option<u32>,
}

/// IPv4-адреса машины, кроме loopback, link-local и подсети туннеля (`tunnel`).
fn candidates(tunnel: Option<(Ipv4Addr, u8)>) -> Vec<Candidate> {
    let interfaces = match if_addrs::get_if_addrs() {
        Ok(list) => list,
        Err(e) => {
            log::warn!("BIND_ADDR: не удалось получить адреса интерфейсов: {e}");
            return Vec::new();
        }
    };
    let mut found: Vec<Candidate> = Vec::new();
    for interface in interfaces {
        let IpAddr::V4(ip) = interface.ip() else { continue };
        if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || tunnel.is_some_and(|net| in_subnet(ip, net)) {
            continue;
        }
        if !found.iter().any(|c| c.ip == ip) {
            found.push(Candidate { name: interface.name, ip, index: interface.index });
        }
    }
    found
}

fn in_subnet(ip: Ipv4Addr, (net, prefix): (Ipv4Addr, u8)) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
    u32::from(ip) & mask == u32::from(net) & mask
}

/// Что видят STUN-серверы у сокета на `ip` (и интерфейсе `ifindex`): внешний адрес по каждому
/// серверу по порядку, `None` — не ответил. Сравнивать можно только ответы одного сервера: до
/// сервера VPN (часто он же STUN) маршрут идёт мимо туннеля, да и домашняя сеть может выходить к
/// разным серверам с разных адресов.
async fn external(ip: IpAddr, ifindex: Option<u32>, servers: &[SocketAddr]) -> Vec<Option<IpAddr>> {
    let socket = match connection::bind::udp(ip, 0, ifindex).await {
        Ok(socket) => socket,
        Err(e) => {
            log::debug!("BIND_ADDR: сокет на {ip}: {e}");
            return vec![None; servers.len()];
        }
    };
    let mut seen = Vec::with_capacity(servers.len());
    for &server in servers {
        let mut answer = None;
        for _ in 0..STUN_ATTEMPTS {
            match tokio::time::timeout(STUN_WAIT, stun::query(&socket, server)).await {
                Ok(Ok(mapped)) => {
                    answer = Some(mapped.ip());
                    break;
                }
                Ok(Err(e)) => {
                    log::debug!("BIND_ADDR: STUN {server} с {ip}: {e}");
                    break;
                }
                Err(_) => {}
            }
        }
        seen.push(answer);
    }
    seen
}

/// Итоговая привязка по настройке; `servers` — STUN-серверы (без них `auto` не проверить).
pub async fn resolve(setting: BindSetting, servers: &[SocketAddr], tunnel: Option<(Ipv4Addr, u8)>) -> Binding {
    match setting {
        BindSetting::Off => Binding::default(),
        BindSetting::Addr(ip) => {
            let ifindex = candidates(None).into_iter().find(|c| IpAddr::V4(c.ip) == ip).and_then(|c| c.index);
            if ifindex.is_none() && cfg!(target_os = "linux") {
                log::warn!("BIND_ADDR={ip}: интерфейс с таким адресом не найден — привязка только к адресу");
            }
            Binding { ip: Some(ip), ifindex }
        }
        BindSetting::Auto if servers.is_empty() => Binding::default(),
        BindSetting::Auto => detect(servers, tunnel).await,
    }
}

/// «STUN → внешний адрес» по всем серверам — для лога.
fn describe(servers: &[SocketAddr], seen: &[Option<IpAddr>]) -> String {
    let parts: Vec<String> = servers
        .iter()
        .zip(seen)
        .map(|(server, ext)| format!("{server} → {}", ext.map(|a| a.to_string()).unwrap_or_else(|| "нет ответа".into())))
        .collect();
    parts.join(", ")
}

async fn detect(servers: &[SocketAddr], tunnel: Option<(Ipv4Addr, u8)>) -> Binding {
    let list = candidates(tunnel);
    let mut probes = tokio::task::JoinSet::new();
    for (i, candidate) in list.iter().enumerate() {
        let (ip, index, servers) = (IpAddr::V4(candidate.ip), candidate.index, servers.to_vec());
        probes.spawn(async move { (i, external(ip, index, &servers).await) });
    }
    let default = external(IpAddr::V4(Ipv4Addr::UNSPECIFIED), None, servers).await;
    let mut seen = vec![vec![None; servers.len()]; list.len()];
    while let Some(Ok((i, ext))) = probes.join_next().await {
        seen[i] = ext;
    }
    log::debug!("BIND_ADDR: маршрут по умолчанию: {}", describe(servers, &default));
    for (candidate, ext) in list.iter().zip(&seen) {
        log::debug!("BIND_ADDR: {} {}: {}", candidate.name, candidate.ip, describe(servers, ext));
    }
    let Some(pick) = choose(&default, &list, &seen) else {
        if default.iter().any(Option::is_some) {
            log::info!("BIND_ADDR: VPN дыры не перехватывает ({}), привязка не нужна", describe(servers, &default));
        } else {
            log::warn!("BIND_ADDR: STUN не отвечает ни по маршруту по умолчанию, ни через адаптеры — сокеты дыр без привязки");
        }
        return Binding::default();
    };
    let candidate = &list[pick];
    log::info!(
        "BIND_ADDR: маршрут по умолчанию идёт через VPN ({}), через {} {} — мимо ({}): дыры и STUN привязаны к {}",
        describe(servers, &default),
        candidate.name,
        candidate.ip,
        describe(servers, &seen[pick]),
        candidate.name
    );
    Binding { ip: Some(IpAddr::V4(candidate.ip)), ifindex: candidate.index }
}

/// Адрес из интернета: не частный, не loopback, не link-local и не CGNAT (100.64/10).
fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || (a == 100 && (b & 0xc0) == 64))
        }
        IpAddr::V6(_) => true,
    }
}

/// Какой адаптер выбрать: тот, у которого хоть один STUN-сервер ответил не так, как по маршруту
/// по умолчанию (другой внешний адрес или ответ там, где по умолчанию тишина) — это путь мимо
/// VPN; из нескольких — сначала частный адрес (домашняя сеть). `None` — обходного пути нет.
///
/// Ответ с частным адресом не в счёт: сервер достигнут внутри той же сети или туннеля. Так
/// STUN на самом сервере VPN видит через туннель внутренний адрес туннеля — это не путь мимо VPN.
fn choose(default: &[Option<IpAddr>], list: &[Candidate], seen: &[Vec<Option<IpAddr>>]) -> Option<usize> {
    let global = |ext: &Option<IpAddr>| ext.filter(|ip| is_global(*ip));
    let differs = |ext: &Vec<Option<IpAddr>>| {
        ext.iter().zip(default).any(|(ext, def)| {
            let (ext, def) = (global(ext), global(def));
            ext.is_some() && ext != def
        })
    };
    let bypass: Vec<usize> = (0..list.len()).filter(|&i| differs(&seen[i])).collect();
    bypass.iter().copied().find(|&i| list[i].ip.is_private()).or_else(|| bypass.first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, ip: [u8; 4]) -> Candidate {
        Candidate { name: name.into(), ip: Ipv4Addr::from(ip), index: Some(1) }
    }

    #[test]
    fn setting_values() {
        assert_eq!(BindSetting::parse(None).unwrap(), BindSetting::Auto);
        assert_eq!(BindSetting::parse(Some(" auto ")).unwrap(), BindSetting::Auto);
        assert_eq!(BindSetting::parse(Some("off")).unwrap(), BindSetting::Off);
        assert_eq!(BindSetting::parse(Some("192.168.1.5")).unwrap(), BindSetting::Addr("192.168.1.5".parse().unwrap()));
        assert!(BindSetting::parse(Some("eth0")).is_err());
    }

    #[test]
    fn picks_adapter_that_bypasses_vpn() {
        let vpn = Some(IpAddr::from([203, 0, 113, 7]));
        let home = Some(IpAddr::from([198, 51, 100, 20]));
        let list = [candidate("tun0", [10, 8, 0, 6]), candidate("docker0", [172, 17, 0, 1]), candidate("eth0", [192, 168, 1, 5])];
        // Один STUN: по умолчанию и через tun0 — адрес VPN, через eth0 — домашний.
        assert_eq!(choose(&[vpn], &list, &[vec![vpn], vec![None], vec![home]]), Some(2));
        // VPN не пропускает STUN вовсе — обход всё равно находится.
        assert_eq!(choose(&[None], &list, &[vec![None], vec![None], vec![home]]), Some(2));
        // Без VPN внешний адрес везде один — привязка не нужна.
        assert_eq!(choose(&[home], &list, &[vec![None], vec![None], vec![home]]), None);
    }

    #[test]
    fn compares_answers_of_the_same_server() {
        // Два STUN: первый — за VPN (по умолчанию молчит), второй — сам сервер VPN, до него
        // маршрут мимо туннеля, и по умолчанию он отвечает «другим» адресом домашней сети.
        let (home, other) = (Some(IpAddr::from([198, 51, 100, 20])), Some(IpAddr::from([198, 51, 100, 99])));
        let list = [candidate("eth0", [192, 168, 1, 5])];
        assert_eq!(choose(&[None, other], &list, &[vec![home, other]]), Some(0));
        // Без VPN оба сервера отвечают одинаково по обоим путям, хоть адреса у серверов и разные.
        assert_eq!(choose(&[home, other], &list, &[vec![home, other]]), None);
    }

    #[test]
    fn tunnel_address_from_the_vpn_server_is_not_a_bypass() {
        // Windows с полным туннелем до сервера, на котором же стоит второй STUN: через туннель он
        // видит внутренний адрес туннеля, а маршрут к нему по умолчанию идёт мимо туннеля.
        let (vpn, grey, home) = (Some(IpAddr::from([203, 0, 113, 10])), Some(IpAddr::from([198, 51, 100, 8])), Some(IpAddr::from([198, 51, 100, 7])));
        let inside = Some(IpAddr::from([10, 8, 0, 10]));
        let list = [candidate("OpenVPN Wintun", [10, 8, 0, 10]), candidate("Ethernet", [192, 168, 122, 33])];
        let default = [vpn, grey];
        assert_eq!(choose(&default, &list, &[vec![vpn, inside], vec![home, grey]]), Some(1));
    }

    #[test]
    fn prefers_private_address() {
        let (vpn, home) = (Some(IpAddr::from([203, 0, 113, 7])), Some(IpAddr::from([198, 51, 100, 20])));
        let list = [candidate("wwan0", [100, 70, 1, 2]), candidate("eth0", [192, 168, 1, 5])];
        assert_eq!(choose(&[vpn], &list, &[vec![home], vec![home]]), Some(1));
    }

    #[test]
    fn tunnel_subnet_is_skipped() {
        assert!(in_subnet(Ipv4Addr::new(10, 80, 1, 7), (Ipv4Addr::new(10, 80, 0, 1), 16)));
        assert!(!in_subnet(Ipv4Addr::new(10, 81, 0, 1), (Ipv4Addr::new(10, 80, 0, 1), 16)));
    }
}
