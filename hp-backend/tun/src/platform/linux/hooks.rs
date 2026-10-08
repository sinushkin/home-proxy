use std::path::{Path, PathBuf};

use crate::platform::HookLauncher;

/// `/bin/sh <файл>`: на OpenWrt bash нет, права на исполнение не нужны.
pub struct ShHooks;

impl HookLauncher for ShHooks {
    const INTERPRETER: &'static str = "sh";

    fn command(script: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.arg(script);
        command
    }

    fn default_scripts() -> (PathBuf, PathBuf) {
        ("/etc/vps-client/on-tun-up.sh".into(), "/etc/vps-client/on-tun-down.sh".into())
    }
}
