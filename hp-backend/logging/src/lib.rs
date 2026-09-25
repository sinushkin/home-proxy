//! Общая инициализация логов для бинарников (`peer`, `router`, `server`).
//! `connection` зависит только от фасада `log`, а вывод настраивается здесь.
//!
//! Уровень — `RUST_LOG` (по умолчанию `info`), формат — как у `env_logger`.
//! `LOG_FILE=путь` дописывает логи в файл (для службы Windows, у которой нет
//! консоли), `LOG_TARGET=syslog` пишет в syslog (Unix; на OpenWrt читается
//! `logread`), иначе в stderr.

#[cfg(unix)]
mod syslog;

use std::fs::OpenOptions;

/// Ставит глобальный логгер по переменным окружения. Вызывать один раз в начале `main`.
pub fn init() -> Result<(), log::SetLoggerError> {
    init_with(|name| std::env::var(name).ok())
}

/// То же, но настройки берутся через `get` (например, из файла настроек службы).
pub fn init_with(get: impl Fn(&str) -> Option<String>) -> Result<(), log::SetLoggerError> {
    let mut builder = env_logger::Builder::new();
    builder.parse_filters(&get("RUST_LOG").unwrap_or_else(|| "info".to_string()));

    #[cfg(unix)]
    if get("LOG_TARGET").as_deref() == Some("syslog") {
        return syslog::init(builder.build());
    }

    if let Some(path) = get("LOG_FILE") {
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                builder.target(env_logger::Target::Pipe(Box::new(file)));
                builder.write_style(env_logger::WriteStyle::Never);
            }
            Err(e) => eprintln!("не удалось открыть LOG_FILE {path}: {e}; пишем в stderr"),
        }
    }
    builder.try_init()
}
