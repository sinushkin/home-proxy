//! Логгер в syslog для OpenWrt: там `logd` принимает syslog(3) на `/dev/log`, а
//! читается всё это через `logread`. Так же пишет `ulog` из libubox, когда
//! stdout не терминал. Фильтры по модулям берём у `env_logger` (`RUST_LOG`).

use std::ffi::CString;
use std::os::raw::{c_char, c_int};

use log::{Level, Log, Metadata, Record};

pub struct SyslogLogger {
    filter: env_logger::Logger,
}

/// Ставит syslog-логгер вместо вывода в stderr. `filter` задаёт, что пропускать.
pub fn init(filter: env_logger::Logger) -> Result<(), log::SetLoggerError> {
    // SAFETY: строка-литерал живёт всю программу, openlog хранит этот указатель.
    unsafe { libc::openlog(c"peer".as_ptr(), libc::LOG_PID, libc::LOG_DAEMON) };
    log::set_max_level(filter.filter());
    log::set_boxed_logger(Box::new(SyslogLogger { filter }))
}

fn priority(level: Level) -> c_int {
    match level {
        Level::Error => libc::LOG_ERR,
        Level::Warn => libc::LOG_WARNING,
        Level::Info => libc::LOG_INFO,
        Level::Debug | Level::Trace => libc::LOG_DEBUG,
    }
}

/// syslog принимает C-строку: NUL внутри сообщения нельзя.
fn c_message(target: &str, text: &str) -> CString {
    let line = format!("{target}: {text}").replace('\0', " ");
    CString::new(line).expect("NUL заменены выше")
}

impl Log for SyslogLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.filter.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = c_message(record.target(), &record.args().to_string());
        // SAFETY: формат "%s" и C-строка без NUL внутри, указатель валиден на время вызова.
        unsafe { libc::syslog(priority(record.level()), c"%s".as_ptr() as *const c_char, message.as_ptr()) };
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_map_to_syslog_priorities() {
        assert_eq!(priority(Level::Error), libc::LOG_ERR);
        assert_eq!(priority(Level::Warn), libc::LOG_WARNING);
        assert_eq!(priority(Level::Info), libc::LOG_INFO);
        assert_eq!(priority(Level::Debug), libc::LOG_DEBUG);
        assert_eq!(priority(Level::Trace), libc::LOG_DEBUG);
    }

    #[test]
    fn message_gets_target_prefix_and_no_nul() {
        let message = c_message("peer", "a\0b");
        assert_eq!(message.to_str().unwrap(), "peer: a b");
    }
}
