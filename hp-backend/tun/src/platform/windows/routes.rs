//! Маршруты через PowerShell (`Get-NetRoute`, `New-NetRoute`, `Find-NetRoute`). Интерфейсы
//! задаём по номеру (`InterfaceIndex`): имя адаптера на русской Windows бывает не ASCII, а вывод
//! консоли — в OEM-кодировке. Туннель ищем по нашему имени (`hp0`).
//!
//! Маршрут выбирается по длине префикса, затем по сумме метрик маршрута и интерфейса. `/1` в
//! туннель длиннее `/0`, но у чужого VPN (OpenVPN, WireGuard) бывают такие же `/1`: поэтому
//! метрика интерфейса туннеля сбрасывается в 1 (у Wintun по умолчанию её назначает система).

use std::net::Ipv4Addr;

use anyhow::{Context, Result};

use super::powershell::{quote, run};
use crate::platform::RouteBackend;
use crate::routes::Uplink;

#[derive(Default)]
pub struct PsRoutes;

/// Метка строк с аплинком в выводе скрипта: `UPLINK|номер|шлюз|имя`.
const MARK: &str = "UPLINK|";

/// Метрика интерфейса туннеля: меньше, чем у любого VPN, который получает метрику от системы.
const TUNNEL_INTERFACE_METRIC: u32 = 1;

/// Разбор `UPLINK|12|192.168.122.1|Ethernet 2`; шлюз `0.0.0.0` — маршрут на канале, без шлюза.
fn parse_uplink(output: &str) -> Result<Option<Uplink>> {
    let Some(line) = output.lines().map(str::trim).find_map(|l| l.strip_prefix(MARK)) else { return Ok(None) };
    let mut parts = line.splitn(3, '|');
    let ifindex: u32 = parts.next().unwrap_or_default().trim().parse().context("номер интерфейса аплинка")?;
    let gateway: Ipv4Addr = parts.next().unwrap_or_default().trim().parse().context("шлюз аплинка")?;
    let dev = parts.next().unwrap_or_default().trim().to_string();
    Ok(Some(Uplink { gateway: (!gateway.is_unspecified()).then_some(gateway), dev, ifindex }))
}

/// Выбор маршрутов `candidates` (PowerShell-выражение): печатает первый как `UPLINK|…`.
const PRINT_FIRST: &str = "| Select-Object -First 1 | ForEach-Object { 'UPLINK|{0}|{1}|{2}' -f $_.I, $_.G, $_.A }";

impl RouteBackend for PsRoutes {
    /// Маршрут по умолчанию с наименьшей суммой метрик (маршрут + интерфейс), кроме туннеля.
    async fn current_uplink(&self, tun: &str) -> Result<Option<Uplink>> {
        let script = format!(
            "Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue \
             | Where-Object {{ $_.InterfaceAlias -ne {tun} }} \
             | ForEach-Object {{ $m = (Get-NetIPInterface -InterfaceIndex $_.InterfaceIndex -AddressFamily IPv4).InterfaceMetric; \
                 [pscustomobject]@{{ I = $_.InterfaceIndex; G = $_.NextHop; A = $_.InterfaceAlias; M = $_.RouteMetric + $m }} }} \
             | Sort-Object M {PRINT_FIRST}",
            tun = quote(tun),
        );
        parse_uplink(&run(&script).await?)
    }

    /// Путь до `probe`, как его выбрала бы система (`Find-NetRoute`), кроме туннеля.
    async fn probe_uplink(&self, tun: &str, probe: Ipv4Addr) -> Result<Option<Uplink>> {
        let script = format!(
            "Find-NetRoute -RemoteIPAddress '{probe}' -ErrorAction SilentlyContinue \
             | Where-Object {{ $_.DestinationPrefix -and $_.InterfaceAlias -ne {tun} }} \
             | ForEach-Object {{ [pscustomobject]@{{ I = $_.InterfaceIndex; G = $_.NextHop; A = $_.InterfaceAlias }} }} {PRINT_FIRST}",
            tun = quote(tun),
        );
        parse_uplink(&run(&script).await?)
    }

    async fn restore_default(&self, up: &Uplink) -> Result<()> {
        let hop = up.gateway.unwrap_or(Ipv4Addr::UNSPECIFIED);
        run(&format!(
            "New-NetRoute -DestinationPrefix '0.0.0.0/0' -InterfaceIndex {} -NextHop '{hop}' -RouteMetric 0 -PolicyStore ActiveStore | Out-Null",
            up.ifindex
        ))
        .await
        .map(drop)
    }

    async fn sync(&self, tun: &str, bypass: &[Ipv4Addr], up: &Uplink, tunnel_up: bool) -> Result<()> {
        let hop = up.gateway.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let mut script = String::from(
            "function Ensure($prefix, $index, $hop) {\n\
               $have = Get-NetRoute -AddressFamily IPv4 -DestinationPrefix $prefix -InterfaceIndex $index -ErrorAction SilentlyContinue | Where-Object { $_.NextHop -eq $hop }\n\
               if (-not $have) {\n\
                 New-NetRoute -DestinationPrefix $prefix -InterfaceIndex $index -NextHop $hop -RouteMetric 0 -PolicyStore ActiveStore | Out-Null\n\
                 \"маршруты: добавлен $prefix через $hop (интерфейс $index)\"\n\
               }\n\
             }\n",
        );
        for host in bypass {
            script.push_str(&format!("Ensure '{host}/32' {} '{hop}'\n", up.ifindex));
        }
        if tunnel_up {
            script.push_str(&format!(
                "$t = (Get-NetIPInterface -InterfaceAlias {tun} -AddressFamily IPv4).InterfaceIndex\n\
                 if ((Get-NetIPInterface -InterfaceIndex $t -AddressFamily IPv4).InterfaceMetric -ne {TUNNEL_INTERFACE_METRIC}) {{\n\
                   Set-NetIPInterface -InterfaceIndex $t -AddressFamily IPv4 -InterfaceMetric {TUNNEL_INTERFACE_METRIC} -AutomaticMetric Disabled\n\
                 }}\n\
                 Ensure '0.0.0.0/1' $t '0.0.0.0'\n\
                 Ensure '128.0.0.0/1' $t '0.0.0.0'\n\
                 Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -InterfaceIndex $t -ErrorAction SilentlyContinue | Remove-NetRoute -Confirm:$false\n",
                tun = quote(tun),
            ));
        }
        for line in run(&script).await?.lines().map(str::trim).filter(|l| !l.is_empty()) {
            log::info!("{line}");
        }
        Ok(())
    }

    async fn remove_tunnel(&self, tun: &str) {
        let script = format!(
            "$ErrorActionPreference = 'SilentlyContinue'\n\
             Remove-NetRoute -DestinationPrefix '0.0.0.0/1' -InterfaceAlias {tun} -Confirm:$false\n\
             Remove-NetRoute -DestinationPrefix '128.0.0.0/1' -InterfaceAlias {tun} -Confirm:$false\n",
            tun = quote(tun),
        );
        if let Err(e) = run(&script).await {
            log::debug!("маршруты: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uplink_is_read_from_the_marked_line_only() {
        let out = "шум\nUPLINK|12|192.168.122.1|Ethernet 2\n";
        let up = parse_uplink(out).unwrap().unwrap();
        assert_eq!(up, Uplink { gateway: Some("192.168.122.1".parse().unwrap()), dev: "Ethernet 2".into(), ifindex: 12 });
    }

    #[test]
    fn on_link_route_has_no_gateway_and_no_line_means_no_uplink() {
        assert_eq!(parse_uplink("UPLINK|7|0.0.0.0|PPP").unwrap().unwrap().gateway, None);
        assert_eq!(parse_uplink("").unwrap(), None);
        assert!(parse_uplink("UPLINK|x|1.1.1.1|a").is_err());
    }
}
