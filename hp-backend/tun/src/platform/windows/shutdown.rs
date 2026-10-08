use std::io;

use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown, CtrlBreak, CtrlC, CtrlClose, CtrlShutdown};

/// Ждёт просьбу остановиться: Ctrl+C, Ctrl+Break, закрытие консоли, выключение системы.
/// (Службу Windows с её сообщением остановки добавим отдельно.)
pub struct Shutdown {
    c: CtrlC,
    brk: CtrlBreak,
    close: CtrlClose,
    shutdown: CtrlShutdown,
}

impl Shutdown {
    /// Ставит обработчики; вызывать внутри tokio-рантайма.
    pub fn new() -> io::Result<Self> {
        Ok(Self { c: ctrl_c()?, brk: ctrl_break()?, close: ctrl_close()?, shutdown: ctrl_shutdown()? })
    }

    /// Завершается, когда пришёл сигнал; возвращает его название для лога. Безопасно в `select!`.
    pub async fn wait(&mut self) -> &'static str {
        tokio::select! {
            _ = self.c.recv() => "Ctrl+C",
            _ = self.brk.recv() => "Ctrl+Break",
            _ = self.close.recv() => "закрытие консоли",
            _ = self.shutdown.recv() => "выключение системы",
        }
    }
}
