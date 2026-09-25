//! Общая инициализация логов для бинарников (`peer`, `router`). `connection`
//! зависит только от фасада `log`, а вывод настраивается здесь.
//!
//! Уровень — `RUST_LOG` (по умолчанию `info`), формат — как у `env_logger`.
//! `LOG_TARGET=syslog` пишет в syslog (на OpenWrt читается `logread`), иначе в
//! stderr.

mod syslog;

/// Ставит глобальный логгер. Вызывать один раз в начале `main`.
pub fn init() -> Result<(), log::SetLoggerError> {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if std::env::var("LOG_TARGET").as_deref() == Ok("syslog") {
        syslog::init(builder.build())
    } else {
        builder.try_init()
    }
}
