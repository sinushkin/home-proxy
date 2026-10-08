//! Всё, что зависит от ОС: создание TUN, системные маршруты, запуск хуков, сигнал остановки.
//!
//! Общий код (`bridge`, `hub`, политика маршрутов в `routes`, `hook_env`) знает только трейты
//! отсюда; реализация выбирается при компиляции по `cfg` и доступна под именами `Native*`.
//! Динамической диспетчеризации нет: на платформу ровно одна реализация.
//!
//! | Трейт / тип        | Linux, OpenWrt, Android                 | Windows                      |
//! |--------------------|-----------------------------------------|----------------------------|
//! | [`TunDevice`]      | `/dev/net/tun`, `ioctl` (`linux::Tun`)  | Wintun                     |
//! | [`RouteBackend`]   | команда `ip`, `/sys/class/net`          | `route.exe` / IP Helper    |
//! | [`HookLauncher`]   | `/bin/sh <скрипт>`                      | `powershell -File <скрипт>`|
//! | [`NativeShutdown`] | SIGTERM, SIGINT                         | Ctrl+C, Ctrl+Close, служба |
//!
//! Чтобы добавить платформу: модуль рядом с `linux` и `windows`, четыре реализации и блок `cfg`
//! ниже. Остальные платформы (macOS и т. д.) собирают `hp-tun` без `platform` и `routes` — мост,
//! хаб и разбор пакетов от них не зависят.

use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::device::PacketDevice;
use crate::routes::Uplink;
use crate::TunConfig;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use linux::{IpRoutes as NativeRoutes, ShHooks as NativeHooks, Shutdown as NativeShutdown, Tun as NativeTun};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{PsHooks as NativeHooks, PsRoutes as NativeRoutes, Shutdown as NativeShutdown, Tun as NativeTun};

/// Интерфейс TUN: создаётся по конфигурации, читает и пишет по одному IP-пакету (`PacketDevice`).
pub trait TunDevice: PacketDevice + Sized {
    /// Создаёт интерфейс, задаёт адрес, MTU и поднимает его (нужны права администратора).
    fn create(config: &TunConfig) -> io::Result<Self>;
    /// Имя интерфейса, как его видит система (`hp0`).
    fn name(&self) -> &str;
}

/// Операции над таблицей маршрутов ОС. Политика — что должно быть, когда ставить и когда
/// сдаваться — общая и лежит в `routes::Routes`; здесь только то, что делает система.
#[allow(async_fn_in_trait)]
pub trait RouteBackend: Default + Send + Sync + 'static {
    /// Аплинк сейчас: маршрут по умолчанию с наименьшей метрикой, кроме самого туннеля `tun`.
    /// `None` — такого нет (аплинк временно лёг или default заменён на туннель).
    async fn current_uplink(&self, tun: &str) -> Result<Option<Uplink>>;

    /// Аплинк по пути до `probe`, когда default нет: через какой шлюз и интерфейс ОС отправила
    /// бы пакет к этому адресу. Маршрут через сам `tun` не считается.
    async fn probe_uplink(&self, tun: &str, probe: Ipv4Addr) -> Result<Option<Uplink>>;

    /// Возвращает маршрут по умолчанию через `up` (он пропал вместе с туннелем прошлого запуска).
    async fn restore_default(&self, up: &Uplink) -> Result<()>;

    /// Ставит недостающее: адреса `bypass` через `up`, а при `tunnel_up` ещё весь остальной
    /// трафик в `tun` (две половины адресного пространства, не заменяя default аплинка).
    /// Лишний default в `tun`, оставшийся от ручной настройки, убирает.
    async fn sync(&self, tun: &str, bypass: &[Ipv4Addr], up: &Uplink, tunnel_up: bool) -> Result<()>;

    /// Убирает маршруты в `tun`. Ошибки не важны: маршрутов может уже не быть.
    async fn remove_tunnel(&self, tun: &str);
}

/// Как запускать скрипт хука. Окружение (`HP_EVENT` и переменные туннеля), каталог запуска,
/// ограничение по времени и разбор вывода — общие, в `routes`.
pub trait HookLauncher {
    /// Имя интерпретатора для лога (`sh`).
    const INTERPRETER: &'static str;
    /// Команда запуска файла `script`, без окружения.
    fn command(script: &Path) -> tokio::process::Command;
    /// Где лежат скрипты подъёма и остановки туннеля, если путь не задан (`on-tun-up`, `on-tun-down`).
    fn default_scripts() -> (PathBuf, PathBuf);
}
