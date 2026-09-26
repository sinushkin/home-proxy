//! Восстановление порядка TCP-пакетов на приёме.
//!
//! Пакеты раскладываются по десяти дырам, поэтому на приёме они приходят не по порядку, и TCP
//! принимает это за потери. Отправитель нумерует TCP-пакеты внутри корзины потока (`Ordered`,
//! `WrappedData` с корзиной, см. `hp_tun::bridge`): по корзине и номеру возвращаем порядок.
//! Недостающий пакет ждём не дольше `wait`, потом отдаём то, что накопилось (опоздавший пакет
//! передаём дальше — TCP сам разберётся). Пакеты без номера (UDP, ICMP, служебные) идут сразу.
//!
//! Модуль чистый: время передаётся снаружи, таймеров и каналов здесь нет.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::multilink::Incoming;

/// Дальше этого «прыжка» вперёд считаем, что сессия сбилась: буфер не копим.
const MAX_GAP: u64 = RING as u64 - 1;
/// Сколько пакетов держим на одну сессию.
const RING: usize = 512;
const MAX_SESSIONS: usize = 16;

/// Пределы адаптивного ожидания.
pub const MIN_WAIT: Duration = Duration::from_millis(3);
pub const MAX_WAIT: Duration = Duration::from_millis(30);
/// Сколько последних отставаний помним и как часто пересчитываем ожидание.
const LATENESS_WINDOW: usize = 512;
const RECALC_EVERY: usize = 64;
/// Сколько пропущенных (выданных по таймауту) номеров помним, чтобы измерить опоздавшего.
const MAX_SKIPPED: usize = 1024;

/// Счётчики работы буфера порядка.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReorderStats {
    /// Пакеты без счётчика (рукопожатие и т. п.), прошли сразу.
    pub passthrough: u64,
    /// Пришли по порядку, прошли сразу.
    pub in_order: u64,
    /// Были придержаны и выданы по порядку, когда пришёл недостающий.
    pub reordered: u64,
    /// Придержаны и выданы по таймауту (недостающий не пришёл вовремя).
    pub timed_out: u64,
    /// Пришли позже своего места (опоздавшие), выданы сразу.
    pub late: u64,
    /// Выданы досрочно из-за переполнения буфера или скачка счётчика.
    pub forced: u64,
    /// Наибольшее число одновременно придержанных пакетов в одной сессии.
    pub max_held: usize,
    /// Текущее ожидание недостающего пакета, мс (подстраивается под сеть).
    pub wait_ms: u64,
}

struct Held {
    packet: Incoming,
    arrived: Instant,
}

/// Придержанные пакеты одной сессии: кольцо фиксированного размера по счётчику (без выделения
/// памяти на пакет; кольцо выделяется один раз, при первом придержанном пакете). Хранит счётчики
/// из окна `[next, next + RING)`, поэтому пробел больше `RING - 1` не ждём (`MAX_GAP`).
struct HeldRing {
    slots: Vec<Option<Held>>,
    count: usize,
    /// Наименьший и наибольший придержанный счётчик (при `count > 0`).
    lo: u64,
    hi: u64,
    /// Самый ранний приход среди придержанных (от него считается ожидание).
    oldest: Option<Instant>,
}

impl HeldRing {
    fn new() -> Self {
        Self { slots: Vec::new(), count: 0, lo: 0, hi: 0, oldest: None }
    }

    fn len(&self) -> usize {
        self.count
    }

    fn first(&self) -> Option<u64> {
        (self.count > 0).then_some(self.lo)
    }

    fn index(counter: u64) -> usize {
        (counter % RING as u64) as usize
    }

    fn insert(&mut self, counter: u64, held: Held) {
        if self.slots.is_empty() {
            self.slots.resize_with(RING, || None);
        }
        let arrived = held.arrived;
        let slot = &mut self.slots[Self::index(counter)];
        if slot.is_none() {
            self.count += 1;
        }
        *slot = Some(held);
        if self.count == 1 {
            self.lo = counter;
            self.hi = counter;
        } else {
            self.lo = self.lo.min(counter);
            self.hi = self.hi.max(counter);
        }
        self.oldest = Some(self.oldest.map_or(arrived, |o| o.min(arrived)));
    }

    fn remove(&mut self, counter: u64) -> Option<Held> {
        if self.count == 0 || counter < self.lo || counter > self.hi {
            return None;
        }
        let held = self.slots[Self::index(counter)].take()?;
        self.count -= 1;
        if self.count == 0 {
            self.oldest = None;
            return Some(held);
        }
        if counter == self.lo {
            self.lo = (counter + 1..=self.hi).find(|c| self.slots[Self::index(*c)].is_some()).expect("count > 0");
        }
        if counter == self.hi {
            self.hi = (self.lo..counter).rev().find(|c| self.slots[Self::index(*c)].is_some()).expect("count > 0");
        }
        if self.oldest == Some(held.arrived) {
            self.oldest = (self.lo..=self.hi).filter_map(|c| self.slots[Self::index(c)].as_ref().map(|h| h.arrived)).min();
        }
        Some(held)
    }
}

struct Session {
    /// Следующий ожидаемый счётчик.
    next: u64,
    held: HeldRing,
    last_seen: Instant,
    /// Номера, выданные дальше без ожидания (не дождались), и когда был замечен их пробел.
    skipped: HashMap<u64, Instant>,
}

impl Session {
    fn oldest_arrival(&self) -> Option<Instant> {
        self.held.oldest
    }
}

/// Сессия порядка: корзина потока (`Ordered`) или корзина потока клиента за роутером
/// (`WrappedData` с корзиной). Разные виды не смешиваются.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SessionKey {
    Flow(u32),
    Client(u8, u32),
}

impl std::fmt::LowerHex for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionKey::Flow(flow) => write!(f, "поток:{flow:x}"),
            SessionKey::Client(client, flow) => write!(f, "клиент {client}, поток:{flow:x}"),
        }
    }
}

pub struct Resequencer {
    wait: Duration,
    adaptive: bool,
    /// Последние измеренные отставания (кольцо) и место под их сортировку.
    lateness: Vec<Duration>,
    lateness_sorted: Vec<Duration>,
    lateness_pos: usize,
    since_recalc: usize,
    sessions: HashMap<SessionKey, Session>,
    stats: ReorderStats,
}

impl Resequencer {
    /// Фиксированное ожидание `wait`.
    pub fn new(wait: Duration) -> Self {
        Self {
            wait,
            adaptive: false,
            lateness: Vec::new(),
            lateness_sorted: Vec::new(),
            lateness_pos: 0,
            since_recalc: 0,
            sessions: HashMap::new(),
            stats: ReorderStats { wait_ms: wait.as_millis() as u64, ..ReorderStats::default() },
        }
    }

    /// Ожидание начинается с `initial` и подстраивается под измеренное отставание:
    /// p99 последних отсчётов с запасом 25%, в пределах `MIN_WAIT..=MAX_WAIT`.
    pub fn adaptive(initial: Duration) -> Self {
        let mut r = Self::new(initial.clamp(MIN_WAIT, MAX_WAIT));
        r.adaptive = true;
        r
    }

    pub fn wait(&self) -> Duration {
        self.wait
    }

    pub fn stats(&self) -> ReorderStats {
        self.stats
    }

    /// Отставание одного переставленного пакета: от обнаружения пробела до его прихода.
    fn record_lateness(&mut self, lateness: Duration) {
        if !self.adaptive {
            return;
        }
        if self.lateness.len() < LATENESS_WINDOW {
            if self.lateness.capacity() == 0 {
                self.lateness.reserve_exact(LATENESS_WINDOW);
            }
            self.lateness.push(lateness);
        } else {
            self.lateness[self.lateness_pos] = lateness;
            self.lateness_pos = (self.lateness_pos + 1) % LATENESS_WINDOW;
        }
        self.since_recalc += 1;
        if self.since_recalc >= RECALC_EVERY {
            self.since_recalc = 0;
            self.recalc_wait();
        }
    }

    fn recalc_wait(&mut self) {
        // Сортируем копию в заранее выделенном месте (без выделения памяти после первого раза).
        self.lateness_sorted.clear();
        self.lateness_sorted.extend_from_slice(&self.lateness);
        self.lateness_sorted.sort_unstable();
        let sorted = &self.lateness_sorted;
        let p99 = sorted[((sorted.len() - 1) as f64 * 0.99).round() as usize];
        let wait = (p99 + p99 / 4).clamp(MIN_WAIT, MAX_WAIT);
        if wait != self.wait {
            log::trace!(
                "порядок: ожидание {} -> {} мс (p99 отставания {:.1} мс по {} отсчётам)",
                self.wait.as_millis(),
                wait.as_millis(),
                p99.as_secs_f64() * 1000.0,
                sorted.len()
            );
            self.wait = wait;
            self.stats.wait_ms = wait.as_millis() as u64;
        }
    }

    /// Принимает пакет и дописывает в `out` те, что можно отдавать дальше сейчас (по порядку).
    /// `out` вызывающий переиспользует между пакетами: выделения памяти на пакет нет.
    pub fn push_into(&mut self, packet: Incoming, now: Instant, out: &mut Vec<Incoming>) {
        let key = match packet.order {
            Some((flow, seq)) => match packet.wrapped {
                Some(info) => Some((SessionKey::Client(info.client_id, flow), seq)),
                None => Some((SessionKey::Flow(flow), seq)),
            },
            None => None,
        };
        let Some((index, counter)) = key else {
            self.stats.passthrough += 1;
            out.push(packet);
            return;
        };
        if self.sessions.len() >= MAX_SESSIONS && !self.sessions.contains_key(&index) {
            self.prune();
        }
        let stats = &mut self.stats;
        let session = self
            .sessions
            .entry(index)
            .or_insert_with(|| Session { next: counter, held: HeldRing::new(), last_seen: now, skipped: HashMap::new() });
        session.last_seen = now;

        if counter < session.next {
            stats.late += 1;
            let noticed = session.skipped.remove(&counter);
            log::trace!("порядок: сессия {index:#x}: опоздавший пакет {counter} (ждём {}), отдаём сразу", session.next);
            out.push(packet);
            if let Some(noticed) = noticed {
                self.record_lateness(now - noticed);
            }
            return;
        }
        if counter == session.next {
            stats.in_order += 1;
            let gap_noticed = session.oldest_arrival();
            let before = out.len();
            out.push(packet);
            session.next = counter + 1;
            let held_before = session.held.len();
            drain(session, out, &mut stats.reordered);
            if held_before > 0 {
                log::trace!(
                    "порядок: сессия {index:#x}: пришёл недостающий {counter}, выдано {} придержанных, осталось {}",
                    out.len() - before - 1,
                    session.held.len()
                );
            }
            if let Some(noticed) = gap_noticed {
                self.record_lateness(now - noticed);
            }
            return;
        }
        if counter - session.next > MAX_GAP {
            // Сессия ушла далеко вперёд: пробел не ждём.
            let before = out.len();
            flush(session, out);
            stats.forced += (out.len() - before) as u64;
            log::trace!("порядок: сессия {index:#x}: скачок {} -> {counter}, выдано {} придержанных", session.next, out.len() - before);
            out.push(packet);
            session.next = counter + 1;
            return;
        }
        log::trace!(
            "порядок: сессия {index:#x}: пакет {counter} раньше ожидаемого {} (пробел {}), придержано {}",
            session.next,
            counter - session.next,
            session.held.len() + 1
        );
        session.held.insert(counter, Held { packet, arrived: now });
        stats.max_held = stats.max_held.max(session.held.len());
    }

    /// Ближайший момент, когда `expire_into` что-то выдаст (для таймера).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.sessions.values().filter_map(Session::oldest_arrival).min().map(|t| t + self.wait)
    }

    /// Дописывает в `out` придержанные пакеты, недостающего для которых мы не дождались.
    pub fn expire_into(&mut self, now: Instant, out: &mut Vec<Incoming>) {
        for (index, session) in self.sessions.iter_mut() {
            while let Some(oldest) = session.oldest_arrival() {
                if now < oldest + self.wait {
                    break;
                }
                let before = out.len();
                let missing = session.next;
                if let Some(lowest) = session.held.first() {
                    if session.skipped.len() + (lowest - missing) as usize > MAX_SKIPPED {
                        session.skipped.clear();
                    }
                    for skipped in missing..lowest.min(missing + MAX_SKIPPED as u64) {
                        session.skipped.insert(skipped, oldest);
                    }
                }
                release_lowest(session, out);
                self.stats.timed_out += (out.len() - before) as u64;
                log::trace!(
                    "порядок: сессия {index:#x}: не дождались {missing} за {} мс, выдано {}, дальше ждём {}",
                    self.wait.as_millis(),
                    out.len() - before,
                    session.next
                );
            }
        }
    }

    fn prune(&mut self) {
        if let Some((&oldest, _)) = self.sessions.iter().min_by_key(|(_, s)| s.last_seen) {
            self.sessions.remove(&oldest);
        }
    }

    /// Для тестов: `push_into` с новым `Vec`.
    #[cfg(test)]
    pub fn push(&mut self, packet: Incoming, now: Instant) -> Vec<Incoming> {
        let mut out = Vec::new();
        self.push_into(packet, now, &mut out);
        out
    }

    /// Для тестов: `expire_into` с новым `Vec`.
    #[cfg(test)]
    pub fn expire(&mut self, now: Instant) -> Vec<Incoming> {
        let mut out = Vec::new();
        self.expire_into(now, &mut out);
        out
    }
}

/// Выдаёт подряд идущие придержанные пакеты, начиная с `session.next`.
fn drain(session: &mut Session, out: &mut Vec<Incoming>, counted: &mut u64) {
    while let Some(held) = session.held.remove(session.next) {
        out.push(held.packet);
        session.next += 1;
        *counted += 1;
    }
}

/// Пропускает пробел: выдаёт самый младший придержанный пакет и всё, что идёт за ним подряд.
fn release_lowest(session: &mut Session, out: &mut Vec<Incoming>) {
    let Some(lowest) = session.held.first() else { return };
    session.next = lowest;
    while let Some(held) = session.held.remove(session.next) {
        out.push(held.packet);
        session.next += 1;
    }
}

/// Выдаёт все придержанные пакеты по возрастанию счётчика.
fn flush(session: &mut Session, out: &mut Vec<Incoming>) {
    while let Some(lowest) = session.held.first() {
        if let Some(held) = session.held.remove(lowest) {
            out.push(held.packet);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_millis(8);

    /// TCP-пакет корзины `flow` с номером `counter` (пришёл по дыре `counter % 10`).
    fn pkt(flow: u32, counter: u64) -> Incoming {
        let mut packet = ordered(flow, counter);
        packet.slot = (counter % 10) as u8;
        packet
    }

    fn counters(packets: &[Incoming]) -> Vec<u64> {
        packets.iter().map(|p| p.order.unwrap().1).collect()
    }

    #[test]
    fn in_order_packets_pass_immediately() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        for counter in 5..10 {
            assert_eq!(counters(&r.push(pkt(1, counter), now)), vec![counter]);
        }
        assert_eq!(r.stats().in_order, 5);
        assert_eq!(r.next_deadline(), None);
    }

    #[test]
    fn swapped_pair_is_released_in_order_without_waiting() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        r.push(pkt(1, 0), now);
        assert!(r.push(pkt(1, 2), now).is_empty());
        assert_eq!(counters(&r.push(pkt(1, 1), now)), vec![1, 2]);
        assert_eq!(r.stats().reordered, 1);
        assert_eq!(r.next_deadline(), None);
    }

    #[test]
    fn missing_packet_is_skipped_after_the_wait() {
        let mut r = Resequencer::new(WAIT);
        let start = Instant::now();
        r.push(pkt(1, 0), start);
        assert!(r.push(pkt(1, 2), start).is_empty());
        assert!(r.push(pkt(1, 3), start + Duration::from_millis(2)).is_empty());
        assert_eq!(r.next_deadline(), Some(start + WAIT));
        assert!(r.expire(start + Duration::from_millis(7)).is_empty());
        assert_eq!(counters(&r.expire(start + WAIT)), vec![2, 3]);
        assert_eq!(r.stats().timed_out, 2);
        // дальше нумерация продолжается с 4
        assert_eq!(counters(&r.push(pkt(1, 4), start + WAIT)), vec![4]);
    }

    #[test]
    fn late_packet_after_a_skip_is_delivered_immediately() {
        let mut r = Resequencer::new(WAIT);
        let start = Instant::now();
        r.push(pkt(1, 0), start);
        r.push(pkt(1, 2), start);
        r.expire(start + WAIT);
        assert_eq!(counters(&r.push(pkt(1, 1), start + WAIT)), vec![1]);
        assert_eq!(r.stats().late, 1);
    }

    #[test]
    fn unnumbered_and_short_packets_pass_through() {
        let mut r = Resequencer::new(WAIT);
        let unnumbered = Incoming { slot: 0, payload: crate::pool::Packet::copy_from(&[0x45u8; 40]).unwrap(), wrapped: None, order: None };
        assert_eq!(r.push(unnumbered, Instant::now()).len(), 1);
        let short = Incoming { slot: 0, payload: crate::pool::Packet::copy_from(&[4, 0, 0, 0]).unwrap(), wrapped: None, order: None };
        assert_eq!(r.push(short, Instant::now()).len(), 1);
        assert_eq!(r.stats().passthrough, 2);
    }

    #[test]
    fn sessions_are_tracked_independently() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        r.push(pkt(1, 100), now);
        r.push(pkt(2, 0), now);
        assert!(r.push(pkt(1, 102), now).is_empty());
        // пакет другой сессии с малым счётчиком не считается опоздавшим и не ждёт
        assert_eq!(counters(&r.push(pkt(2, 1), now)), vec![1]);
        assert_eq!(counters(&r.push(pkt(1, 101), now)), vec![101, 102]);
    }

    #[test]
    fn a_huge_jump_flushes_and_resyncs() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        r.push(pkt(1, 0), now);
        r.push(pkt(1, 2), now);
        let out = r.push(pkt(1, 100_000), now);
        assert_eq!(counters(&out), vec![2, 100_000]);
        assert_eq!(counters(&r.push(pkt(1, 100_001), now)), vec![100_001]);
    }

    #[test]
    fn held_packets_are_bounded() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        r.push(pkt(1, 0), now);
        let mut released = 0;
        for counter in 2..(RING as u64 + 4) {
            released += r.push(pkt(1, counter), now).len();
        }
        assert!(released > 0, "переполнение должно выдать накопленное");
        assert!(r.stats().max_held <= RING);
    }

    #[test]
    fn random_shuffle_within_a_window_comes_out_sorted() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        let mut out = Vec::new();
        out.extend(r.push(pkt(1, 0), now));
        // перестановки внутри окна из 4 пакетов
        let order = [3u64, 1, 2, 4, 7, 5, 6, 8, 11, 9, 10];
        for counter in order {
            out.extend(r.push(pkt(1, counter), now));
        }
        assert_eq!(counters(&out), (0..=11).collect::<Vec<_>>());
        assert_eq!(r.stats().timed_out, 0);
    }

    fn feed_lateness(r: &mut Resequencer, start: Instant, lateness: Duration, rounds: u64) {
        // каждый раунд: 0 по порядку, 2 раньше времени, 1 спустя `lateness`
        let mut t = start;
        for round in 0..rounds {
            let base = round * 3;
            r.push(pkt(1, base), t);
            r.push(pkt(1, base + 2), t);
            t += lateness;
            r.push(pkt(1, base + 1), t);
        }
    }

    #[test]
    fn adaptive_wait_grows_with_lateness_but_stops_at_the_cap() {
        let mut r = Resequencer::adaptive(Duration::from_millis(8));
        feed_lateness(&mut r, Instant::now(), Duration::from_millis(16), 200);
        assert_eq!(r.wait(), Duration::from_millis(20), "p99 16 мс + 25%");
        let mut r = Resequencer::adaptive(Duration::from_millis(8));
        feed_lateness(&mut r, Instant::now(), Duration::from_millis(100), 200);
        assert_eq!(r.wait(), MAX_WAIT);
    }

    #[test]
    fn adaptive_wait_shrinks_on_a_calm_network_but_not_below_the_floor() {
        let mut r = Resequencer::adaptive(Duration::from_millis(20));
        feed_lateness(&mut r, Instant::now(), Duration::from_micros(200), 200);
        assert_eq!(r.wait(), MIN_WAIT);
        assert_eq!(r.stats().wait_ms, 3);
    }

    #[test]
    fn a_packet_that_missed_the_wait_still_teaches_the_buffer() {
        // Пакет 1 приходит через 40 мс: по таймауту (8 мс) его пропустили, но как
        // опоздавший он даёт отсчёт 40 мс, и ожидание растёт (до потолка).
        let mut r = Resequencer::adaptive(Duration::from_millis(8));
        let mut t = Instant::now();
        for round in 0..100u64 {
            let base = round * 3;
            r.push(pkt(1, base), t);
            r.push(pkt(1, base + 2), t);
            t += Duration::from_millis(40);
            r.expire(t);
            r.push(pkt(1, base + 1), t);
        }
        assert!(r.stats().late > 0);
        assert_eq!(r.wait(), MAX_WAIT);
    }

    #[test]
    fn fixed_wait_does_not_adapt() {
        let mut r = Resequencer::new(Duration::from_millis(8));
        feed_lateness(&mut r, Instant::now(), Duration::from_millis(16), 200);
        assert_eq!(r.wait(), Duration::from_millis(8));
    }

    fn ordered(flow: u32, seq: u64) -> Incoming {
        let mut payload = vec![0x45u8; 40]; // IPv4-подобный пакет
        payload[39] = seq as u8;
        Incoming { slot: 0, payload: crate::pool::Packet::copy_from(&payload).unwrap(), wrapped: None, order: Some((flow, seq)) }
    }

    fn seqs(packets: &[Incoming]) -> Vec<(u32, u64)> {
        packets.iter().map(|p| p.order.unwrap()).collect()
    }

    #[test]
    fn ordered_packets_are_resequenced_per_flow() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        assert_eq!(seqs(&r.push(ordered(1, 0), now)), vec![(1, 0)]);
        assert!(r.push(ordered(1, 2), now).is_empty(), "поток 1 ждёт номер 1");
        // Другой поток не ждёт пробела потока 1.
        assert_eq!(seqs(&r.push(ordered(2, 0), now)), vec![(2, 0)]);
        assert_eq!(seqs(&r.push(ordered(2, 1), now)), vec![(2, 1)]);
        assert_eq!(seqs(&r.push(ordered(1, 1), now)), vec![(1, 1), (1, 2)]);
    }

    fn client_ordered(client_id: u8, flow: u32, seq: u64) -> Incoming {
        let mut packet = ordered(flow, seq);
        packet.wrapped = Some(crate::multilink::WrappedInfo { client_id, seq });
        packet
    }

    #[test]
    fn the_same_flow_of_different_clients_is_resequenced_separately() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        assert_eq!(r.push(client_ordered(1, 5, 0), now).len(), 1);
        assert!(r.push(client_ordered(1, 5, 2), now).is_empty(), "клиент 1 ждёт номер 1");
        // У клиента 2 своя корзина 5 со своими номерами; у телефона без роутера — своя.
        assert_eq!(r.push(client_ordered(2, 5, 0), now).len(), 1);
        assert_eq!(r.push(ordered(5, 0), now).len(), 1);
        assert_eq!(r.push(client_ordered(1, 5, 1), now).len(), 2);
    }

    #[test]
    fn unnumbered_ip_packets_pass_immediately_even_while_a_flow_waits() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        r.push(ordered(3, 0), now);
        assert!(r.push(ordered(3, 2), now).is_empty());
        let udp = Incoming { slot: 0, payload: crate::pool::Packet::copy_from(&[0x45u8; 40]).unwrap(), wrapped: None, order: None };
        assert_eq!(r.push(udp, now).len(), 1, "UDP/ICMP без номера отдаются сразу");
        assert_eq!(r.stats().passthrough, 1);
    }
}
