//! Маршруты клиента туннеля (`ROUTES=auto`, по умолчанию): весь трафик — в туннель, кроме того, на
//! чём туннель держится. Общий модуль `hp-router` и `vps-client`.
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

use crate::platform::{HookLauncher, NativeHooks, NativeRoutes, RouteBackend};

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
    /// Системные операции над таблицей маршрутов (Linux: команда `ip`, см. `platform`).
    backend: NativeRoutes,
}

impl Routes {
    /// Находит аплинк и ставит маршруты в обход туннеля (до старта дыр). `probe` — адрес, путь к
    /// которому подсказывает аплинк, если default в main нет (его заменили руками на TUN).
    ///
    /// Примеры команд Linux-реализации (аплинк — Wi-Fi-клиент `phy0-sta1`, шлюз 192.168.17.1,
    /// VPS 203.0.113.30) — в `platform::linux::routes`: сначала ищем default, а если его нет —
    /// узнаём аплинк по пути до VPS и возвращаем default: без него привязанные к интерфейсу
    /// сокеты некуда отправить. Затем недостающие /32 в обход туннеля.
    pub async fn start(tun: &str, mut bypass: Vec<Ipv4Addr>, probe: Ipv4Addr) -> Result<Self> {
        bypass.sort_unstable();
        bypass.dedup();
        #[allow(clippy::default_constructed_unit_structs)] // тип платформы: у Linux и Windows сейчас без полей
        let backend = NativeRoutes::default();
        let uplink = match backend.current_uplink(tun).await? {
            Some(up) => up,
            None => {
                // default пропал (заменён на TUN прошлого запуска и исчез вместе с ним).
                let up = backend.probe_uplink(tun, probe).await?.context(
                    "не найден выход в интернет: в таблице main нет default, кроме туннеля (ROUTES=off — маршруты вручную)",
                )?;
                log::warn!("маршруты: в main нет default — возвращаю default через {up}");
                backend.restore_default(&up).await?;
                up
            }
        };
        log::info!("маршруты: аплинк {} (интерфейс {}), мимо туннеля {:?}", uplink, uplink.ifindex, bypass);
        let routes = Self { tun: tun.to_string(), bypass, uplink, tunnel_up: false, backend };
        routes.ensure().await?;
        Ok(routes)
    }

    /// TUN поднят: весь остальной трафик — в него (две половины адресного пространства
    /// `0.0.0.0/1` и `128.0.0.0/1`; default аплинка остаётся на месте).
    pub async fn tunnel_up(&mut self) -> Result<()> {
        self.tunnel_up = true;
        self.ensure().await?;
        log::info!("маршруты: всё в {}, кроме {:?} (через {})", self.tun, self.bypass, self.uplink);
        Ok(())
    }

    /// Служба останавливается: весь трафик — обратно на аплинк, не дожидаясь, пока исчезнет TUN.
    /// Ошибки не важны (маршрутов может уже не быть). /32 до VPS, STUN и MQTT остаются: они и так
    /// идут через аплинк.
    pub async fn tunnel_down(&mut self) {
        self.tunnel_up = false;
        self.backend.remove_tunnel(&self.tun).await;
        log::info!("маршруты: туннель снят, всё через {}", self.uplink);
    }

    /// Сверка раз в 10 с. Ошибка — аплинк сменился: сокеты дыр привязаны к старому, нужен
    /// перезапуск службы.
    pub async fn check(&mut self) -> Result<()> {
        // Не прочитали аплинк (интерфейс пропал) — для сокетов дыр это то же, что смена аплинка.
        match self.backend.current_uplink(&self.tun).await {
            Ok(Some(now)) if now != self.uplink => {
                anyhow::bail!("аплинк сменился: был {} (интерфейс {}), теперь {now} (интерфейс {}) — перезапуск", self.uplink, self.uplink.ifindex, now.ifindex);
            }
            Err(e) => {
                anyhow::bail!("аплинк сменился: был {} (интерфейс {}), теперь {e:#} — перезапуск", self.uplink, self.uplink.ifindex);
            }
            // Тот же аплинк, а если default нет — он временно лёг (DHCP, Wi-Fi переподключается): ждём.
            Ok(_) => {}
        }
        self.ensure().await
    }

    /// Недостающие маршруты — как положено в текущем состоянии (`tunnel_up`).
    async fn ensure(&self) -> Result<()> {
        self.backend.sync(&self.tun, &self.bypass, &self.uplink, self.tunnel_up).await
    }
}

/// Скрипты подъёма и остановки туннеля. Запускаются интерпретатором платформы (Linux: `/bin/sh <файл>`,
/// на OpenWrt bash нет, права на исполнение не нужны), если файл существует; ждём не дольше `HOOK_TIMEOUT`, код
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

/// Переменные хуков туннеля — общие для `vps-client` и `hp-router`: адрес и DNS из туннеля,
/// адрес VPS, адреса мимо туннеля и (если маршруты ведём) аплинк. `DNS` — через запятую, `TUN_DNS` —
/// через пробел (для скриптов, которым удобнее список).
pub fn hook_env(
    tun: &str,
    address: (Ipv4Addr, u8),
    dns: &[Ipv4Addr],
    vps_ip: Ipv4Addr,
    bypass: &[Ipv4Addr],
    routes: Option<&Routes>,
) -> Vec<(&'static str, String)> {
    let list = |ips: &[Ipv4Addr], sep: &str| ips.iter().map(Ipv4Addr::to_string).collect::<Vec<_>>().join(sep);
    let mut env = vec![
        ("DNS", list(dns, ",")),
        ("TUN_DNS", list(dns, " ")),
        ("TUN_DEV", tun.to_string()),
        ("TUN_ADDR", address.0.to_string()),
        ("TUN_PREFIX", address.1.to_string()),
        ("VPS_IP", vps_ip.to_string()),
        ("BYPASS", list(bypass, " ")),
        ("ROUTES", if routes.is_some() { "auto" } else { "off" }.to_string()),
    ];
    if let Some(up) = routes.map(|r| &r.uplink) {
        env.push(("UPLINK_DEV", up.dev.clone()));
        env.push(("UPLINK_GW", up.gateway.map(|g| g.to_string()).unwrap_or_default()));
        env.push(("UPLINK_IFINDEX", up.ifindex.to_string()));
    }
    env
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

/// Скрипт (интерпретатор — `HookLauncher` платформы) с переменными `env` и `HP_EVENT=<event>`; нет файла — ничего не делает.
async fn run_hook(script: &std::path::Path, event: &str, env: &[(&'static str, String)]) {
    if !script.is_file() {
        log::debug!("хук {}: файла нет", script.display());
        return;
    }
    log::info!("хук: {} {} (HP_EVENT={event})", NativeHooks::INTERPRETER, script.display());
    let mut command = NativeHooks::command(script);
    command.env("HP_EVENT", event).envs(env.iter().map(|(k, v)| (*k, v))).kill_on_drop(true);
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
