//! Маршруты через команду `ip` (iproute2 или busybox на OpenWrt) и `/sys/class/net`.

use std::net::Ipv4Addr;

use anyhow::{Context, Result};

use crate::platform::RouteBackend;
use crate::routes::Uplink;

#[derive(Default)]
pub struct IpRoutes;

impl RouteBackend for IpRoutes {
    /// ```sh
    /// ip -4 route show                       # default с наименьшей метрикой, кроме hp0
    /// cat /sys/class/net/phy0-sta1/ifindex   # номер интерфейса для SO_BINDTOIFINDEX (файл, не ip)
    /// ```
    async fn current_uplink(&self, tun: &str) -> Result<Option<Uplink>> {
        let table = ip(&["-4", "route", "show"]).await?;
        default_route(&table, tun).map(|(gateway, dev)| uplink(gateway, dev)).transpose()
    }

    /// ```sh
    /// ip -4 route get 203.0.113.30           # -> 203.0.113.30 via 192.168.17.1 dev phy0-sta1 …
    /// ```
    async fn probe_uplink(&self, tun: &str, probe: Ipv4Addr) -> Result<Option<Uplink>> {
        let path = ip(&["-4", "route", "get", &probe.to_string()]).await?;
        route_get(&path).filter(|(_, dev)| dev != tun).map(|(gateway, dev)| uplink(gateway, dev)).transpose()
    }

    /// ```sh
    /// ip route replace default via 192.168.17.1 dev phy0-sta1
    /// ```
    async fn restore_default(&self, up: &Uplink) -> Result<()> {
        ip(&route_args("replace", "default", up)).await.map(drop)
    }

    /// Снимок `ip -4 route show`, затем по одной команде `ip` на недостающее — см. `missing`.
    async fn sync(&self, tun: &str, bypass: &[Ipv4Addr], up: &Uplink, tunnel_up: bool) -> Result<()> {
        let table = ip(&["-4", "route", "show"]).await?;
        for missing in missing(&table, tun, bypass, up, tunnel_up) {
            log::info!("маршруты: ip {}", missing.join(" "));
            let args: Vec<&str> = missing.iter().map(String::as_str).collect();
            ip(&args).await?;
        }
        Ok(())
    }

    /// ```sh
    /// ip route del 0.0.0.0/1 dev hp0
    /// ip route del 128.0.0.0/1 dev hp0
    /// ```
    async fn remove_tunnel(&self, tun: &str) {
        for half in ["0.0.0.0/1", "128.0.0.0/1"] {
            if let Err(e) = ip(&["route", "del", half, "dev", tun]).await {
                log::debug!("маршруты: {e:#}");
            }
        }
    }
}

/// Номер интерфейса — из `/sys/class/net/<dev>/ifindex` (как `cat`, без запуска `ip`).
fn uplink(gateway: Option<Ipv4Addr>, dev: String) -> Result<Uplink> {
    let path = format!("/sys/class/net/{dev}/ifindex");
    let ifindex = std::fs::read_to_string(&path).with_context(|| format!("не прочитать {path}"))?.trim().parse().context("ifindex")?;
    Ok(Uplink { gateway, dev, ifindex })
}

/// Аргументы `ip`: `route <verb> <target> [via <шлюз>] dev <аплинк>`, например
/// `ip route replace 203.0.113.30 via 192.168.17.1 dev phy0-sta1`.
fn route_args(verb: &str, target: &str, up: &Uplink) -> Vec<String> {
    let mut args = vec!["route".to_string(), verb.into(), target.into()];
    if let Some(gw) = up.gateway {
        args.extend(["via".into(), gw.to_string()]);
    }
    args.extend(["dev".into(), up.dev.clone()]);
    args
}

/// Команды `ip`, которых не хватает в таблице: /32 в обход, /1 в TUN, лишний default в TUN —
/// убрать (его ставили руками вместо default аплинка). Сами не выполняет, только составляет:
/// ```sh
/// ip route replace <VPS, STUN, MQTT> via <шлюз аплинка> dev <аплинк>   # на каждый адрес
/// ip route replace 0.0.0.0/1 dev hp0                                  # когда TUN поднят
/// ip route replace 128.0.0.0/1 dev hp0
/// ip route del default dev hp0
/// ```
fn missing(table: &str, tun: &str, bypass: &[Ipv4Addr], up: &Uplink, tunnel_up: bool) -> Vec<Vec<String>> {
    let lines: Vec<Vec<&str>> = table.lines().map(|l| l.split_whitespace().collect()).collect();
    let has = |want: &[String]| lines.iter().any(|l| l.len() >= want.len() && l.iter().zip(want).all(|(a, b)| a == b));
    let mut out = Vec::new();
    for host in bypass {
        let want = route_args("replace", &host.to_string(), up);
        if !has(&want[2..]) {
            out.push(want);
        }
    }
    if tunnel_up {
        for half in ["0.0.0.0/1", "128.0.0.0/1"] {
            let want: Vec<String> = ["route", "replace", half, "dev", tun].map(String::from).into();
            if !has(&want[2..]) {
                out.push(want);
            }
        }
        if lines.iter().any(|l| l.first() == Some(&"default") && dev_of(l) == Some(tun)) {
            out.push(["route", "del", "default", "dev", tun].map(String::from).into());
        }
    }
    out
}

fn dev_of<'a>(words: &[&'a str]) -> Option<&'a str> {
    words.iter().position(|w| *w == "dev").and_then(|i| words.get(i + 1).copied())
}

fn via_of(words: &[&str]) -> Option<Ipv4Addr> {
    words.iter().position(|w| *w == "via").and_then(|i| words.get(i + 1)).and_then(|a| a.parse().ok())
}

/// default с наименьшей метрикой, кроме TUN: (шлюз, интерфейс).
fn default_route(table: &str, tun: &str) -> Option<(Option<Ipv4Addr>, String)> {
    table
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|w| w.first() == Some(&"default"))
        .filter_map(|w| {
            let dev = dev_of(&w).filter(|d| *d != tun)?;
            let metric: u32 = w.iter().position(|x| *x == "metric").and_then(|i| w.get(i + 1)).and_then(|m| m.parse().ok()).unwrap_or(0);
            Some((metric, via_of(&w), dev.to_string()))
        })
        .min_by_key(|(metric, ..)| *metric)
        .map(|(_, via, dev)| (via, dev))
}

/// Разбор `ip route get`: `203.0.113.30 via 192.168.17.1 dev phy0-sta1  src …`.
fn route_get(text: &str) -> Option<(Option<Ipv4Addr>, String)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    Some((via_of(&words), dev_of(&words)?.to_string()))
}

/// Запускает `ip <args>` (на OpenWrt — busybox) и возвращает его вывод; ненулевой код — ошибка с
/// текстом stderr.
async fn ip(args: &[impl AsRef<std::ffi::OsStr>]) -> Result<String> {
    let out = tokio::process::Command::new("ip").args(args).output().await.context("не удалось запустить ip")?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    let shown = || args.iter().map(|a| a.as_ref().to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
    anyhow::ensure!(out.status.success(), "ip {}: {}", shown(), text(&out.stderr));
    Ok(text(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "default via 192.168.3.1 dev eth0.2  metric 20 \n\
                         default via 192.168.17.1 dev phy0-sta1 \n\
                         192.168.1.0/24 dev br-lan scope link  src 192.168.1.1 \n";

    fn up() -> Uplink {
        Uplink { gateway: Some("192.168.17.1".parse().unwrap()), dev: "phy0-sta1".into(), ifindex: 12 }
    }

    #[test]
    fn default_with_lowest_metric_wins_and_tunnel_is_not_an_uplink() {
        assert_eq!(default_route(TABLE, "hp0"), Some((Some("192.168.17.1".parse().unwrap()), "phy0-sta1".into())));
        assert_eq!(default_route("default via 10.80.0.1 dev hp0 \n", "hp0"), None);
        assert_eq!(default_route("default dev pppoe-wan scope link \n", "hp0"), Some((None, "pppoe-wan".into())));
    }

    #[test]
    fn route_get_gives_gateway_and_device() {
        let text = "203.0.113.30 via 192.168.17.1 dev phy0-sta1  src 192.168.17.139 ";
        assert_eq!(route_get(text), Some((Some("192.168.17.1".parse().unwrap()), "phy0-sta1".into())));
    }

    #[test]
    fn missing_routes_are_added_and_present_ones_kept() {
        let vps: Ipv4Addr = "203.0.113.30".parse().unwrap();
        let stun: Ipv4Addr = "203.0.113.10".parse().unwrap();
        let table = "default via 10.80.0.1 dev hp0 \n\
                     203.0.113.30 via 192.168.17.1 dev phy0-sta1 \n\
                     0.0.0.0/1 dev hp0 scope link \n";
        let cmds: Vec<String> = missing(table, "hp0", &[vps, stun], &up(), true).iter().map(|c| c.join(" ")).collect();
        assert_eq!(
            cmds,
            [
                "route replace 203.0.113.10 via 192.168.17.1 dev phy0-sta1",
                "route replace 128.0.0.0/1 dev hp0",
                "route del default dev hp0",
            ]
        );
        // Пока TUN не поднят — только обход.
        assert_eq!(missing("", "hp0", &[vps], &up(), false).len(), 1);
    }
}
