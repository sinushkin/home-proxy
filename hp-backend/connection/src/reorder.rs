//! Восстановление порядка пакетов WireGuard на приёме.
//!
//! `send_data` раскладывает пакеты по десяти дырам, поэтому на приёме они приходят не по
//! порядку, и вложенный TCP принимает это за потери. У пакета передачи данных WireGuard
//! в открытом заголовке лежит номер сессии (`receiver index`) и 64-битный счётчик: по ним
//! возвращаем порядок, не меняя наш протокол. Недостающий пакет ждём не дольше `wait`,
//! потом отдаём то, что накопилось (опоздавший пакет передаём дальше, WireGuard принимает
//! пакеты вне очереди в пределах своего окна).
//!
//! Модуль чистый: время передаётся снаружи, таймеров и каналов здесь нет.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::multilink::Incoming;

/// Пакет передачи данных WireGuard: тип 4, три нулевых байта, `receiver index` (4),
/// счётчик (8), затем шифротекст с тегом (не меньше 16 байт).
const TRANSPORT_HEADER: usize = 16;
const MIN_TRANSPORT_LEN: usize = 32;
/// Дальше этого «прыжка» вперёд считаем, что сессия сбилась: буфер не копим.
const MAX_GAP: u64 = 4096;
/// Сколько пакетов держим на одну сессию.
const MAX_HELD: usize = 512;
const MAX_SESSIONS: usize = 16;

/// Пределы адаптивного ожидания.
pub const MIN_WAIT: Duration = Duration::from_millis(3);
pub const MAX_WAIT: Duration = Duration::from_millis(30);
/// Сколько последних отставаний помним и как часто пересчитываем ожидание.
const LATENESS_WINDOW: usize = 512;
const RECALC_EVERY: usize = 64;
/// Сколько пропущенных (выданных по таймауту) номеров помним, чтобы измерить опоздавшего.
const MAX_SKIPPED: usize = 1024;

/// `(receiver index, счётчик)` пакета передачи данных WireGuard; для остальных типов
/// (рукопожатие, cookie) `None`.
pub fn parse_transport(payload: &[u8]) -> Option<(u32, u64)> {
    if payload.len() < MIN_TRANSPORT_LEN || payload[0] != 4 || payload[1..4] != [0, 0, 0] {
        return None;
    }
    let index = u32::from_le_bytes(payload[4..8].try_into().ok()?);
    let counter = u64::from_le_bytes(payload[8..TRANSPORT_HEADER].try_into().ok()?);
    Some((index, counter))
}

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

struct Session {
    /// Следующий ожидаемый счётчик.
    next: u64,
    held: BTreeMap<u64, Held>,
    last_seen: Instant,
    /// Номера, выданные дальше без ожидания (не дождались), и когда был замечен их пробел.
    skipped: HashMap<u64, Instant>,
}

impl Session {
    fn oldest_arrival(&self) -> Option<Instant> {
        self.held.values().map(|h| h.arrived).min()
    }
}

pub struct Resequencer {
    wait: Duration,
    adaptive: bool,
    /// Последние измеренные отставания (кольцо).
    lateness: Vec<Duration>,
    lateness_pos: usize,
    since_recalc: usize,
    sessions: HashMap<u32, Session>,
    stats: ReorderStats,
}

impl Resequencer {
    /// Фиксированное ожидание `wait`.
    pub fn new(wait: Duration) -> Self {
        Self {
            wait,
            adaptive: false,
            lateness: Vec::new(),
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
        let mut sorted = self.lateness.clone();
        sorted.sort_unstable();
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

    /// Принимает пакет и возвращает те, что можно отдавать дальше сейчас (по порядку).
    pub fn push(&mut self, packet: Incoming, now: Instant) -> Vec<Incoming> {
        let Some((index, counter)) = parse_transport(&packet.payload) else {
            self.stats.passthrough += 1;
            return vec![packet];
        };
        if self.sessions.len() >= MAX_SESSIONS && !self.sessions.contains_key(&index) {
            self.prune();
        }
        let stats = &mut self.stats;
        let session = self
            .sessions
            .entry(index)
            .or_insert_with(|| Session { next: counter, held: BTreeMap::new(), last_seen: now, skipped: HashMap::new() });
        session.last_seen = now;

        if counter < session.next {
            stats.late += 1;
            let noticed = session.skipped.remove(&counter);
            log::trace!("порядок: сессия {index:#x}: опоздавший пакет {counter} (ждём {}), отдаём сразу", session.next);
            if let Some(noticed) = noticed {
                self.record_lateness(now - noticed);
            }
            return vec![packet];
        }
        if counter == session.next {
            stats.in_order += 1;
            let gap_noticed = session.oldest_arrival();
            let mut out = vec![packet];
            session.next = counter + 1;
            let held_before = session.held.len();
            drain(session, &mut out, &mut stats.reordered);
            if held_before > 0 {
                log::trace!(
                    "порядок: сессия {index:#x}: пришёл недостающий {counter}, выдано {} придержанных, осталось {}",
                    out.len() - 1,
                    session.held.len()
                );
            }
            if let Some(noticed) = gap_noticed {
                self.record_lateness(now - noticed);
            }
            return out;
        }
        if counter - session.next > MAX_GAP {
            // Сессия ушла далеко вперёд: пробел не ждём.
            let mut out = Vec::new();
            stats.forced += flush(session, &mut out);
            log::trace!("порядок: сессия {index:#x}: скачок {} -> {counter}, выдано {} придержанных", session.next, out.len());
            out.push(packet);
            session.next = counter + 1;
            return out;
        }
        log::trace!(
            "порядок: сессия {index:#x}: пакет {counter} раньше ожидаемого {} (пробел {}), придержано {}",
            session.next,
            counter - session.next,
            session.held.len() + 1
        );
        session.held.insert(counter, Held { packet, arrived: now });
        stats.max_held = stats.max_held.max(session.held.len());
        if session.held.len() > MAX_HELD {
            let mut out = Vec::new();
            release_lowest(session, &mut out);
            stats.forced += out.len() as u64;
            log::trace!("порядок: сессия {index:#x}: буфер переполнен, выдано {}", out.len());
            return out;
        }
        Vec::new()
    }

    /// Ближайший момент, когда `expire` что-то выдаст (для таймера).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.sessions.values().filter_map(Session::oldest_arrival).min().map(|t| t + self.wait)
    }

    /// Выдаёт придержанные пакеты, недостающего для которых мы не дождались.
    pub fn expire(&mut self, now: Instant) -> Vec<Incoming> {
        let mut out = Vec::new();
        for (index, session) in self.sessions.iter_mut() {
            while let Some(oldest) = session.oldest_arrival() {
                if now < oldest + self.wait {
                    break;
                }
                let before = out.len();
                let missing = session.next;
                if let Some(&lowest) = session.held.keys().next() {
                    if session.skipped.len() + (lowest - missing) as usize > MAX_SKIPPED {
                        session.skipped.clear();
                    }
                    for skipped in missing..lowest.min(missing + MAX_SKIPPED as u64) {
                        session.skipped.insert(skipped, oldest);
                    }
                }
                release_lowest(session, &mut out);
                self.stats.timed_out += (out.len() - before) as u64;
                log::trace!(
                    "порядок: сессия {index:#x}: не дождались {missing} за {} мс, выдано {}, дальше ждём {}",
                    self.wait.as_millis(),
                    out.len() - before,
                    session.next
                );
            }
        }
        out
    }

    fn prune(&mut self) {
        if let Some((&oldest, _)) = self.sessions.iter().min_by_key(|(_, s)| s.last_seen) {
            self.sessions.remove(&oldest);
        }
    }
}

/// Выдаёт подряд идущие придержанные пакеты, начиная с `session.next`.
fn drain(session: &mut Session, out: &mut Vec<Incoming>, counted: &mut u64) {
    while let Some(held) = session.held.remove(&session.next) {
        out.push(held.packet);
        session.next += 1;
        *counted += 1;
    }
}

/// Пропускает пробел: выдаёт самый младший придержанный пакет и всё, что идёт за ним подряд.
fn release_lowest(session: &mut Session, out: &mut Vec<Incoming>) {
    let Some(&lowest) = session.held.keys().next() else { return };
    session.next = lowest;
    while let Some(held) = session.held.remove(&session.next) {
        out.push(held.packet);
        session.next += 1;
    }
}

/// Выдаёт все придержанные пакеты по возрастанию счётчика.
fn flush(session: &mut Session, out: &mut Vec<Incoming>) -> u64 {
    let count = session.held.len() as u64;
    out.extend(std::mem::take(&mut session.held).into_values().map(|h| h.packet));
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_millis(8);

    fn wg(index: u32, counter: u64) -> Incoming {
        let mut payload = vec![0u8; 48];
        payload[0] = 4;
        payload[4..8].copy_from_slice(&index.to_le_bytes());
        payload[8..16].copy_from_slice(&counter.to_le_bytes());
        Incoming { slot: (counter % 10) as u8, payload, wrapped: None }
    }

    fn counters(packets: &[Incoming]) -> Vec<u64> {
        packets.iter().map(|p| parse_transport(&p.payload).unwrap().1).collect()
    }

    #[test]
    fn only_wireguard_transport_packets_are_parsed() {
        assert_eq!(parse_transport(&wg(7, 42).payload), Some((7, 42)));
        let mut handshake = wg(7, 42).payload;
        handshake[0] = 1;
        assert_eq!(parse_transport(&handshake), None);
        assert_eq!(parse_transport(&wg(7, 42).payload[..31]), None);
        let mut reserved = wg(7, 42).payload;
        reserved[2] = 1;
        assert_eq!(parse_transport(&reserved), None);
    }

    #[test]
    fn in_order_packets_pass_immediately() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        for counter in 5..10 {
            assert_eq!(counters(&r.push(wg(1, counter), now)), vec![counter]);
        }
        assert_eq!(r.stats().in_order, 5);
        assert_eq!(r.next_deadline(), None);
    }

    #[test]
    fn swapped_pair_is_released_in_order_without_waiting() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        r.push(wg(1, 0), now);
        assert!(r.push(wg(1, 2), now).is_empty());
        assert_eq!(counters(&r.push(wg(1, 1), now)), vec![1, 2]);
        assert_eq!(r.stats().reordered, 1);
        assert_eq!(r.next_deadline(), None);
    }

    #[test]
    fn missing_packet_is_skipped_after_the_wait() {
        let mut r = Resequencer::new(WAIT);
        let start = Instant::now();
        r.push(wg(1, 0), start);
        assert!(r.push(wg(1, 2), start).is_empty());
        assert!(r.push(wg(1, 3), start + Duration::from_millis(2)).is_empty());
        assert_eq!(r.next_deadline(), Some(start + WAIT));
        assert!(r.expire(start + Duration::from_millis(7)).is_empty());
        assert_eq!(counters(&r.expire(start + WAIT)), vec![2, 3]);
        assert_eq!(r.stats().timed_out, 2);
        // дальше нумерация продолжается с 4
        assert_eq!(counters(&r.push(wg(1, 4), start + WAIT)), vec![4]);
    }

    #[test]
    fn late_packet_after_a_skip_is_delivered_immediately() {
        let mut r = Resequencer::new(WAIT);
        let start = Instant::now();
        r.push(wg(1, 0), start);
        r.push(wg(1, 2), start);
        r.expire(start + WAIT);
        assert_eq!(counters(&r.push(wg(1, 1), start + WAIT)), vec![1]);
        assert_eq!(r.stats().late, 1);
    }

    #[test]
    fn handshake_and_short_packets_pass_through() {
        let mut r = Resequencer::new(WAIT);
        let mut handshake = wg(1, 0);
        handshake.payload[0] = 1;
        assert_eq!(r.push(handshake, Instant::now()).len(), 1);
        let short = Incoming { slot: 0, payload: vec![4, 0, 0, 0], wrapped: None };
        assert_eq!(r.push(short, Instant::now()).len(), 1);
        assert_eq!(r.stats().passthrough, 2);
    }

    #[test]
    fn sessions_are_tracked_independently() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        r.push(wg(1, 100), now);
        r.push(wg(2, 0), now);
        assert!(r.push(wg(1, 102), now).is_empty());
        // пакет другой сессии с малым счётчиком не считается опоздавшим и не ждёт
        assert_eq!(counters(&r.push(wg(2, 1), now)), vec![1]);
        assert_eq!(counters(&r.push(wg(1, 101), now)), vec![101, 102]);
    }

    #[test]
    fn a_huge_jump_flushes_and_resyncs() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        r.push(wg(1, 0), now);
        r.push(wg(1, 2), now);
        let out = r.push(wg(1, 100_000), now);
        assert_eq!(counters(&out), vec![2, 100_000]);
        assert_eq!(counters(&r.push(wg(1, 100_001), now)), vec![100_001]);
    }

    #[test]
    fn held_packets_are_bounded() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        r.push(wg(1, 0), now);
        let mut released = 0;
        for counter in 2..(MAX_HELD as u64 + 4) {
            released += r.push(wg(1, counter), now).len();
        }
        assert!(released > 0, "переполнение должно выдать накопленное");
        assert!(r.stats().max_held <= MAX_HELD + 1);
    }

    #[test]
    fn random_shuffle_within_a_window_comes_out_sorted() {
        let mut r = Resequencer::new(WAIT);
        let now = Instant::now();
        let mut out = Vec::new();
        out.extend(r.push(wg(1, 0), now));
        // перестановки внутри окна из 4 пакетов
        let order = [3u64, 1, 2, 4, 7, 5, 6, 8, 11, 9, 10];
        for counter in order {
            out.extend(r.push(wg(1, counter), now));
        }
        assert_eq!(counters(&out), (0..=11).collect::<Vec<_>>());
        assert_eq!(r.stats().timed_out, 0);
    }

    fn feed_lateness(r: &mut Resequencer, start: Instant, lateness: Duration, rounds: u64) {
        // каждый раунд: 0 по порядку, 2 раньше времени, 1 спустя `lateness`
        let mut t = start;
        for round in 0..rounds {
            let base = round * 3;
            r.push(wg(1, base), t);
            r.push(wg(1, base + 2), t);
            t += lateness;
            r.push(wg(1, base + 1), t);
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
            r.push(wg(1, base), t);
            r.push(wg(1, base + 2), t);
            t += Duration::from_millis(40);
            r.expire(t);
            r.push(wg(1, base + 1), t);
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
}
