use std::path::{Path, PathBuf};

use crate::platform::HookLauncher;

/// `powershell -File <файл>`: хуки на Windows — `.ps1`, политика исполнения обходится для
/// конкретного запуска.
pub struct PsHooks;

impl HookLauncher for PsHooks {
    const INTERPRETER: &'static str = "powershell";

    fn command(script: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]).arg(script);
        command
    }

    fn default_scripts() -> (PathBuf, PathBuf) {
        let dir = PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into())).join("vps-client");
        (dir.join("on-tun-up.ps1"), dir.join("on-tun-down.ps1"))
    }
}
