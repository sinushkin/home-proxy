//! Банк буферов под пакеты: MTU известен (пакет в дыре ≤ 1500 байт), поэтому вместо
//! выделения памяти на каждый пакет берём буфер фиксированного размера из банка и возвращаем
//! его, когда пакет больше не нужен (`Packet` при дропе). На слабом роутере это главный
//! резерв: аллокатор musl отдаёт освобождённое ядру (`mmap`/`munmap` на каждый пакет).
//!
//! Банк один на процесс, держит до `POOL_SIZE` свободных буферов; если свободных нет, буфер
//! выделяется (и потом тоже возвращается, пока банк не полон). 64-битных атомиков нет.

use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Размер одного буфера: больше любого пакета в дыре.
pub const PACKET_CAP: usize = 1500;
/// Сколько свободных буферов банк держит у себя.
pub const POOL_SIZE: usize = 100;

type Buf = Box<[u8; PACKET_CAP]>;

/// Банк: свободные буферы и счётчик выделенных за всё время.
pub struct Bank {
    free: Mutex<Vec<Buf>>,
    allocated: AtomicU32,
}

impl Bank {
    pub const fn new() -> Self {
        Self { free: Mutex::new(Vec::new()), allocated: AtomicU32::new(0) }
    }

    fn take(&self) -> Buf {
        if let Some(buf) = self.free.lock().unwrap().pop() {
            return buf;
        }
        self.allocated.fetch_add(1, Ordering::Relaxed);
        Box::new([0u8; PACKET_CAP])
    }

    fn give_back(&self, buf: Buf) {
        let mut free = self.free.lock().unwrap();
        if free.len() < POOL_SIZE {
            if free.capacity() == 0 {
                free.reserve_exact(POOL_SIZE);
            }
            free.push(buf);
        }
    }

    /// Сколько буферов выделено за всё время и сколько сейчас свободно.
    pub fn stats(&self) -> (u32, usize) {
        (self.allocated.load(Ordering::Relaxed), self.free.lock().unwrap().len())
    }
}

impl Default for Bank {
    fn default() -> Self {
        Self::new()
    }
}

/// Банк процесса.
pub static BANK: Bank = Bank::new();

/// Статистика банка процесса.
pub fn stats() -> (u32, usize) {
    BANK.stats()
}

/// Пакет в буфере из банка. При дропе буфер возвращается в банк.
pub struct Packet {
    buf: Option<Buf>,
    len: usize,
    bank: &'static Bank,
}

impl Packet {
    /// Копия `data` в буфер из банка процесса; `None`, если данные больше буфера.
    pub fn copy_from(data: &[u8]) -> Option<Self> {
        Self::copy_from_bank(&BANK, data)
    }

    /// То же из заданного банка.
    pub fn copy_from_bank(bank: &'static Bank, data: &[u8]) -> Option<Self> {
        if data.len() > PACKET_CAP {
            return None;
        }
        let mut buf = bank.take();
        buf[..data.len()].copy_from_slice(data);
        Some(Self { buf: Some(buf), len: data.len(), bank })
    }
}

impl Deref for Packet {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf.as_ref().expect("буфер есть до дропа")[..self.len]
    }
}

impl std::ops::DerefMut for Packet {
    fn deref_mut(&mut self) -> &mut [u8] {
        let len = self.len;
        &mut self.buf.as_mut().expect("буфер есть до дропа")[..len]
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            self.bank.give_back(buf);
        }
    }
}

impl fmt::Debug for Packet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Packet({} байт)", self.len)
    }
}

impl PartialEq for Packet {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl PartialEq<[u8]> for Packet {
    fn eq(&self, other: &[u8]) -> bool {
        **self == *other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for Packet {
    fn eq(&self, other: &&[u8; N]) -> bool {
        **self == other[..]
    }
}

impl PartialEq<Vec<u8>> for Packet {
    fn eq(&self, other: &Vec<u8>) -> bool {
        **self == other[..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Банк общий на процесс, а тесты идут параллельно: проверяем только то, что не зависит от
    // чужих тестов.
    #[test]
    fn packet_holds_a_copy_and_compares_with_bytes() {
        let packet = Packet::copy_from(b"hello").unwrap();
        assert_eq!(&*packet, b"hello");
        assert_eq!(packet, b"hello");
        assert_eq!(packet, b"hello".to_vec());
        assert!(Packet::copy_from(&[0u8; PACKET_CAP + 1]).is_none());
        assert_eq!(Packet::copy_from(&[7u8; PACKET_CAP]).unwrap().len(), PACKET_CAP);
    }

    #[test]
    fn buffers_are_reused_instead_of_allocated() {
        static TEST_BANK: Bank = Bank::new();
        // 10 000 пакетов по одному: выделяется один буфер, дальше он же.
        for _ in 0..10_000 {
            let packet = Packet::copy_from_bank(&TEST_BANK, &[2u8; 1400]).unwrap();
            assert_eq!(packet[0], 2);
        }
        assert_eq!(TEST_BANK.stats(), (1, 1));
        // Много пакетов сразу: банк держит не больше POOL_SIZE свободных.
        let many: Vec<Packet> = (0..POOL_SIZE + 20).map(|_| Packet::copy_from_bank(&TEST_BANK, b"x").unwrap()).collect();
        drop(many);
        let (allocated, free) = TEST_BANK.stats();
        assert_eq!(allocated, (POOL_SIZE + 20) as u32);
        assert_eq!(free, POOL_SIZE);
    }
}
