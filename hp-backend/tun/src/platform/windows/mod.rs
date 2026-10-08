//! Windows 10+: TUN — Wintun (`wintun.dll` рядом с `.exe`), маршруты и хуки — PowerShell.
//! Нужны права администратора.

mod hooks;
mod powershell;
mod routes;
mod shutdown;
mod tun;

pub use hooks::PsHooks;
pub use routes::PsRoutes;
pub use shutdown::Shutdown;
pub use tun::Tun;
