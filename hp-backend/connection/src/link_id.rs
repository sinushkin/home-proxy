//! `PeerLinkId` — стабильный идентификатор одной пробитой дыры.
//!
//! Дыра — это пара сессий: наша (сокет с этой стороны) и пира. У обеих сторон
//! на руках оба GUID сессий, поэтому идентификатор считается симметрично
//! (порядок аргументов не важен) и получается одинаковым на обоих концах. Это
//! нужно, чтобы `Map<PeerLinkId, u8>` присваивала одной и той же дыре один и
//! тот же номер слота по обе стороны.

use std::fmt;

use uuid::Uuid;

/// 32 байта: два 16-байтных GUID сессий в каноническом (отсортированном)
/// порядке. Одинаков на обеих сторонах линка.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerLinkId([u8; 32]);

impl PeerLinkId {
    /// Свёртка двух сессий в идентификатор. `new(a, b) == new(b, a)`.
    pub fn new(session_a: Uuid, session_b: Uuid) -> Self {
        let (lo, hi) = if session_a <= session_b {
            (session_a, session_b)
        } else {
            (session_b, session_a)
        };
        let mut bytes = [0u8; 32];
        bytes[..16].copy_from_slice(lo.as_bytes());
        bytes[16..].copy_from_slice(hi.as_bytes());
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for PeerLinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Две половины как обычные UUID — читаемо в логах.
        let lo = Uuid::from_slice(&self.0[..16]).unwrap();
        let hi = Uuid::from_slice(&self.0[16..]).unwrap();
        write!(f, "PeerLinkId({lo}+{hi})")
    }
}

impl fmt::Display for PeerLinkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_in_arguments() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(PeerLinkId::new(a, b), PeerLinkId::new(b, a));
    }

    #[test]
    fn distinct_pairs_differ() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        assert_ne!(PeerLinkId::new(a, b), PeerLinkId::new(a, c));
    }

    #[test]
    fn stable_bytes_layout() {
        // Меньший GUID идёт первым независимо от порядка аргументов.
        let lo = Uuid::from_u128(1);
        let hi = Uuid::from_u128(2);
        let id = PeerLinkId::new(hi, lo);
        assert_eq!(&id.as_bytes()[..16], lo.as_bytes());
        assert_eq!(&id.as_bytes()[16..], hi.as_bytes());
    }
}
