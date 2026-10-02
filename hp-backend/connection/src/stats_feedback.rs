//! hp-stats (фича `stats`, см. PLAN-ML.md): клиентская сторона обратной связи — чистое
//! состояние для синхронизации часов (`TimeProbe`/`TimeEcho`) и накопления `PidReport` по
//! принятым номерам пакетов вниз. Сетевые отправки (какой дырой, когда) — у `control_loop`,
//! здесь только бухгалтерия, поэтому проверяется без сети.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use hp_stats::offset_from_triple;

use crate::proto::{pid_report, PidReport, TimeEcho};

pub(crate) fn unix_ms_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Сколько проб гоняем в начальном раунде синхронизации (берём с минимальным RTT) — PLAN-ML.md §2.2.
pub const INITIAL_PROBES: u32 = 8;

/// Синхронизация часов: учёт отправленных проб, разбор ответов и выбор лучшей (по RTT) из
/// начального раунда; дальше — применяет каждую новую пробу сразу (редкие, против дрейфа).
/// Когда слать пробы — решает вызывающий (`multilink::time_probe_loop`), здесь только бухгалтерия.
pub(crate) struct TimeSync {
    server_time: hp_stats::ServerTime,
    next_seq: u32,
    outstanding: HashMap<u32, u64>,
    /// Пробы текущего начального раунда, пока их не набралось `INITIAL_PROBES`.
    round: Vec<(i64, u32)>,
    synced_once: bool,
}

impl TimeSync {
    pub fn new() -> Self {
        Self {
            server_time: hp_stats::ServerTime::new(),
            next_seq: 0,
            outstanding: HashMap::new(),
            round: Vec::with_capacity(INITIAL_PROBES as usize),
            synced_once: false,
        }
    }

    /// Прошёл ли начальный раунд (значит, дальше пробы редкие — только против дрейфа).
    pub fn synced(&self) -> bool {
        self.synced_once
    }

    /// Готовит пробу к отправке: `(client_send_ms, seq)` — передать в `LinkSender::send_time_probe`.
    pub fn send_probe(&mut self) -> (u64, u32) {
        let t0 = unix_ms_now();
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.outstanding.insert(seq, t0);
        (t0, seq)
    }

    /// Разбирает `TimeEcho`. В начальном раунде копит пробу и применяет лучшую (минимальный RTT)
    /// по достижении `INITIAL_PROBES`; после — применяет каждую новую пробу сразу (отслеживание
    /// дрейфа, без накопления). Неизвестный/запоздалый `seq` молча игнорируется.
    pub fn on_echo(&mut self, echo: &TimeEcho) {
        self.on_echo_at(echo, unix_ms_now());
    }

    /// То же, что `on_echo`, но с явным `t2` (момент приёма) — тестовый шов: `on_echo` берёт его
    /// из настоящих часов, из-за чего в тесте без реальной задержки сети все RTT были бы ~0.
    fn on_echo_at(&mut self, echo: &TimeEcho, t2: u64) {
        let Some(t0) = self.outstanding.remove(&echo.seq) else { return };
        let (offset, rtt) = offset_from_triple(t0, echo.server_ms, t2);
        if self.synced_once {
            self.server_time.set(offset, rtt);
            return;
        }
        self.round.push((offset, rtt));
        if self.round.len() as u32 >= INITIAL_PROBES {
            if let Some(&best) = self.round.iter().min_by_key(|(_, rtt)| *rtt) {
                self.server_time.set(best.0, best.1);
            }
            self.round.clear();
            self.synced_once = true;
        }
    }

    pub fn server_time(&self) -> &hp_stats::ServerTime {
        &self.server_time
    }
}

struct PendingEntry {
    pid: u32,
    recv_server_ms: u64,
    out_of_order: bool,
}

/// Копит принятые `pid` (с номером — те, что отправитель пронумеровал) до отправки `PidReport`.
/// `ack_through` в собранном отчёте — минимальный `pid` партии (база для дельт, компактность на
/// проводе), а не строгий cumulative-ack с учётом пропусков: порядок доставки `PidReport` для
/// сборщика не важен, он сопоставляет по абсолютному `pid` в каждой записи.
pub(crate) struct PidFeedbackBuilder {
    entries: Vec<PendingEntry>,
    max_pid_seen: Option<u32>,
}

impl PidFeedbackBuilder {
    pub fn new() -> Self {
        Self { entries: Vec::new(), max_pid_seen: None }
    }

    /// `recv_server_ms` — время приёма, уже переведённое в часы сервера (`ServerTime::to_server_ms`).
    pub fn observe(&mut self, pid: u32, recv_server_ms: u64) {
        // Сравнение не учитывает перенос через 2^32 (при текущих объёмах — миллиарды пакетов —
        // не актуально; как и окно дедупликации в PLAN-dynamic-holes-relay.md, учёт переноса —
        // доработка на будущее).
        let out_of_order = self.max_pid_seen.is_some_and(|max| pid <= max);
        self.max_pid_seen = Some(self.max_pid_seen.map_or(pid, |max| max.max(pid)));
        self.entries.push(PendingEntry { pid, recv_server_ms, out_of_order });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Забирает накопленное и строит `PidReport`; `None`, если копить было нечего.
    pub fn take(&mut self) -> Option<PidReport> {
        if self.entries.is_empty() {
            return None;
        }
        let entries = std::mem::take(&mut self.entries);
        let ack_through = entries.iter().map(|e| e.pid).min().expect("entries непусты");
        let recv_base_server_ms = entries.iter().map(|e| e.recv_server_ms).min().expect("entries непусты");
        let entries = entries
            .into_iter()
            .map(|e| pid_report::Entry {
                pid_delta: e.pid - ack_through,
                recv_delta_ms: u32::try_from(e.recv_server_ms.saturating_sub(recv_base_server_ms)).unwrap_or(u32::MAX),
                // Сколько пакет ждал в буфере порядка — не отслеживаем по пакету в этой версии
                // (нужен хук внутри `reorder::Resequencer`); 0 читается как «не ждал».
                // См. PLAN-ML.md, журнал.
                reorder_wait_ms: 0,
                out_of_order: e.out_of_order,
            })
            .collect();
        Some(PidReport { recv_base_server_ms, ack_through, entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_sync_applies_best_of_the_initial_round() {
        let mut sync = TimeSync::new();
        assert!(!sync.synced());
        // RTT проб убывает, затем снова растёт: лучшая — с минимальным RTT (четвёртая, rtt=4).
        // `t0` — настоящий (из `send_probe`, часы теста), `t2` сдвигаем на желаемый RTT: именно
        // так `on_echo_at` его и считает (`t0` берётся из `outstanding` по `seq`, не из `echo`).
        let rtts = [40u64, 30, 20, 4, 25, 15, 35, 10];
        for (i, &rtt) in rtts.iter().enumerate() {
            let (t0, seq) = sync.send_probe();
            let echo = TimeEcho { client_send_ms: t0, server_ms: t0 + rtt / 2, seq };
            sync.on_echo_at(&echo, t0 + rtt);
            assert_eq!(sync.synced(), i == rtts.len() - 1, "раунд закрывается только на последней пробе");
        }
        assert_eq!(sync.server_time().best_rtt_ms(), Some(4));
    }

    #[test]
    fn after_initial_round_each_new_probe_is_applied_immediately() {
        let mut sync = TimeSync::new();
        for _ in 0..INITIAL_PROBES {
            let (t0, seq) = sync.send_probe();
            sync.on_echo_at(&TimeEcho { client_send_ms: t0, server_ms: t0 + 5, seq }, t0 + 10);
        }
        assert!(sync.synced());
        let before = sync.server_time().best_rtt_ms();
        let (t0, seq) = sync.send_probe();
        sync.on_echo_at(&TimeEcho { client_send_ms: t0, server_ms: t0 + 50, seq }, t0 + 99);
        assert_ne!(sync.server_time().best_rtt_ms(), before, "новая проба после раунда применяется сразу, без накопления");
        assert_eq!(sync.server_time().best_rtt_ms(), Some(99));
    }

    #[test]
    fn unknown_or_late_echo_is_ignored() {
        let mut sync = TimeSync::new();
        let echo = TimeEcho { client_send_ms: 0, server_ms: 0, seq: 999 };
        sync.on_echo(&echo); // не паникует и не засчитывает несуществующую пробу
        assert!(!sync.synced());
    }

    #[test]
    fn pid_feedback_builds_deltas_relative_to_batch_minimum() {
        let mut b = PidFeedbackBuilder::new();
        assert_eq!(b.len(), 0);
        b.observe(100, 1_000);
        b.observe(101, 1_005);
        b.observe(102, 1_010);
        assert_eq!(b.len(), 3);
        let report = b.take().unwrap();
        assert_eq!(report.ack_through, 100);
        assert_eq!(report.recv_base_server_ms, 1_000);
        assert_eq!(report.entries.len(), 3);
        assert_eq!(report.entries[0], pid_report::Entry { pid_delta: 0, recv_delta_ms: 0, reorder_wait_ms: 0, out_of_order: false });
        assert_eq!(report.entries[2], pid_report::Entry { pid_delta: 2, recv_delta_ms: 10, reorder_wait_ms: 0, out_of_order: false });
        assert_eq!(b.len(), 0, "take() опустошает накопленное");
    }

    #[test]
    fn pid_feedback_flags_out_of_order_arrivals() {
        let mut b = PidFeedbackBuilder::new();
        b.observe(5, 1_000);
        b.observe(7, 1_010);
        b.observe(6, 1_020); // пришёл после 7, хотя номер меньше
        let report = b.take().unwrap();
        assert_eq!(report.entries.iter().map(|e| e.out_of_order).collect::<Vec<_>>(), vec![false, false, true]);
    }
}
