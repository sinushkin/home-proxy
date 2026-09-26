//! Подлинность пакетов: имя пира, секрет пары, ключи и подпись.
//!
//! Полный GUID пира — секрет: его знают только два участника пары (обмениваются им заранее,
//! например QR-кодом). Наружу (MQTT: топики, `client_id`, `username`; заголовок `Init`) уходит
//! только **имя** — первая группа GUID (`kdjfkdjf` из `kdjfkdjf-…`). Из двух полных GUID обе
//! стороны получают одинаковый **секрет пары** (`PairSecret`), из него — всё остальное:
//!
//! - XOR-вектор маскировки каждой стороны — по её `session_id` (раньше вектор публиковался в
//!   `Rendezvous.key`, теперь не публикуется вовсе);
//! - ключ подписи на каждое направление дыры — по паре сессий (новая регистрация слота — новый
//!   ключ);
//! - ключ знакомства в VPS-режиме и подпись `Rendezvous` (MQTT, виртуал-брокер).
//!
//! Каждый пакет по дыре подписан: в начале `метка (16 байт) ‖ счётчик (8 байт, LE)`, дальше
//! protobuf. Метка — ChaCha20-Poly1305 с пустым открытым текстом; присоединённые данные — первые
//! `SIGNED_PREFIX` (128) байт protobuf (заголовок `Lite`/`Init` и заголовки вложенного IP-пакета),
//! nonce — счётчик и полная длина сообщения. Подпись впереди и под XOR-маской вместе с этими же
//! 128 байтами (`codec::MASKED_PREFIX`): узнаваемого заголовка нет. Шифрования нет (нагрузка — и
//! так HTTPS/TLS). Без секрета пары пакет не подделать, не вставить, не обрезать и не удлинить,
//! повтор отсекает окно по счётчику. Байты дальше 128-го не подписаны (на MIPS-роутере подпись
//! всего пакета стоила бы ~50 мкс вместо ~17): тот, кто на пути, может их испортить в настоящем
//! пакете — TLS это заметит и оборвёт соединение, открытый протокол — нет. Пакет без верной метки
//! отбрасывается целиком — и адрес пира по нему не меняется.

use std::sync::Mutex;

use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::codec::{XorKey, KEY_LEN};

/// Подпись в начале каждого пакета: метка и счётчик.
pub const AUTH_LEN: usize = TAG_LEN + COUNTER_LEN;
/// Сколько первых байт сообщения подписано (вместе с его полной длиной).
pub const SIGNED_PREFIX: usize = 128;
const COUNTER_LEN: usize = 8;
const TAG_LEN: usize = 16;
/// Длина подписи `Rendezvous` (усечённый HMAC-SHA256).
pub const RENDEZVOUS_MAC_LEN: usize = 16;

/// Ключ подписи (ChaCha20-Poly1305).
pub type AuthKey = [u8; 32];

/// Публичное имя пира: первая группа его GUID (8 hex-символов).
pub fn peer_name(id: &Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

/// Секрет пары: SHA-256 от двух полных GUID (порядок не важен — у обеих сторон одинаковый).
#[derive(Clone)]
pub struct PairSecret([u8; 32]);

impl std::fmt::Debug for PairSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairSecret(..)")
    }
}

impl PairSecret {
    pub fn new(a: Uuid, b: Uuid) -> Self {
        let (low, high) = if a.as_bytes() <= b.as_bytes() { (a, b) } else { (b, a) };
        let mut h = Sha256::new();
        h.update(b"home-proxy/pair/v1");
        h.update(low.as_bytes());
        h.update(high.as_bytes());
        Self(h.finalize().into())
    }

    /// Ключ для назначения `label` и уточнений `parts`.
    fn derive(&self, label: &[u8], parts: &[&[u8]]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"home-proxy/v1/");
        h.update(label);
        h.update([0]);
        h.update(self.0);
        for part in parts {
            h.update(part);
        }
        h.finalize().into()
    }

    /// XOR-вектор, которым маскируют пакеты **к** владельцу сессии `session`.
    pub fn xor_key(&self, session: Uuid) -> XorKey {
        let full = self.derive(b"xor", &[session.as_bytes()]);
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&full[..KEY_LEN]);
        key
    }

    /// Ключ подписи пакетов от сессии `from` к сессии `to`.
    pub fn link_key(&self, from: Uuid, to: Uuid) -> AuthKey {
        self.derive(b"link", &[from.as_bytes(), to.as_bytes()])
    }

    /// Порт знакомства VPS-режима: вектор и ключ подписи (одни на пару, в обе стороны).
    pub fn bootstrap(&self) -> (XorKey, AuthKey) {
        let full = self.derive(b"bootstrap-xor", &[]);
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&full[..KEY_LEN]);
        (key, self.derive(b"bootstrap-auth", &[]))
    }

    fn rendezvous_mac(&self) -> Hmac<Sha256> {
        <Hmac<Sha256> as Mac>::new_from_slice(&self.derive(b"rendezvous", &[])).expect("HMAC принимает ключ любой длины")
    }

    /// Подпись записи `Rendezvous` (байты записи с пустым полем подписи).
    pub fn sign_rendezvous(&self, bytes: &[u8]) -> [u8; RENDEZVOUS_MAC_LEN] {
        let mut mac = self.rendezvous_mac();
        mac.update(bytes);
        let full = mac.finalize().into_bytes();
        let mut out = [0u8; RENDEZVOUS_MAC_LEN];
        out.copy_from_slice(&full[..RENDEZVOUS_MAC_LEN]);
        out
    }

    /// Проверка подписи `Rendezvous` (за постоянное время).
    pub fn verify_rendezvous(&self, bytes: &[u8], signature: &[u8]) -> bool {
        let mut mac = self.rendezvous_mac();
        mac.update(bytes);
        signature.len() == RENDEZVOUS_MAC_LEN && mac.verify_truncated_left(signature).is_ok()
    }
}

/// Nonce: счётчик и полная длина сообщения — так подпись привязана и к длине.
fn nonce(counter: u64, len: usize) -> Nonce {
    let mut n = [0u8; 12];
    n[..COUNTER_LEN].copy_from_slice(&counter.to_le_bytes());
    n[COUNTER_LEN..].copy_from_slice(&(len as u32).to_le_bytes());
    Nonce::from(n)
}

/// Подписанная часть сообщения.
fn signed(message: &[u8]) -> &[u8] {
    &message[..message.len().min(SIGNED_PREFIX)]
}

/// Подписывает исходящие пакеты одного направления: свой счётчик на ключ (nonce не повторяется).
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    next: Mutex<u64>,
}

impl Sealer {
    /// Ключ новый (дыра: ключ от пары сессий) — счётчик с нуля.
    pub fn new(key: &AuthKey) -> Self {
        Self { cipher: ChaCha20Poly1305::new(key.into()), next: Mutex::new(0) }
    }

    /// Ключ постоянный (порт знакомства): счётчик со случайного места, чтобы после перезапуска
    /// не повторить nonce с тем же ключом.
    pub fn with_random_start(key: &AuthKey) -> Self {
        let start = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into().expect("8 байт")) >> 1;
        Self { cipher: ChaCha20Poly1305::new(key.into()), next: Mutex::new(start) }
    }

    /// Подписывает сообщение `buf[AUTH_LEN..AUTH_LEN + len]`, записывая подпись в `buf[..AUTH_LEN]`;
    /// длина пакета или `None`, если в `buf` не хватило места.
    pub fn seal(&self, buf: &mut [u8], len: usize) -> Option<usize> {
        let total = len.checked_add(AUTH_LEN)?;
        if buf.len() < total {
            return None;
        }
        let counter = {
            let mut next = self.next.lock().unwrap();
            let c = *next;
            *next = c.checked_add(1)?;
            c
        };
        let (header, message) = buf[..total].split_at_mut(AUTH_LEN);
        let tag = self.cipher.encrypt_in_place_detached(&nonce(counter, len), signed(message), &mut []).ok()?;
        header[..TAG_LEN].copy_from_slice(&tag);
        header[TAG_LEN..].copy_from_slice(&counter.to_le_bytes());
        Some(total)
    }

    /// То же для сообщения в `Vec` (не на горячем пути): подпись встаёт перед ним.
    pub fn seal_vec(&self, message: Vec<u8>) -> Vec<u8> {
        let len = message.len();
        let mut packet = vec![0u8; AUTH_LEN];
        packet.extend_from_slice(&message);
        self.seal(&mut packet, len).expect("место под подпись есть");
        packet
    }
}

/// Всё для отправки в одну сторону: XOR-вектор получателя и подпись.
pub struct SendKeys {
    pub xor: XorKey,
    pub sealer: Sealer,
}

/// Всё для приёма с одной стороны: свой XOR-вектор и проверка подписи.
pub struct RecvKeys {
    pub xor: XorKey,
    pub opener: Opener,
}

impl PairSecret {
    /// Ключи дыры для сессии `me` ↔ сессия пира `peer`: (отправка, приём).
    pub fn link_keys(&self, me: Uuid, peer: Uuid) -> (SendKeys, RecvKeys) {
        (
            SendKeys { xor: self.xor_key(peer), sealer: Sealer::new(&self.link_key(me, peer)) },
            RecvKeys { xor: self.xor_key(me), opener: Opener::new(&self.link_key(peer, me)) },
        )
    }

    /// Ключи порта знакомства (VPS-режим): одни на пару, счётчик со случайного места, без окна.
    pub fn bootstrap_keys(&self) -> (SendKeys, RecvKeys) {
        let (xor, auth) = self.bootstrap();
        (
            SendKeys { xor, sealer: Sealer::with_random_start(&auth) },
            RecvKeys { xor, opener: Opener::without_replay_window(&auth) },
        )
    }
}

/// Скользящее окно из 64 последних счётчиков: повтор и слишком старый пакет — отказ, перестановка
/// в пределах окна (пакеты идут по разным дырам) — нормально.
#[derive(Default)]
struct ReplayWindow {
    /// Наибольший принятый счётчик + 1 (0 — ещё ничего не принято).
    top: u64,
    /// Бит `i` — принят счётчик `top - 1 - i`.
    seen: u64,
}

impl ReplayWindow {
    fn fresh(&self, counter: u64) -> bool {
        if counter >= self.top {
            return true;
        }
        let back = self.top - 1 - counter;
        back < 64 && self.seen & (1 << back) == 0
    }

    fn accept(&mut self, counter: u64) {
        if counter >= self.top {
            let shift = counter + 1 - self.top;
            self.seen = if shift >= 64 { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.top = counter + 1;
        } else {
            self.seen |= 1 << (self.top - 1 - counter);
        }
    }
}

/// Проверяет подпись входящих пакетов одного направления.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    window: Option<ReplayWindow>,
}

impl Opener {
    /// С окном против повтора (дыра).
    pub fn new(key: &AuthKey) -> Self {
        Self { cipher: ChaCha20Poly1305::new(key.into()), window: Some(ReplayWindow::default()) }
    }

    /// Без окна (порт знакомства: у отправителя счётчик со случайного места после перезапуска;
    /// повтор старой записи безвреден — она только снова анонсирует слот).
    pub fn without_replay_window(key: &AuthKey) -> Self {
        Self { cipher: ChaCha20Poly1305::new(key.into()), window: None }
    }

    /// Проверяет пакет `buf` (метка ‖ счётчик ‖ сообщение); сообщение — `buf[AUTH_LEN..]`, при
    /// верной подписи возвращается его длина, иначе `None`.
    pub fn open(&mut self, buf: &[u8]) -> Option<usize> {
        if buf.len() < AUTH_LEN {
            return None;
        }
        let (header, message) = buf.split_at(AUTH_LEN);
        let counter = u64::from_le_bytes(header[TAG_LEN..].try_into().ok()?);
        if let Some(window) = &self.window
            && !window.fresh(counter)
        {
            return None;
        }
        let tag = Tag::from_slice(&header[..TAG_LEN]);
        self.cipher.decrypt_in_place_detached(&nonce(counter, message.len()), signed(message), &mut [], tag).ok()?;
        if let Some(window) = &mut self.window {
            window.accept(counter);
        }
        Some(message.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_the_first_group_of_the_guid() {
        let id: Uuid = "1a2b3c4d-0000-4000-8000-000000000000".parse().unwrap();
        assert_eq!(peer_name(&id), "1a2b3c4d");
    }

    #[test]
    fn the_pair_secret_is_symmetric_and_pair_specific() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(PairSecret::new(a, b).0, PairSecret::new(b, a).0);
        assert_ne!(PairSecret::new(a, b).0, PairSecret::new(a, c).0);
        let pair = PairSecret::new(a, b);
        let (s1, s2) = (Uuid::new_v4(), Uuid::new_v4());
        assert_ne!(pair.link_key(s1, s2), pair.link_key(s2, s1), "у направлений свои ключи");
        assert_ne!(pair.xor_key(s1), pair.xor_key(s2));
    }

    fn sealed(sealer: &Sealer, message: &[u8]) -> Vec<u8> {
        sealer.seal_vec(message.to_vec())
    }

    #[test]
    fn sealed_packets_open_and_tampering_is_detected() {
        let key = PairSecret::new(Uuid::new_v4(), Uuid::new_v4()).link_key(Uuid::new_v4(), Uuid::new_v4());
        let (sealer, mut opener) = (Sealer::new(&key), Opener::new(&key));
        let packet = sealed(&sealer, b"ip packet");
        assert_eq!(packet.len(), AUTH_LEN + 9);
        assert_eq!(&packet[AUTH_LEN..], b"ip packet", "подпись впереди, сообщение как есть");
        assert_eq!(opener.open(&packet), Some(9));

        for i in 0..packet.len() {
            let mut bad = sealed(&sealer, b"ip packet");
            bad[i] ^= 1;
            assert_eq!(opener.open(&bad), None, "испорченный байт {i}");
        }
        let other = Sealer::new(&[7; 32]);
        assert_eq!(opener.open(&sealed(&other, b"ip packet")), None, "чужой ключ");
        assert_eq!(opener.open(&[0u8; 10]), None, "короче подписи");
    }

    #[test]
    fn the_signed_prefix_and_the_length_are_covered_the_tail_is_not() {
        let key = [8u8; 32];
        let (sealer, mut opener) = (Sealer::new(&key), Opener::new(&key));
        let message: Vec<u8> = (0..1400).map(|i| i as u8).collect();
        for i in [0, 23, AUTH_LEN, AUTH_LEN + SIGNED_PREFIX - 1] {
            let mut bad = sealed(&sealer, &message);
            bad[i] ^= 1;
            assert_eq!(opener.open(&bad), None, "байт {i} подписан");
        }
        let mut shorter = sealed(&sealer, &message);
        shorter.pop();
        assert_eq!(opener.open(&shorter), None, "длина подписана: обрезать нельзя");
        let mut longer = sealed(&sealer, &message);
        longer.push(0);
        assert_eq!(opener.open(&longer), None, "и удлинить тоже");
        let mut tail = sealed(&sealer, &message);
        tail[AUTH_LEN + SIGNED_PREFIX] ^= 1;
        assert_eq!(opener.open(&tail), Some(1400), "хвост дальше 128 байт не подписан");
    }

    #[test]
    fn replays_are_rejected_but_reordering_within_the_window_is_fine() {
        let key = [3u8; 32];
        let (sealer, mut opener) = (Sealer::new(&key), Opener::new(&key));
        let packets: Vec<Vec<u8>> = (0..100u8).map(|i| sealed(&sealer, &[i])).collect();
        assert!(opener.open(&packets[5]).is_some());
        assert!(opener.open(&packets[5]).is_none(), "повтор");
        assert!(opener.open(&packets[2]).is_some(), "переставлен, но в окне");
        assert!(opener.open(&packets[99]).is_some());
        assert!(opener.open(&packets[20]).is_none(), "старше окна (64)");
        assert!(opener.open(&packets[40]).is_some());
        assert!(opener.open(&packets[40]).is_none());

        let mut no_window = Opener::without_replay_window(&key);
        assert!(no_window.open(&packets[1]).is_some());
        assert!(no_window.open(&packets[1]).is_some(), "порт знакомства: без окна");
    }

    #[test]
    fn random_start_sealers_do_not_start_at_zero() {
        let key = [9u8; 32];
        let a = sealed(&Sealer::with_random_start(&key), b"x");
        let b = sealed(&Sealer::with_random_start(&key), b"x");
        assert_ne!(a, b, "разные счётчики — разные nonce");
        assert!(Opener::without_replay_window(&key).open(&a).is_some());
    }

    #[test]
    fn rendezvous_signature_checks_bytes_and_pair() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let pair = PairSecret::new(a, b);
        let signature = pair.sign_rendezvous(b"record");
        assert!(PairSecret::new(b, a).verify_rendezvous(b"record", &signature));
        assert!(!pair.verify_rendezvous(b"recorD", &signature));
        assert!(!PairSecret::new(a, Uuid::new_v4()).verify_rendezvous(b"record", &signature));
        assert!(!pair.verify_rendezvous(b"record", &signature[..8]));
    }
}
