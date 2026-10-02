//! Окно дедупликации по номеру пакета (`Lite.pid`, см. `PLAN-ML.md` §2.1 и
//! `PLAN-dynamic-holes-relay.md`, раздел 5): когда пакет может дойти дважды (второй путь — релей,
//! `PLAN-dynamic-holes-relay.md` M7), получатель отбрасывает уже виденный.
//!
//! Тот же приём, что и окно против повтора подписи (`auth::ReplayWindow`), только шире: там
//! счётчик — на одно направление одной дыры и растёт строго по единице на отправленный пакет, а
//! `pid` — общий счётчик на все дыры сразу, и пакеты с разных дыр естественно приходят сильно
//! вперемешку (на живом прогоне — около 75% «не по порядку» относительно глобального `pid`, см.
//! `PLAN-ML.md`). 64 бит там, где разброс в тысячи позиций, — мало, поэтому здесь окно шире
//! (4096 бит = 512 байт) и хранится массивом `u32`, а не одним `u64`-словом.
//!
//! Без атомиков (ни 64-битных, ни каких-либо ещё): `DedupWindow` живёт за `&mut self`, один
//! потребитель за раз (`control_loop` в `multilink.rs`), как и `ReplayWindow`.

/// Ширина окна в битах: насколько старый (по сравнению с максимальным увиденным) `pid` ещё
/// помним. Пакет древнее — считается новым без проверки (т.е. тоже принимается; окно — защита от
/// дублей в разумных пределах реордеринга, не абсолютная гарантия на любую задержку).
const WINDOW_BITS: u32 = 4096;
const WORDS: usize = (WINDOW_BITS / 32) as usize;

/// Окно дедупликации: помнит, какие `pid` уже видели, в пределах `WINDOW_BITS` от максимального.
pub struct DedupWindow {
    /// Наибольший принятый `pid` (действителен только после первого пакета).
    top: u32,
    have_top: bool,
    /// Бит `i` (считая `i = back`, `word = i / 32`, `bit = i % 32`) — принят `pid = top - i`.
    seen: [u32; WORDS],
}

impl DedupWindow {
    pub fn new() -> Self {
        Self { top: 0, have_top: false, seen: [0; WORDS] }
    }

    fn bit(&self, back: u32) -> bool {
        let (word, bit) = (back / 32, back % 32);
        self.seen[word as usize] & (1 << bit) != 0
    }

    fn set_bit(&mut self, back: u32) {
        let (word, bit) = (back / 32, back % 32);
        self.seen[word as usize] |= 1 << bit;
    }

    /// Сдвигает окно вперёд на `shift` позиций (новый максимум старше прежнего на `shift`):
    /// бит `back` переезжает в `back + shift`, освободившиеся младшие биты — в ноль. При
    /// `shift >= WINDOW_BITS` старое окно целиком устарело — просто обнуляем.
    fn shift_by(&mut self, shift: u32) {
        if shift >= WINDOW_BITS {
            self.seen = [0; WORDS];
            return;
        }
        let (word_shift, bit_shift) = (shift as usize / 32, shift % 32);
        if bit_shift == 0 {
            for i in (word_shift..WORDS).rev() {
                self.seen[i] = self.seen[i - word_shift];
            }
        } else {
            for i in (0..WORDS).rev() {
                let hi = if i >= word_shift { self.seen[i - word_shift] << bit_shift } else { 0 };
                let lo = if i > word_shift { self.seen[i - word_shift - 1] >> (32 - bit_shift) } else { 0 };
                self.seen[i] = hi | lo;
            }
        }
        for word in self.seen.iter_mut().take(word_shift.min(WORDS)) {
            *word = 0;
        }
    }

    /// Удобство для вызывающего (`control_loop`): пакет без номера (`None`) дедупликации не
    /// подлежит — пропускаем как раньше, до появления `pid`; с номером — решает `seen`.
    pub fn admit(&mut self, pid: Option<u32>) -> bool {
        pid.is_none_or(|p| self.seen(p))
    }

    /// `true` — пакет новый (не видели раньше и либо внутри окна, либо древнее его целиком),
    /// окно его запоминает; `false` — уже видели (дубликат, отбросить). Сравнение — по модулю
    /// `2³²` (разница через знаковое `i32`, как сравнение TCP sequence numbers): пока расхождение
    /// между пакетами меньше `2³¹`, перенос через `2³²` не ломает порядок.
    pub fn seen(&mut self, pid: u32) -> bool {
        if !self.have_top {
            self.have_top = true;
            self.top = pid;
            self.set_bit(0);
            return true;
        }
        let delta = pid.wrapping_sub(self.top) as i32;
        if delta > 0 {
            self.shift_by(delta as u32);
            self.top = pid;
            self.set_bit(0);
            true
        } else {
            let back = delta.unsigned_abs();
            if back >= WINDOW_BITS {
                // Древнее всего окна: не можем доказать, что это дубликат, — пропускаем, как и
                // до введения pid (поведение по умолчанию для пакетов без номера).
                true
            } else if self.bit(back) {
                false
            } else {
                self.set_bit(back);
                true
            }
        }
    }
}

impl Default for DedupWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_without_a_pid_are_never_deduplicated() {
        let mut w = DedupWindow::new();
        assert!(w.admit(None));
        assert!(w.admit(None), "без номера — всегда «новый», окно его не отслеживает");
        assert!(w.admit(Some(1)));
        assert!(!w.admit(Some(1)), "а с номером — дедуплицируется как обычно");
    }

    #[test]
    fn first_packet_is_always_new() {
        let mut w = DedupWindow::new();
        assert!(w.seen(1000));
    }

    #[test]
    fn exact_duplicate_is_rejected() {
        let mut w = DedupWindow::new();
        assert!(w.seen(5));
        assert!(!w.seen(5), "тот же pid второй раз — дубликат");
    }

    #[test]
    fn increasing_order_all_pass_and_are_not_duplicates_of_each_other() {
        let mut w = DedupWindow::new();
        for pid in 0..1000u32 {
            assert!(w.seen(pid), "pid {pid} должен быть новым");
        }
    }

    #[test]
    fn out_of_order_within_window_is_accepted_once_each() {
        // Имитация реального разброса между дырами: пакеты 0..10 приходят вперемешку.
        let mut w = DedupWindow::new();
        let order = [5u32, 2, 8, 0, 9, 1, 7, 3, 6, 4];
        for &pid in &order {
            assert!(w.seen(pid), "pid {pid} в своём первом приходе — новый");
        }
        for &pid in &order {
            assert!(!w.seen(pid), "pid {pid} во втором приходе — дубликат");
        }
    }

    #[test]
    fn packet_older_than_the_window_is_treated_as_new_not_rejected() {
        let mut w = DedupWindow::new();
        assert!(w.seen(10_000));
        // Пакет на WINDOW_BITS+100 древнее максимума — вне окна, не можем судить, пропускаем.
        assert!(w.seen(10_000 - WINDOW_BITS - 100));
    }

    #[test]
    fn window_slides_forward_and_forgets_the_oldest_bits() {
        let mut w = DedupWindow::new();
        assert!(w.seen(0));
        assert!(w.seen(WINDOW_BITS)); // сдвигает окно ровно на WINDOW_BITS: pid=0 выходит за край
        assert!(w.seen(0), "pid=0 теперь вне окна — пропускаем, а не считаем дублем");
    }

    #[test]
    fn duplicate_still_detected_after_a_partial_slide() {
        let mut w = DedupWindow::new();
        assert!(w.seen(100));
        assert!(w.seen(105)); // сдвиг на 5; pid=100 всё ещё в окне (back=5)
        assert!(!w.seen(100), "100 всё ещё в пределах окна — дубликат");
        assert!(!w.seen(105), "105 — текущий максимум, тоже дубликат");
    }

    #[test]
    fn wraps_around_u32_without_breaking_comparison() {
        let mut w = DedupWindow::new();
        assert!(w.seen(u32::MAX - 2));
        assert!(w.seen(u32::MAX));
        assert!(w.seen(0), "перенос через 2^32: 0 идёт сразу за u32::MAX, это новый pid");
        assert!(!w.seen(0), "0 теперь максимум — повтор отбрасывается");
        assert!(!w.seen(u32::MAX), "u32::MAX всё ещё в окне позади нового максимума");
    }

    #[test]
    fn huge_jump_resets_the_window_instead_of_overflowing() {
        let mut w = DedupWindow::new();
        assert!(w.seen(0));
        assert!(w.seen(0u32.wrapping_add(WINDOW_BITS * 10)), "огромный скачок вперёд — тоже новый pid");
        assert!(w.seen(0u32.wrapping_add(WINDOW_BITS * 10 + 1)));
    }
}
