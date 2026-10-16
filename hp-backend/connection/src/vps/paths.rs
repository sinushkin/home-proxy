//! Качество путей клиента: напрямую на сервер или через релей. Раз в минуту клиент сравнивает
//! потери по каждому пути (дыры этого пути, взвешенно по пакетам); путь с потерями больше чем вдвое
//! выше лучшего (но не ниже `FLOOR`) уходит в чёрный список на `BAN`: новых дыр на нём не открываем,
//! живые доживают своё. Лучший путь в список не попадает никогда, а если в списке оказались все
//! доступные пути — список не действует (лучше плохой интернет, чем никакого). Порт знакомства
//! сервера список не трогает.
//!
//! Модуль чистый: время и снимок дыр передаются снаружи.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::multilink::HoleStatus;

/// Путь дыры: `None` — напрямую на сервер, `Some` — порт знакомства релея.
pub(crate) type Route = Option<SocketAddr>;

/// Как часто сравниваем пути.
pub(crate) const CHECK: Duration = Duration::from_secs(60);
/// На сколько путь уходит в чёрный список (потом пробуем снова: без дыр на нём свежих данных нет).
const BAN: Duration = Duration::from_secs(600);
/// Во сколько раз потери пути должны превышать лучшие, чтобы путь считался плохим.
const RATIO: f32 = 2.0;
/// Потери ниже этого не сравниваем (0,1% против 0,3% — не «втрое хуже», а шум).
const FLOOR: f32 = 0.01;
/// Минимум пакетов по пути для оценки.
const MIN_SAMPLE: u64 = 200;
/// Молодые дыры в оценку не берём: у них потери завышены задержкой отчёта пира.
const MIN_AGE: Duration = Duration::from_secs(25);

/// Потери по одному пути.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PathLoss {
    pub route: Route,
    pub loss: f32,
    pub sample: u64,
}

/// Потери по путям: худшее из двух направлений, взвешенно по пакетам дыр пути.
pub(crate) fn path_losses(holes: &[HoleStatus]) -> Vec<PathLoss> {
    // (потеряно туда, отправлено, потеряно оттуда, получено)
    let mut by_route: HashMap<Route, (f64, u64, f64, u64)> = HashMap::new();
    for h in holes.iter().filter(|h| h.age >= MIN_AGE) {
        let route = match &h.via {
            None => None,
            Some(via) => match via.parse() {
                Ok(addr) => Some(addr),
                Err(_) => continue,
            },
        };
        let entry = by_route.entry(route).or_default();
        if let Some(loss) = h.loss_out {
            entry.0 += f64::from(loss) * h.sent as f64;
            entry.1 += h.sent;
        }
        if let Some(loss) = h.loss_in {
            entry.2 += f64::from(loss) * h.received as f64;
            entry.3 += h.received;
        }
    }
    let ratio = |lost: f64, total: u64| if total == 0 { 0.0 } else { lost / total as f64 };
    let mut losses: Vec<PathLoss> = by_route
        .into_iter()
        .map(|(route, (lost_out, sent, lost_in, received))| PathLoss {
            route,
            loss: ratio(lost_out, sent).max(ratio(lost_in, received)) as f32,
            sample: sent.max(received),
        })
        .collect();
    losses.sort_by_key(|p| p.route);
    losses
}

/// Плохие пути: потери выше `RATIO` × лучших (не ниже `FLOOR`); сравниваем, только если путей с
/// достаточной выборкой хотя бы два. Возвращает (путь, его потери, лучшие потери).
pub(crate) fn judge(losses: &[PathLoss]) -> Vec<(Route, f32, f32)> {
    let eligible: Vec<&PathLoss> = losses.iter().filter(|p| p.sample >= MIN_SAMPLE).collect();
    if eligible.len() < 2 {
        return Vec::new();
    }
    let best = eligible.iter().map(|p| p.loss).fold(f32::INFINITY, f32::min);
    let limit = RATIO * best.max(FLOOR);
    eligible.into_iter().filter(|p| p.loss > limit).map(|p| (p.route, p.loss, best)).collect()
}

/// Чёрный список путей: до какого момента путь закрыт для новых дыр.
#[derive(Default)]
pub(crate) struct Blacklist {
    until: HashMap<Route, Instant>,
}

impl Blacklist {
    pub(crate) fn ban(&mut self, route: Route, now: Instant) {
        self.until.insert(route, now + BAN);
    }

    /// Убирает истёкшие; возвращает пути, которые снова открыты.
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<Route> {
        let expired: Vec<Route> = self.until.iter().filter(|(_, until)| **until <= now).map(|(route, _)| *route).collect();
        for route in &expired {
            self.until.remove(route);
        }
        expired
    }

    pub(crate) fn banned(&self, route: Route, now: Instant) -> bool {
        self.until.get(&route).is_some_and(|until| now < *until)
    }

    /// Какие пути можно брать для новой дыры: напрямую ли и какие релеи (из разрешённых сервером).
    /// Если закрыто всё — не действует: открываем всё доступное.
    pub(crate) fn allowed(&self, relays: &[SocketAddr], now: Instant) -> (bool, Vec<SocketAddr>) {
        let direct = !self.banned(None, now);
        let relays_ok: Vec<SocketAddr> = relays.iter().copied().filter(|r| !self.banned(Some(*r), now)).collect();
        if !direct && relays_ok.is_empty() {
            return (true, relays.to_vec());
        }
        (direct, relays_ok)
    }
}

/// Имя пути для лога.
pub(crate) fn route_name(route: Route) -> String {
    match route {
        None => "напрямую на сервер".into(),
        Some(relay) => format!("через релей {relay}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::holes::HoleState;
    use std::sync::Arc;

    fn hole(via: Option<&str>, age_s: u64, sent: u64, loss_out: f32) -> HoleStatus {
        HoleStatus {
            slot: 0,
            peer_addr: None,
            sent,
            received: sent,
            loss_out: Some(loss_out),
            loss_in: Some(0.0),
            age: Duration::from_secs(age_s),
            state: HoleState::Active,
            via: via.map(Arc::from),
        }
    }

    const R: &str = "203.0.113.20:40600";

    #[test]
    fn losses_are_weighted_per_route_and_young_holes_skipped() {
        let holes = [hole(None, 60, 900, 0.20), hole(None, 60, 100, 0.0), hole(Some(R), 60, 500, 0.01), hole(Some(R), 5, 50, 0.9)];
        let losses = path_losses(&holes);
        assert_eq!(losses.len(), 2);
        assert_eq!(losses[0].route, None);
        assert!((losses[0].loss - 0.18).abs() < 1e-4 && losses[0].sample == 1000);
        assert_eq!(losses[1].route, Some(R.parse().unwrap()));
        assert!((losses[1].loss - 0.01).abs() < 1e-4 && losses[1].sample == 500, "молодая дыра (5 с) не учтена");
    }

    #[test]
    fn a_path_twice_worse_than_the_best_is_bad_but_noise_is_not() {
        let relay = Some(R.parse().unwrap());
        let p = |route, loss| PathLoss { route, loss, sample: 1000 };
        assert_eq!(judge(&[p(None, 0.18), p(relay, 0.01)]), vec![(None, 0.18, 0.01)], "18% против 1%");
        assert!(judge(&[p(None, 0.003), p(relay, 0.001)]).is_empty(), "0,3% против 0,1% — шум, ниже FLOOR");
        assert!(judge(&[p(None, 0.15), p(relay, 0.10)]).is_empty(), "15% против 10% — не вдвое");
        assert!(judge(&[p(None, 0.5)]).is_empty(), "один путь не с чем сравнивать");
        let small = PathLoss { route: relay, loss: 0.0, sample: 50 };
        assert!(judge(&[p(None, 0.5), small]).is_empty(), "путь с малой выборкой не в счёт");
    }

    #[test]
    fn blacklist_never_closes_every_path() {
        let now = Instant::now();
        let relay: SocketAddr = R.parse().unwrap();
        let mut list = Blacklist::default();
        assert_eq!(list.allowed(&[relay], now), (true, vec![relay]));
        list.ban(None, now);
        assert_eq!(list.allowed(&[relay], now), (false, vec![relay]), "сервер плохой — только релей");
        list.ban(Some(relay), now);
        assert_eq!(list.allowed(&[relay], now), (true, vec![relay]), "плохо всё — список не действует");
        assert_eq!(list.allowed(&[], now), (true, vec![]), "релеев нет — напрямую, даже если сервер плохой");
        assert_eq!(list.expire(now + BAN).len(), 2);
        assert!(!list.banned(None, now + BAN));
    }
}
