//! Слив дыры по договорённости — чистый конечный автомат (без сети, часов и задач): события и
//! время приходят параметрами, ответ — список действий для оболочки (`multilink::SlotBase::drain`).
//! Поэтому вся логика проверяется детерминированными юнит-тестами.
//!
//! Правила:
//! - Кто просит слить (`Requester`), тот шлёт `Drain` и до подтверждения держит дыру в работе
//!   (данные по ней идут). Просьба повторяется каждые `RETRY`; прошлой версии пира, не знающей
//!   `Drain`, во второй половине ожидания шлётся ещё и `DeleteLink`. `Drain` пира в ответ —
//!   подтверждение; не пришло за `CONFIRM_WAIT` — сливаем сами.
//! - Кто получил просьбу (`Receiver`), сразу перестаёт слать по дыре и подтверждает таким же
//!   `Drain`; повторную просьбу подтверждает снова, но не чаще раза в `RETRY`.
//! - С момента подтверждения дыра `Draining`: данные по ней не шлём, принимаем ещё `GRACE`
//!   (доходит всё, что пир отправил до подтверждения), потом дыра закрывается.
//! - Если просили оба (просьбы встретились), каждая принимается как подтверждение другой.

use std::time::{Duration, Instant};

/// Сколько просящий ждёт подтверждения, прежде чем слить сам.
pub(crate) const CONFIRM_WAIT: Duration = Duration::from_secs(2);
/// Как часто повторяем просьбу (и не чаще повторяем подтверждение).
pub(crate) const RETRY: Duration = Duration::from_millis(300);
/// Сколько дыра ещё принимает после подтверждения слива.
pub(crate) const GRACE: Duration = Duration::from_millis(2100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Послать пиру `Drain` (просьбу или подтверждение); `delete_link` — ещё и `DeleteLink` для прошлых версий.
    SendDrain { delete_link: bool },
    /// Данные по дыре больше не шлём.
    MarkDraining,
    /// Дыру закрыть.
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Просили мы, подтверждения ещё нет: дыра в работе.
    Confirming { started: Instant, last_sent: Instant },
    /// Подтверждено (или ждать надоело): не шлём, принимаем до `until`.
    Draining { until: Instant, last_confirm: Instant },
    Closed,
}

#[derive(Debug)]
pub(crate) struct Drain {
    requested: bool,
    state: State,
}

impl Drain {
    /// Мы просим слить дыру.
    pub(crate) fn request(now: Instant) -> (Self, Vec<Action>) {
        (Self { requested: true, state: State::Confirming { started: now, last_sent: now } }, vec![Action::SendDrain { delete_link: false }])
    }

    /// Пир просит слить дыру: подтверждаем и перестаём слать.
    pub(crate) fn receive(now: Instant) -> (Self, Vec<Action>) {
        (
            Self { requested: false, state: State::Draining { until: now + GRACE, last_confirm: now } },
            vec![Action::SendDrain { delete_link: false }, Action::MarkDraining],
        )
    }

    /// Пришёл `Drain` (или `DeleteLink`) пира по этой дыре.
    pub(crate) fn on_peer_drain(&mut self, now: Instant) -> Vec<Action> {
        match self.state {
            // Подтверждение нашей просьбы (или встречная просьба).
            State::Confirming { .. } => {
                self.state = State::Draining { until: now + GRACE, last_confirm: now };
                vec![Action::MarkDraining]
            }
            // Пир повторил просьбу — наше подтверждение, видимо, не дошло. Если просили мы, это
            // уже подтверждённый слив: не отвечаем (иначе обмен не кончится).
            State::Draining { until, last_confirm } if !self.requested && now.duration_since(last_confirm) >= RETRY => {
                self.state = State::Draining { until, last_confirm: now };
                vec![Action::SendDrain { delete_link: false }]
            }
            _ => Vec::new(),
        }
    }

    /// Время идёт: повтор просьбы, конец ожидания, конец приёма.
    pub(crate) fn on_tick(&mut self, now: Instant) -> Vec<Action> {
        match self.state {
            State::Confirming { started, last_sent } => {
                if now.duration_since(started) >= CONFIRM_WAIT {
                    self.state = State::Draining { until: now + GRACE, last_confirm: now };
                    vec![Action::MarkDraining]
                } else if now.duration_since(last_sent) >= RETRY {
                    self.state = State::Confirming { started, last_sent: now };
                    // Прошлой версии, не знающей `Drain`, во второй половине ожидания шлём ещё и `DeleteLink`.
                    vec![Action::SendDrain { delete_link: now.duration_since(started) >= CONFIRM_WAIT / 2 }]
                } else {
                    Vec::new()
                }
            }
            State::Draining { until, .. } if now >= until => {
                self.state = State::Closed;
                vec![Action::Close]
            }
            _ => Vec::new(),
        }
    }

    /// Когда оболочке проснуться в следующий раз; `None` — слив закончен.
    pub(crate) fn next_wakeup(&self) -> Option<Instant> {
        match self.state {
            State::Confirming { started, last_sent } => Some((last_sent + RETRY).min(started + CONFIRM_WAIT)),
            State::Draining { until, .. } => Some(until),
            State::Closed => None,
        }
    }

    /// Дыру уже нельзя использовать для данных (слив подтверждён).
    #[cfg(test)]
    pub(crate) fn is_draining(&self) -> bool {
        matches!(self.state, State::Draining { .. })
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.state == State::Closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn requester_keeps_the_hole_in_work_until_the_peer_confirms() {
        let t0 = Instant::now();
        let (mut d, first) = Drain::request(t0);
        assert_eq!(first, vec![Action::SendDrain { delete_link: false }], "просьба уходит сразу");
        assert!(!d.is_draining(), "до подтверждения дыра в работе");
        assert_eq!(d.on_tick(t0 + ms(100)), vec![], "повтор не раньше RETRY");
        assert_eq!(d.on_tick(t0 + ms(300)), vec![Action::SendDrain { delete_link: false }], "просьба повторяется");
        assert!(!d.is_draining());
        // Подтверждение пришло.
        assert_eq!(d.on_peer_drain(t0 + ms(450)), vec![Action::MarkDraining]);
        assert!(d.is_draining());
        // Принимаем ещё GRACE и закрываемся.
        assert_eq!(d.on_tick(t0 + ms(450) + GRACE - ms(1)), vec![]);
        assert_eq!(d.on_tick(t0 + ms(450) + GRACE), vec![Action::Close]);
        assert!(d.is_closed());
        assert_eq!(d.next_wakeup(), None);
    }

    #[test]
    fn requester_without_an_answer_gives_up_after_the_wait_and_asks_old_peers_by_delete_link() {
        let t0 = Instant::now();
        let (mut d, _) = Drain::request(t0);
        let mut sends = Vec::new();
        let mut t = t0;
        while !d.is_draining() {
            t += RETRY;
            for action in d.on_tick(t) {
                match action {
                    Action::SendDrain { delete_link } => sends.push((t.duration_since(t0), delete_link)),
                    Action::MarkDraining => {}
                    Action::Close => panic!("закрыться раньше приёма"),
                }
            }
        }
        // DeleteLink — только во второй половине ожидания.
        assert!(sends.iter().filter(|(_, delete)| !*delete).all(|(at, _)| *at < CONFIRM_WAIT / 2));
        assert!(sends.iter().filter(|(_, delete)| *delete).all(|(at, _)| *at >= CONFIRM_WAIT / 2));
        assert!(sends.iter().any(|(_, delete)| *delete), "прошлой версии DeleteLink всё же уходит");
        assert!(t.duration_since(t0) >= CONFIRM_WAIT && t.duration_since(t0) < CONFIRM_WAIT + RETRY, "ждали ровно CONFIRM_WAIT");
        // Дальше — тот же приём GRACE и закрытие.
        assert_eq!(d.on_tick(t + GRACE), vec![Action::Close]);
    }

    #[test]
    fn receiver_confirms_at_once_stops_sending_and_closes_after_grace() {
        let t0 = Instant::now();
        let (mut d, first) = Drain::receive(t0);
        assert_eq!(first, vec![Action::SendDrain { delete_link: false }, Action::MarkDraining]);
        assert!(d.is_draining());
        assert_eq!(d.on_tick(t0 + GRACE - ms(1)), vec![]);
        assert_eq!(d.on_tick(t0 + GRACE), vec![Action::Close]);
    }

    #[test]
    fn receiver_repeats_the_confirmation_on_a_repeated_request_but_not_more_often_than_retry() {
        let t0 = Instant::now();
        let (mut d, _) = Drain::receive(t0);
        // Просьба идёт сразу по нескольким дырам: дубли в пределах RETRY не множат ответов.
        assert_eq!(d.on_peer_drain(t0 + ms(10)), vec![]);
        assert_eq!(d.on_peer_drain(t0 + ms(20)), vec![]);
        // Следующая волна просьбы (наше подтверждение не дошло) — подтверждаем снова.
        assert_eq!(d.on_peer_drain(t0 + ms(310)), vec![Action::SendDrain { delete_link: false }]);
        assert_eq!(d.on_peer_drain(t0 + ms(320)), vec![]);
        // Срок приёма от повторов не продлевается.
        assert_eq!(d.on_tick(t0 + GRACE), vec![Action::Close]);
    }

    #[test]
    fn requester_does_not_answer_the_peers_confirmation_again() {
        let t0 = Instant::now();
        let (mut d, _) = Drain::request(t0);
        assert_eq!(d.on_peer_drain(t0 + ms(100)), vec![Action::MarkDraining]);
        // Лишние `Drain` пира (он подтверждает по нескольким дырам и повторяет) — без ответа.
        for n in 1..10 {
            assert_eq!(d.on_peer_drain(t0 + ms(100 + 400 * n)), vec![], "ответ на подтверждение вызвал бы вечный обмен");
        }
    }

    #[test]
    fn crossing_requests_confirm_each_other() {
        let t0 = Instant::now();
        let (mut a, _) = Drain::request(t0);
        let (mut b, _) = Drain::request(t0);
        // Просьба каждого доходит до другого и считается подтверждением.
        assert_eq!(a.on_peer_drain(t0 + ms(40)), vec![Action::MarkDraining]);
        assert_eq!(b.on_peer_drain(t0 + ms(40)), vec![Action::MarkDraining]);
        assert!(a.is_draining() && b.is_draining());
    }

    #[test]
    fn wakeup_follows_the_next_deadline() {
        let t0 = Instant::now();
        let (mut d, _) = Drain::request(t0);
        assert_eq!(d.next_wakeup(), Some(t0 + RETRY));
        d.on_tick(t0 + RETRY);
        assert_eq!(d.next_wakeup(), Some(t0 + 2 * RETRY));
        d.on_peer_drain(t0 + ms(400));
        assert_eq!(d.next_wakeup(), Some(t0 + ms(400) + GRACE));
    }
}
