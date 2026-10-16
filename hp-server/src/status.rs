//! Снимок набора дыр (`connection::multilink::LinkStatus`) → сообщения протокола управления. Общее
//! для `hp-server`, `vps-server` и `hp-router`: всё из памяти службы, без журнала и сторонних утилит.

use connection::holes::HoleState;
use connection::multilink::{HoleCounters, HoleEvent, HoleStatus, LinkStatus};
use connection::vps::relay::RelayInfo;
use hp_control::proto;

pub fn hole(h: &HoleStatus) -> proto::HoleStatus {
    proto::HoleStatus {
        slot: h.slot,
        peer_addr: h.peer_addr.map(|a| a.to_string()).unwrap_or_default(),
        sent: h.sent,
        received: h.received,
        loss_out: h.loss_out.unwrap_or(-1.0),
        loss_in: h.loss_in.unwrap_or(-1.0),
        age_secs: h.age.as_secs().min(u64::from(u32::MAX)) as u32,
        draining: h.state == HoleState::Draining,
        via: h.via.as_deref().unwrap_or_default().to_string(),
    }
}

pub fn holes(status: &LinkStatus) -> Vec<proto::HoleStatus> {
    status.holes.iter().map(hole).collect()
}

pub fn counters(c: &HoleCounters) -> proto::HoleCounters {
    proto::HoleCounters {
        opened: c.opened,
        lost: c.lost,
        drained_by_us: c.drained_by_us,
        drained_by_peer: c.drained_by_peer,
        bad: c.bad,
        not_confirmed: c.not_confirmed,
    }
}

pub fn events(events: &[HoleEvent]) -> Vec<proto::HoleEvent> {
    events
        .iter()
        .map(|e| proto::HoleEvent {
            at_unix_ms: e.at_unix_ms,
            slot: e.slot,
            kind: e.kind.as_str().into(),
            via: e.via.as_deref().unwrap_or_default().to_string(),
        })
        .collect()
}

/// Релеи VPS-сервера.
pub fn relays(relays: Vec<RelayInfo>) -> Vec<proto::RelayStatus> {
    relays
        .into_iter()
        .map(|r| proto::RelayStatus {
            name: r.name,
            addr: r.public.map(|a| a.to_string()).unwrap_or_default(),
            alive: r.alive,
            live: r.link.as_ref().map_or(0, |l| l.in_work() as u32),
            holes: r.link.as_ref().map(holes).unwrap_or_default(),
            counters: r.link.as_ref().map(|l| counters(&l.counters)),
        })
        .collect()
}
