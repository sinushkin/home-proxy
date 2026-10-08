use std::io;

use tokio::signal::unix::{signal, Signal, SignalKind};

/// Ждёт просьбу остановиться: procd и systemd шлют SIGTERM, из консоли — Ctrl+C (SIGINT).
pub struct Shutdown {
    term: Signal,
    int: Signal,
}

impl Shutdown {
    /// Ставит обработчики; вызывать внутри tokio-рантайма.
    pub fn new() -> io::Result<Self> {
        Ok(Self { term: signal(SignalKind::terminate())?, int: signal(SignalKind::interrupt())? })
    }

    /// Завершается, когда пришёл сигнал; возвращает его название для лога. Безопасно в `select!`.
    pub async fn wait(&mut self) -> &'static str {
        tokio::select! {
            _ = self.term.recv() => "SIGTERM",
            _ = self.int.recv() => "SIGINT",
        }
    }
}
