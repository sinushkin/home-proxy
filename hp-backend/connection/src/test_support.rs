//! Общее для интеграционных тестов режимов (`vps`, `p2p`): ожидание, быстрая политика набора,
//! номера дыр в статусе.

use std::time::Duration;

use crate::holes::{HoleState, PoolPolicy};
use crate::multilink::{MultiLink, MultiLinkOptions, SlotId};

pub(crate) async fn wait_until(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(tokio::time::Instant::now() < deadline, "не дождались: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Быстрая политика набора для тестов: дыры добавляются каждые полсекунды, живут `age`.
pub(crate) fn quick_options(min: usize, max: usize, age: (Duration, Duration)) -> MultiLinkOptions {
    MultiLinkOptions {
        pool: PoolPolicy { min_active: min, max_total: max, add_interval: Duration::from_millis(500), ..PoolPolicy::default() },
        hole_age: age,
        ..MultiLinkOptions::default()
    }
}

#[allow(dead_code)] // нужна тестам VPS-режима
pub(crate) const LONG_AGE: (Duration, Duration) = (Duration::from_secs(600), Duration::from_secs(600));

/// Номера дыр в работе (не сливаемых).
pub(crate) fn active_ids(link: &MultiLink) -> Vec<SlotId> {
    link.status().holes.iter().filter(|h| h.state == HoleState::Active).map(|h| h.slot).collect()
}

pub(crate) fn all_ids(link: &MultiLink) -> Vec<SlotId> {
    link.status().holes.iter().map(|h| h.slot).collect()
}
