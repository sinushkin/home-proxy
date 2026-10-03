//! Ротация дыр (M5b, `PLAN-dynamic-holes-relay.md`): раз в минуту худшая активная дыра помечается
//! на слив, вместо неё открывается новая. Слитая дыра освобождается, когда на неё 10 с не приходит
//! ни одного входящего пакета (тишина отсчитывается от момента слива). Чистая логика: время и
//! снимок дыр передаёт вызывающий, сети и часов внутри нет — как в `holes.rs`. Подключение к
//! `MultiLink` — отдельный шаг.

use std::time::{Duration, Instant};

use crate::holes::{HoleId, HoleState};

/// Как часто искать худшую дыру.
pub const ROTATION_INTERVAL: Duration = Duration::from_secs(60);

/// Сколько тишины (входящих пакетов нет) нужно, чтобы освободить слитую дыру.
pub const QUIET_TO_RELEASE: Duration = Duration::from_secs(10);

/// Страховка: слитая дыра не живёт дольше этого, даже если тишины нет (пир шлёт, но мы не
/// отвечаем, или ответ потерян). Не из условия задачи — чтобы не копить мёртвые дыры.
pub const DRAIN_MAX: Duration = Duration::from_secs(60);

/// Снимок дыры для ротации.
#[derive(Clone, Copy, Debug)]
pub struct RotationHole {
    pub id: HoleId,
    pub state: HoleState,
    /// Доля потерь от нас к пиру; `None` — мало данных.
    pub loss_out: Option<f32>,
    /// Доля потерь от пира к нам; `None` — мало данных.
    pub loss_in: Option<f32>,
    /// Сколько прошло с последнего входящего пакета от пира по этой дыре.
    pub idle_in: Duration,
}

/// Что сделать по итогам шага.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RotationAction {
    /// Пометить дыру на слив: перестаём слать по ней, просим пира сделать то же (`Drain`).
    Drain(HoleId),
    /// Открыть замену (`id` назначит вызывающий).
    Open,
    /// Освободить слитую дыру: закрыть сокет и убрать из реестра.
    Release(HoleId),
}

/// Состояние ротации между шагами.
#[derive(Debug, Default)]
pub struct Rotation {
    /// Когда следующая ротация; `None` — отсчёт ещё не начат (первый шаг запускает таймер).
    next_rotation: Option<Instant>,
    /// Слитые дыры и момент, когда их слили.
    draining: Vec<(HoleId, Instant)>,
    /// Уже освобождённые дыры: пока вызывающий не убрал их из снимка, второй раз не освобождаем.
    released: Vec<HoleId>,
}

/// Ключ «худшести»: потери (худшая из двух сторон, `None` — 0) и тишина. При равных потерях
/// хуже та, что дольше не присылала нам пакетов.
fn worst_key(h: &RotationHole) -> (f32, Duration) {
    (h.loss_out.unwrap_or(0.0).max(h.loss_in.unwrap_or(0.0)), h.idle_in)
}

impl Rotation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Один шаг: вызывать часто (например, раз в секунду) с текущими `now` и снимком `holes`.
    /// Освобождения идут до ротации; одна ротация за шаг.
    pub fn step(&mut self, now: Instant, holes: &[RotationHole]) -> Vec<RotationAction> {
        let mut actions = Vec::new();

        // Слитые и освобождённые дыры, которых в снимке уже нет, забываем.
        self.draining.retain(|(id, _)| holes.iter().any(|h| h.id == *id && h.state == HoleState::Draining));
        self.released.retain(|id| holes.iter().any(|h| h.id == *id));

        // Освобождение: тишина от момента слива (но не старше DRAIN_MAX).
        for h in holes.iter().filter(|h| h.state == HoleState::Draining && !self.released.contains(&h.id)) {
            let started = match self.draining.iter().find(|(id, _)| *id == h.id) {
                Some((_, started)) => *started,
                None => {
                    // Слили не мы (или до перезапуска ротации) — отсчёт начинаем сейчас.
                    self.draining.push((h.id, now));
                    now
                }
            };
            let since_drain = now.saturating_duration_since(started);
            let quiet = h.idle_in.min(since_drain);
            if quiet >= QUIET_TO_RELEASE || since_drain >= DRAIN_MAX {
                actions.push(RotationAction::Release(h.id));
            }
        }
        for action in &actions {
            if let RotationAction::Release(id) = action {
                self.draining.retain(|(d, _)| d != id);
                self.released.push(*id);
            }
        }

        // Ротация: по расписанию, одна за шаг.
        let next = *self.next_rotation.get_or_insert(now + ROTATION_INTERVAL);
        if now >= next {
            self.next_rotation = Some(now + ROTATION_INTERVAL);
            let worst = holes
                .iter()
                .filter(|h| h.state == HoleState::Active)
                .max_by(|a, b| {
                    let (la, ia) = worst_key(a);
                    let (lb, ib) = worst_key(b);
                    la.total_cmp(&lb).then(ia.cmp(&ib))
                });
            if let Some(worst) = worst {
                actions.push(RotationAction::Drain(worst.id));
                actions.push(RotationAction::Open);
                self.draining.push((worst.id, now));
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hole(id: HoleId, state: HoleState, loss: Option<f32>, idle_s: u64) -> RotationHole {
        RotationHole { id, state, loss_out: loss, loss_in: None, idle_in: Duration::from_secs(idle_s) }
    }

    #[test]
    fn nothing_happens_before_the_first_minute() {
        let t0 = Instant::now();
        let holes = [hole(1, HoleState::Active, Some(0.5), 0)];
        let mut rot = Rotation::new();
        assert!(rot.step(t0, &holes).is_empty());
        assert!(rot.step(t0 + Duration::from_secs(59), &holes).is_empty());
    }

    #[test]
    fn at_a_minute_the_worst_active_hole_is_drained_and_replaced() {
        let t0 = Instant::now();
        let holes = [hole(1, HoleState::Active, Some(0.01), 0), hole(2, HoleState::Active, Some(0.2), 0), hole(3, HoleState::Active, Some(0.05), 0)];
        let mut rot = Rotation::new();
        rot.step(t0, &holes);
        assert_eq!(rot.step(t0 + ROTATION_INTERVAL, &holes), vec![RotationAction::Drain(2), RotationAction::Open]);
    }

    #[test]
    fn warming_and_draining_holes_are_never_chosen_for_rotation() {
        let t0 = Instant::now();
        let holes = [hole(1, HoleState::Warming, Some(0.9), 0), hole(2, HoleState::Active, Some(0.1), 0)];
        let mut rot = Rotation::new();
        rot.step(t0, &holes);
        assert_eq!(rot.step(t0 + ROTATION_INTERVAL, &holes), vec![RotationAction::Drain(2), RotationAction::Open]);
    }

    #[test]
    fn without_active_holes_there_is_no_rotation() {
        let t0 = Instant::now();
        let holes = [hole(1, HoleState::Warming, Some(0.9), 0)];
        let mut rot = Rotation::new();
        rot.step(t0, &holes);
        assert!(rot.step(t0 + ROTATION_INTERVAL, &holes).is_empty());
    }

    #[test]
    fn equal_loss_or_no_data_picks_the_one_silent_longest() {
        let t0 = Instant::now();
        let holes = [hole(1, HoleState::Active, None, 2), hole(2, HoleState::Active, None, 30), hole(3, HoleState::Active, None, 5)];
        let mut rot = Rotation::new();
        rot.step(t0, &holes);
        assert_eq!(rot.step(t0 + ROTATION_INTERVAL, &holes), vec![RotationAction::Drain(2), RotationAction::Open]);
    }

    #[test]
    fn rotations_repeat_every_minute_not_more_often() {
        let t0 = Instant::now();
        let holes = [hole(1, HoleState::Active, Some(0.1), 0), hole(2, HoleState::Active, Some(0.2), 0)];
        let mut rot = Rotation::new();
        rot.step(t0, &holes);
        rot.step(t0 + ROTATION_INTERVAL, &holes);
        assert!(rot.step(t0 + ROTATION_INTERVAL + Duration::from_secs(30), &holes).is_empty());
        assert!(!rot.step(t0 + ROTATION_INTERVAL * 2, &holes).is_empty());
    }

    #[test]
    fn drained_hole_is_released_after_ten_quiet_seconds() {
        let t0 = Instant::now();
        let active = [hole(1, HoleState::Active, Some(0.1), 0), hole(2, HoleState::Active, Some(0.2), 0)];
        let mut rot = Rotation::new();
        rot.step(t0, &active);
        let drain_at = t0 + ROTATION_INTERVAL;
        rot.step(drain_at, &active);
        // Пакетов от пира после слива нет: тишина в снимке растёт вместе со временем.
        let quiet_since = |secs: u64| [hole(2, HoleState::Draining, Some(0.2), secs), hole(1, HoleState::Active, Some(0.1), 0)];
        assert!(rot.step(drain_at + Duration::from_secs(5), &quiet_since(5)).is_empty());
        assert_eq!(rot.step(drain_at + QUIET_TO_RELEASE, &quiet_since(10)), vec![RotationAction::Release(2)]);
    }

    #[test]
    fn packets_still_arriving_keep_a_drained_hole_alive_until_the_cap() {
        let t0 = Instant::now();
        let active = [hole(1, HoleState::Active, Some(0.1), 0), hole(2, HoleState::Active, Some(0.2), 0)];
        let mut rot = Rotation::new();
        rot.step(t0, &active);
        let drain_at = t0 + ROTATION_INTERVAL;
        rot.step(drain_at, &active);
        // Пакеты приходят (тишина 0), пока не сработала страховка.
        let busy = [hole(2, HoleState::Draining, Some(0.2), 0)];
        assert!(rot.step(drain_at + QUIET_TO_RELEASE * 3, &busy).is_empty());
        assert_eq!(rot.step(drain_at + DRAIN_MAX, &busy), vec![RotationAction::Release(2)]);
    }

    #[test]
    fn a_hole_that_was_already_silent_still_waits_ten_seconds_after_the_drain() {
        let t0 = Instant::now();
        let active = [hole(1, HoleState::Active, Some(0.1), 0), hole(2, HoleState::Active, Some(0.2), 500)];
        let mut rot = Rotation::new();
        rot.step(t0, &active);
        let drain_at = t0 + ROTATION_INTERVAL;
        rot.step(drain_at, &active);
        let draining = [hole(2, HoleState::Draining, Some(0.2), 500), hole(1, HoleState::Active, Some(0.1), 0)];
        assert!(rot.step(drain_at + Duration::from_secs(1), &draining).is_empty(), "тишина считается от слива, не от прошлых пакетов");
        assert_eq!(rot.step(drain_at + QUIET_TO_RELEASE, &draining), vec![RotationAction::Release(2)]);
    }

    #[test]
    fn released_hole_is_forgotten_and_not_released_twice() {
        let t0 = Instant::now();
        let active = [hole(1, HoleState::Active, Some(0.1), 0), hole(2, HoleState::Active, Some(0.2), 0)];
        let mut rot = Rotation::new();
        rot.step(t0, &active);
        let drain_at = t0 + ROTATION_INTERVAL;
        rot.step(drain_at, &active);
        let quiet_10 = [hole(2, HoleState::Draining, Some(0.2), 10), hole(1, HoleState::Active, Some(0.1), 0)];
        assert_eq!(rot.step(drain_at + QUIET_TO_RELEASE, &quiet_10), vec![RotationAction::Release(2)]);
        // Дыра ещё числится в снимке (пока её не закрыли) — второго освобождения быть не должно.
        let quiet_11 = [hole(2, HoleState::Draining, Some(0.2), 11), hole(1, HoleState::Active, Some(0.1), 0)];
        assert!(rot.step(drain_at + QUIET_TO_RELEASE + Duration::from_secs(1), &quiet_11).is_empty());
    }

    #[test]
    fn active_holes_are_never_released() {
        let t0 = Instant::now();
        let holes = [hole(1, HoleState::Active, Some(0.1), 900), hole(2, HoleState::Active, Some(0.2), 900)];
        let mut rot = Rotation::new();
        rot.step(t0, &holes);
        assert!(rot.step(t0 + ROTATION_INTERVAL * 5, &holes).iter().all(|a| !matches!(a, RotationAction::Release(_))));
    }
}
