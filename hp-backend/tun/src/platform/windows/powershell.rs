//! Запуск PowerShell для маршрутов. Windows может быть русской: текст `netsh`/`route print`
//! зависит от языка, поэтому разбираем только строки с нашей меткой, которые печатает скрипт.

use anyhow::{Context, Result};

/// Выполняет `script` в `powershell.exe` (Windows PowerShell 5.1 есть везде) и возвращает stdout.
/// Любая необработанная ошибка в скрипте (`$ErrorActionPreference = 'Stop'`) — ошибка с stderr. Код
/// возврата 0 ставим сами: иначе PowerShell вернёт 1, если *последняя* команда скрипта
/// отметила ошибку (`$?`), даже заглушённую `-ErrorAction SilentlyContinue`.
pub async fn run(script: &str) -> Result<String> {
    // Вывод — в UTF-8 без BOM (по умолчанию консольная OEM-кодировка: на русской Windows 866).
    let script = format!(
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false\n$ErrorActionPreference = 'Stop'\n{script}\nexit 0"
    );
    let out = tokio::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .output()
        .await
        .context("не удалось запустить powershell.exe")?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    anyhow::ensure!(out.status.success(), "powershell: {} {}", text(&out.stderr), text(&out.stdout));
    Ok(text(&out.stdout))
}

/// Строка в одинарных кавычках для PowerShell (`'` удваивается).
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn quotes_are_doubled() {
        assert_eq!(super::quote("hp0"), "'hp0'");
        assert_eq!(super::quote("it's"), "'it''s'");
    }
}
