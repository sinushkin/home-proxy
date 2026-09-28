//! Маршруты роутера (`ROUTES=auto`, по умолчанию): весь трафик — в туннель, кроме того, на чём
//! туннель держится.
//!
//! - `0.0.0.0/1` и `128.0.0.0/1` в TUN перекрывают маршрут по умолчанию аплинка, но не заменяют
//!   его: default от netifd (DHCP) остаётся на месте, а когда служба останавливается и TUN
//!   исчезает, ядро само убирает маршруты через него — трафик возвращается на аплинк.
//! - /32 до VPS, STUN и MQTT — через шлюз и интерфейс аплинка.
//! - Сокеты дыр (VPS и телефонов) привязаны к интерфейсу аплинка (`SO_BINDTOIFINDEX`): адрес
//!   телефона заранее неизвестен, а пробивать надо с того адреса, который видел STUN.
//!
//! Аплинк — default в таблице main с наименьшей метрикой, кроме самого TUN. Раз в 10 с маршруты
//! сверяются: снесённые (`network reload`) ставятся заново; если аплинк сменился (другой default,
//! Wi-Fi-клиент пересоздан с новым номером интерфейса), служба выходит с ошибкой — procd
//! перезапускает её, и сокеты привязываются к новому аплинку.
//!
//! Хуки (`Hooks`): после маршрутов подъёма TUN — `on-tun-up.sh`, при остановке службы после
//! снятия /1 — `on-tun-down.sh` (рядом с `router.env`, если файлы есть; работают и при
//! `ROUTES=off` — тогда маршруты можно ставить в них самим). Переменные — см. `Hooks::env`.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

/// Физический выход в интернет.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uplink {
    pub gateway: Option<Ipv4Addr>,
    pub dev: String,
    pub ifindex: u32,
}

impl std::fmt::Display for Uplink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.gateway {
            Some(gw) => write!(f, "{} через {gw}", self.dev),
            None => write!(f, "{}", self.dev),
        }
    }
}

pub struct Routes {
    tun: String,
    /// Адреса, которые идут мимо туннеля: VPS, STUN, MQTT.
    bypass: Vec<Ipv4Addr>,
    pub uplink: Uplink,
    /// TUN поднят: пора держать /1 в нём.
    tunnel_up: bool,
}

impl Routes {
    /// Находит аплинк и ставит маршруты в обход туннеля (до старта дыр). `probe` — адрес, путь к
    /// которому подсказывает аплинк, если default в main нет (его заменили руками на TUN).
    ///
    /// Команды (пример: аплинк — Wi-Fi-клиент `phy0-sta1`, шлюз 192.168.17.1, VPS 203.0.113.30):
    /// ```sh
    /// ip -4 route show                       # ищем default с наименьшей метрикой, кроме hp0
    /// # default нет — узнаём аплинк по пути до VPS и возвращаем default:
    /// ip -4 route get 203.0.113.30           # -> 203.0.113.30 via 192.168.17.1 dev phy0-sta1 …
    /// ip route replace default via 192.168.17.1 dev phy0-sta1
    /// cat /sys/class/net/phy0-sta1/ifindex   # номер интерфейса для SO_BINDTOIFINDEX (файл, не ip)
    /// ```
    /// затем недостающие /32 в обход туннеля — см. `missing`.
    pub async fn start(tun: &str, mut bypass: Vec<Ipv4Addr>, probe: Ipv4Addr) -> Result<Self> {
        bypass.sort_unstable();
        bypass.dedup();
        let table = ip(&["-4", "route", "show"]).await?;
        let uplink = match default_route(&table, tun) {
            Some((gateway, dev)) => uplink(gateway, dev)?,
            None => {
                // default пропал (заменён на TUN прошлого запуска и исчез вместе с ним) — берём
                // путь до VPS и возвращаем default аплинка: без него привязанные к интерфейсу
                // сокеты некуда отправить.
                let path = ip(&["-4", "route", "get", &probe.to_string()]).await?;
                let (gateway, dev) = route_get(&path).filter(|(_, dev)| dev != tun).context(
                    "не найден выход в интернет: в таблице main нет default, кроме туннеля (ROUTES=off — маршруты вручную)",
                )?;
                let up = uplink(gateway, dev)?;
                log::warn!("маршруты: в main нет default — возвращаю default через {up}");
                ip(&route_args("replace", "default", &up)).await?;
                up
            }
        };
        log::info!("маршруты: аплинк {} (интерфейс {}), мимо туннеля {:?}", uplink, uplink.ifindex, bypass);
        let mut routes = Self { tun: tun.to_string(), bypass, uplink, tunnel_up: false };
        routes.ensure().await?;
        Ok(routes)
    }

    /// TUN поднят: весь остальной трафик — в него.
    /// ```sh
    /// ip -4 route show
    /// ip route replace 0.0.0.0/1 dev hp0        # если нет
    /// ip route replace 128.0.0.0/1 dev hp0      # если нет
    /// ip route del default dev hp0              # если default в hp0 поставили руками
    /// ```
    pub async fn tunnel_up(&mut self) -> Result<()> {
        self.tunnel_up = true;
        self.ensure().await?;
        log::info!("маршруты: всё в {}, кроме {:?} (через {})", self.tun, self.bypass, self.uplink);
        Ok(())
    }

    /// Служба останавливается: весь трафик — обратно на аплинк, не дожидаясь, пока исчезнет TUN.
    /// Ошибки не важны (маршрутов может уже не быть).
    /// ```sh
    /// ip route del 0.0.0.0/1 dev hp0
    /// ip route del 128.0.0.0/1 dev hp0
    /// ```
    /// /32 до VPS, STUN и MQTT остаются: они и так идут через аплинк.
    pub async fn tunnel_down(&mut self) {
        self.tunnel_up = false;
        for half in ["0.0.0.0/1", "128.0.0.0/1"] {
            if let Err(e) = ip(&["route", "del", half, "dev", &self.tun]).await {
                log::debug!("маршруты: {e:#}");
            }
        }
        log::info!("маршруты: туннель снят, всё через {}", self.uplink);
    }

    /// Сверка раз в 10 с. Ошибка — аплинк сменился: сокеты дыр привязаны к старому, нужен
    /// перезапуск службы.
    /// ```sh
    /// ip -4 route show                          # default аплинка тот же? чего не хватает?
    /// cat /sys/class/net/phy0-sta1/ifindex      # номер интерфейса не сменился?
    /// ip route replace …                        # только недостающие маршруты, см. `missing`
    /// ```
    pub async fn check(&mut self) -> Result<()> {
        let table = ip(&["-4", "route", "show"]).await?;
        if let Some((gateway, dev)) = default_route(&table, &self.tun) {
            let now = uplink(gateway, dev);
            if now.as_ref().ok() != Some(&self.uplink) {
                let now = now.map(|u| format!("{u} (интерфейс {})", u.ifindex)).unwrap_or_else(|e| format!("{e:#}"));
                anyhow::bail!("аплинк сменился: был {} (интерфейс {}), теперь {now} — перезапуск", self.uplink, self.uplink.ifindex);
            }
        }
        // default нет — аплинк временно лёг (DHCP, Wi-Fi переподключается): ждём.
        self.ensure_with(&table).await
    }

    /// `ip -4 route show`, затем недостающие маршруты (`ensure_with`).
    async fn ensure(&mut self) -> Result<()> {
        let table = ip(&["-4", "route", "show"]).await?;
        self.ensure_with(&table).await
    }

    /// Ставит недостающие маршруты (по снимку `ip route show`): по одной команде `ip` из `missing`.
    async fn ensure_with(&self, table: &str) -> Result<()> {
        for missing in missing(table, &self.tun, &self.bypass, &self.uplink, self.tunnel_up) {
            log::info!("маршруты: ip {}", missing.join(" "));
            let args: Vec<&str> = missing.iter().map(String::as_str).collect();
            ip(&args).await?;
        }
        Ok(())
    }
}

/// Скрипты подъёма и остановки туннеля. Запускаются через `/bin/sh <файл>` (на OpenWrt bash нет,
/// права на исполнение не нужны), если файл существует; ждём не дольше `HOOK_TIMEOUT`, код
/// возврата и вывод — в лог, на работу службы они не влияют.
pub struct Hooks {
    pub up: PathBuf,
    pub down: PathBuf,
    /// Переменные окружения скриптов (кроме `HP_EVENT`):
    /// - `TUN_DEV` — имя TUN (`hp0`), `TUN_ADDR` / `TUN_PREFIX` — адрес в туннеле от VPS
    ///   (`10.80.0.2`, `16`), `TUN_DNS` — DNS от VPS через пробел;
    /// - `VPS_IP` — адрес VPS, `BYPASS` — адреса мимо туннеля (VPS, STUN, MQTT) через пробел;
    /// - `ROUTES` — `auto` или `off`; при `auto` ещё `UPLINK_DEV`, `UPLINK_GW` (пусто — без
    ///   шлюза), `UPLINK_IFINDEX` — аплинк, через который идут дыры.
    pub env: Vec<(&'static str, String)>,
}

const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

impl Hooks {
    /// `HP_EVENT=up sh on-tun-up.sh` — после маршрутов туннеля (`Routes::tunnel_up`).
    pub async fn up(&self) {
        run_hook(&self.up, "up", &self.env).await;
    }

    /// `HP_EVENT=down sh on-tun-down.sh` — после снятия маршрутов (`Routes::tunnel_down`), TUN ещё
    /// существует.
    pub async fn down(&self) {
        run_hook(&self.down, "down", &self.env).await;
    }
}

/// `/bin/sh <script>` с переменными `env` и `HP_EVENT=<event>`; нет файла — ничего не делает.
async fn run_hook(script: &std::path::Path, event: &str, env: &[(&'static str, String)]) {
    if !script.is_file() {
        log::debug!("хук {}: файла нет", script.display());
        return;
    }
    log::info!("хук: sh {} (HP_EVENT={event})", script.display());
    let mut command = tokio::process::Command::new("/bin/sh");
    command.arg(script).env("HP_EVENT", event).envs(env.iter().map(|(k, v)| (*k, v))).kill_on_drop(true);
    if let Some(dir) = script.parent() {
        command.current_dir(dir);
    }
    match tokio::time::timeout(HOOK_TIMEOUT, command.output()).await {
        Err(_) => log::warn!("хук {}: не завершился за {HOOK_TIMEOUT:?}, остановлен", script.display()),
        Ok(Err(e)) => log::warn!("хук {}: не запустился: {e}", script.display()),
        Ok(Ok(out)) => {
            for line in String::from_utf8_lossy(&out.stdout).lines().chain(String::from_utf8_lossy(&out.stderr).lines()) {
                log::info!("хук {}: {line}", script.file_name().unwrap_or_default().to_string_lossy());
            }
            if !out.status.success() {
                log::warn!("хук {}: {}", script.display(), out.status);
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
