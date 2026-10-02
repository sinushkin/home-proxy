//! Оценка смещения часов клиент → сервер (hp-stats, PLAN-ML.md §2.2): клиент гоняет несколько
//! `TimeProbe`/`TimeEcho` в начале сессии (и редко потом, против дрейфа), берёт пробу с
//! минимальным RTT, и дальше переводит свои метки времени в шкалу сервера для `PidReport`.
//!
//! Чистые функции здесь, сетевой цикл (отправка `TimeProbe`, ожидание `TimeEcho` по дыре) —
//! в `connection` (там есть доступ к `LinkSender` и событиям дыры); эта оценка от него не
//! зависит и проверяется без сети.

/// Из тройки `t0` (часы клиента на отправку пробы), `t1` (часы сервера в `TimeEcho`), `t2`
/// (часы клиента на приём эха) — RTT и смещение «сервер − клиент», в предположении
/// симметричного пути (см. оговорку в PLAN-ML.md: асимметрия даёт постоянный сдвиг в
/// *абсолютном* `flight_ms`, но не портит *относительные* различия между пакетами).
pub fn offset_from_triple(t0: u64, t1: u64, t2: u64) -> (i64, u32) {
    let rtt = t2.saturating_sub(t0);
    let mid = t0 as i64 + (t2 as i64 - t0 as i64) / 2;
    let offset = t1 as i64 - mid;
    (offset, u32::try_from(rtt).unwrap_or(u32::MAX))
}

/// Текущая оценка смещения часов, живёт на клиенте. Обновляется целиком за раунд (`set`) —
/// вызывающий сам выбирает лучшую (минимальный RTT) пробу среди нескольких в раунде через
/// `offset_from_triple` и передаёт сюда только победителя.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServerTime {
    offset_ms: i64,
    best_rtt_ms: u32,
    have: bool,
}

impl ServerTime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Запоминает результат раунда синхронизации.
    pub fn set(&mut self, offset_ms: i64, rtt_ms: u32) {
        self.offset_ms = offset_ms;
        self.best_rtt_ms = rtt_ms;
        self.have = true;
    }

    pub fn offset_ms(&self) -> Option<i64> {
        self.have.then_some(self.offset_ms)
    }

    pub fn best_rtt_ms(&self) -> Option<u32> {
        self.have.then_some(self.best_rtt_ms)
    }

    /// Переводит наши часы (unix-мс) в оценку часов сервера; без синхронизации — тождество
    /// (лучше заведомо смещённая на ноль оценка, чем паника или `Option` на каждый пакет).
    pub fn to_server_ms(&self, client_unix_ms: u64) -> u64 {
        if !self.have {
            return client_unix_ms;
        }
        (client_unix_ms as i64 + self.offset_ms).max(0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_path_gives_zero_offset() {
        // t0=1000, сервер отвечает через 20мс (t1=1020), клиент получает ещё через 20мс (t2=1040).
        let (offset, rtt) = offset_from_triple(1000, 1020, 1040);
        assert_eq!(offset, 0);
        assert_eq!(rtt, 40);
    }

    #[test]
    fn server_clock_ahead_gives_positive_offset() {
        // Часы сервера на 500мс впереди, путь симметричный (RTT 40, значит t1 должен быть
        // серединой пути + 500).
        let (offset, rtt) = offset_from_triple(1000, 1520, 1040);
        assert_eq!(offset, 500);
        assert_eq!(rtt, 40);
    }

    #[test]
    fn to_server_ms_applies_offset() {
        let mut st = ServerTime::new();
        assert_eq!(st.to_server_ms(1000), 1000, "без синхронизации — тождество");
        st.set(500, 40);
        assert_eq!(st.to_server_ms(1000), 1500);
        assert_eq!(st.offset_ms(), Some(500));
        assert_eq!(st.best_rtt_ms(), Some(40));
    }

    #[test]
    fn negative_offset_does_not_underflow() {
        let mut st = ServerTime::new();
        st.set(-2000, 10);
        assert_eq!(st.to_server_ms(500), 0, "насыщение в ноль, а не паника/underflow");
    }
}
