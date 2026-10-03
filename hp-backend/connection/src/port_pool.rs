//! Реестр занятых портов диапазона слотов VPS-сервера: один на процесс, общий для всех клиентов.
//!
//! Занятость — битовая карта (`u32`-слова, без 64-битных атомиков). Блокировка `RwLock` берётся
//! только при занятии и освобождении порта, то есть при открытии и закрытии дыры; на пути пакетов
//! её нет. Порт берётся из свободных по карте, а не наугад с повтором `bind`, поэтому коллизий с
//! собственными дырами не бывает; `bind` всё равно нужен, потому что порт может быть занят чужой
//! службой.

use std::ops::RangeInclusive;
use std::sync::{Arc, RwLock};

use uuid::Uuid;

pub struct PortPool {
    low: u16,
    span: u32,
    busy: RwLock<Vec<u32>>,
}

/// Занятый порт: освобождается при дропе.
pub struct PortLease {
    pool: Arc<PortPool>,
    port: u16,
}

impl PortPool {
    pub fn new(range: RangeInclusive<u16>) -> Arc<Self> {
        let (low, high) = (*range.start(), *range.end());
        let span = u32::from(high.saturating_sub(low)) + 1;
        Arc::new(Self { low, span, busy: RwLock::new(vec![0; span.div_ceil(32) as usize]) })
    }

    /// Занимает свободный порт; `None`, если диапазон весь занят. Начинает поиск со случайного
    /// места, чтобы клиенты не выстраивались в один ряд портов.
    pub fn lease(self: &Arc<Self>) -> Option<PortLease> {
        let start = u32::from_le_bytes(Uuid::new_v4().into_bytes()[..4].try_into().expect("4 байта")) % self.span;
        let mut busy = self.busy.write().unwrap();
        (0..self.span).map(|i| (start + i) % self.span).find_map(|index| {
            let (word, bit) = ((index / 32) as usize, index % 32);
            if busy[word] & (1 << bit) != 0 {
                return None;
            }
            busy[word] |= 1 << bit;
            Some(PortLease { pool: self.clone(), port: self.low + index as u16 })
        })
    }

    /// Сколько портов занято сейчас.
    pub fn in_use(&self) -> usize {
        self.busy.read().unwrap().iter().map(|w| w.count_ones() as usize).sum()
    }

    fn release(&self, port: u16) {
        let index = u32::from(port - self.low);
        self.busy.write().unwrap()[(index / 32) as usize] &= !(1 << (index % 32));
    }
}

impl PortLease {
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for PortLease {
    fn drop(&mut self) {
        self.pool.release(self.port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases_are_distinct_and_released_on_drop() {
        let pool = PortPool::new(45000..=45003);
        let held: Vec<_> = (0..4).map(|_| pool.lease().expect("свободный порт")).collect();
        let mut ports: Vec<u16> = held.iter().map(PortLease::port).collect();
        ports.sort_unstable();
        assert_eq!(ports, vec![45000, 45001, 45002, 45003], "все порты разные и в диапазоне");
        assert!(pool.lease().is_none(), "диапазон исчерпан");
        assert_eq!(pool.in_use(), 4);

        drop(held);
        assert_eq!(pool.in_use(), 0);
        assert!(pool.lease().is_some(), "после освобождения порт снова свободен");
    }

    #[test]
    fn range_that_is_not_a_multiple_of_32_is_fully_usable() {
        let pool = PortPool::new(1000..=1039); // 40 портов: два слова, второе — частично
        let held: Vec<_> = (0..40).map(|_| pool.lease().expect("свободный порт")).collect();
        assert!(pool.lease().is_none());
        let mut ports: Vec<u16> = held.iter().map(PortLease::port).collect();
        ports.sort_unstable();
        assert_eq!(ports.first(), Some(&1000));
        assert_eq!(ports.last(), Some(&1039));
    }
}
