//! Записи знакомства (`Rendezvous`): подписанная секретом пары запись о дыре — внешние адреса
//! сокета, сессия, номер дыры — и её разбор с проверкой имени и подписи. Общее для обоих режимов:
//! в P2P запись уходит через MQTT (`p2p::mqtt`) и по живым дырам (виртуал-брокер), в VPS — на порт
//! знакомства сервера (`vps`). Имя пира — первая группа GUID (`auth::peer_name`): полный GUID —
//! секрет пары, наружу он не попадает. Запись подписана ключом из секрета пары: чужую (подложенную
//! под нашим именем) пир не примет.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use prost::Message as _;
use uuid::Uuid;

use crate::auth::{peer_name, PairSecret};
use crate::proto::{Endpoint, Rendezvous};

/// Запись одного слота пира, полученная с брокера.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerSession {
    pub slot: crate::multilink::SlotId,
    pub session_id: Uuid,
    pub addr: SocketAddr,
    /// Другие внешние адреса того же сокета (их видели другие STUN-серверы).
    pub extra: Vec<SocketAddr>,
    /// Когда пир зарегистрировал запись (мс Unix; `0` — не указано). В VPS-режиме клиент указывает
    /// здесь время запуска процесса: по росту значения сервер узнаёт о перезапуске клиента.
    pub registered_at_unix_ms: u64,
}

impl PeerSession {
    /// Все адреса, по которым стучимся к пиру: основной, затем дополнительные.
    pub fn candidates(&self) -> Vec<SocketAddr> {
        let mut all = vec![self.addr];
        all.extend(self.extra.iter().copied().filter(|a| *a != self.addr));
        all
    }
}

/// Последняя регистрация пира на брокере: откуда (адрес, который пир увидел через STUN) и
/// когда (`registered_at_unix_ms` записи, по часам пира). Обновляется на каждой публикации пира,
/// в том числе повторной той же сессии, — видно, жив ли пир на брокере, даже если дыры не
/// пробиваются.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerRegistration {
    pub addr: SocketAddr,
    pub registered_at_unix_ms: u64,
}

/// Подписанная запись `Rendezvous` о нашем слоте. `endpoints` — внешние адреса сокета, какими
/// его увидели STUN-серверы: первый основной, остальные уходят как дополнительные.
pub fn our_record(
    pair: &PairSecret,
    my_peer_id: Uuid,
    slot: crate::multilink::SlotId,
    session_id: Uuid,
    endpoints: &[SocketAddr],
    registered_at_unix_ms: u64,
) -> Rendezvous {
    let primary = endpoints.first().copied().unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0)));
    let mut record = Rendezvous {
        public_ip: primary.ip().to_string(),
        public_port: u32::from(primary.port()),
        peer_id: peer_name(&my_peer_id),
        registered_at_unix_ms,
        session_id: session_id.to_string(),
        slot,
        extra_endpoints: endpoints
            .iter()
            .skip(1)
            .filter(|a| **a != primary)
            .map(|a| Endpoint { ip: a.ip().to_string(), port: u32::from(a.port()) })
            .collect(),
        signature: Vec::new(),
    };
    record.signature = pair.sign_rendezvous(&record.encode_to_vec()).to_vec();
    record
}

/// Подпись записи верна (считается по байтам записи с пустой подписью).
fn signature_is_valid(r: &Rendezvous, pair: &PairSecret) -> bool {
    let mut unsigned = r.clone();
    let signature = std::mem::take(&mut unsigned.signature);
    pair.verify_rendezvous(&unsigned.encode_to_vec(), &signature)
}

/// Преобразует запись `Rendezvous` пира `peer_id` в `PeerSession`, проверив имя и подпись.
/// Используется и для записей из MQTT, и для тех, что пришли напрямую по дыре (виртуал-брокер),
/// и на порту знакомства VPS-режима.
pub fn peer_session_from(r: &Rendezvous, pair: &PairSecret, peer_id: Uuid) -> Result<PeerSession> {
    anyhow::ensure!(r.peer_id == peer_name(&peer_id), "запись другого пира ({})", r.peer_id);
    anyhow::ensure!(signature_is_valid(r, pair), "подпись записи неверна");
    Ok(PeerSession {
        slot: r.slot,
        session_id: r.session_id.parse().context("некорректный session_id")?,
        addr: SocketAddr::new(
            r.public_ip.parse().context("некорректный public_ip")?,
            r.public_port as u16,
        ),
        extra: r
            .extra_endpoints
            .iter()
            .filter_map(|e| {
                let port = u16::try_from(e.port).ok().filter(|p| *p != 0)?;
                Some(SocketAddr::new(e.ip.parse().ok()?, port))
            })
            .collect(),
        registered_at_unix_ms: r.registered_at_unix_ms,
    })
}

/// Разбирает payload записи пира (из MQTT или другого транспорта). `Ok(None)` — запись не про этого
/// пира (лишний топик), её просто пропускаем. Вместе с сессией — время регистрации из записи.
#[cfg(any(feature = "p2p", test))]
pub(crate) fn decode_peer_session(payload: &[u8], peer_id: Uuid, pair: &PairSecret) -> Result<Option<(PeerSession, u64)>> {
    let r = Rendezvous::decode(payload).context("не удалось разобрать Rendezvous")?;
    if r.peer_id != peer_name(&peer_id) {
        return Ok(None);
    }
    Ok(Some((peer_session_from(&r, pair, peer_id)?, r.registered_at_unix_ms)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (Uuid, Uuid, PairSecret) {
        let (me, peer) = (Uuid::new_v4(), Uuid::new_v4());
        (me, peer, PairSecret::new(me, peer))
    }

    #[test]
    fn signed_record_decodes_for_the_pair_and_carries_only_the_name() {
        let (me, peer, pair) = ids();
        let session_id = Uuid::new_v4();
        let record = our_record(&pair, peer, 3, session_id, &["203.0.113.7:40000".parse().unwrap()], 1_700_000_000_000);
        assert_eq!(record.peer_id, peer_name(&peer));
        assert_eq!(record.peer_id.len(), 8);

        let got = decode_peer_session(&record.encode_to_vec(), peer, &PairSecret::new(peer, me)).unwrap().unwrap();
        assert_eq!(
            got,
            (PeerSession { slot: 3, session_id, addr: "203.0.113.7:40000".parse().unwrap(), extra: vec![], registered_at_unix_ms: 1_700_000_000_000 }, 1_700_000_000_000)
        );
    }

    #[test]
    fn forged_or_altered_records_are_rejected() {
        let (_, peer, pair) = ids();
        let record = our_record(&pair, peer, 0, Uuid::new_v4(), &["203.0.113.7:40000".parse().unwrap()], 0);

        let mut moved = record.clone();
        moved.public_port = 40001;
        assert!(peer_session_from(&moved, &pair, peer).is_err(), "адрес подменён");

        let stranger = PairSecret::new(peer, Uuid::new_v4());
        let forged = our_record(&stranger, peer, 0, Uuid::new_v4(), &["198.51.100.1:1".parse().unwrap()], 0);
        assert!(peer_session_from(&forged, &pair, peer).is_err(), "подписано не секретом пары");

        let mut unsigned = record;
        unsigned.signature.clear();
        assert!(peer_session_from(&unsigned, &pair, peer).is_err(), "без подписи");
    }

    #[test]
    fn extra_endpoints_round_trip_and_become_candidates() {
        let (_, peer, pair) = ids();
        let endpoints: Vec<SocketAddr> = vec![
            "203.0.113.7:40000".parse().unwrap(),
            "198.51.100.2:41000".parse().unwrap(),
            "203.0.113.7:40000".parse().unwrap(), // повтор основного отбрасывается
        ];
        let record = our_record(&pair, peer, 3, Uuid::new_v4(), &endpoints, 0);
        assert_eq!(record.extra_endpoints.len(), 1);

        let session = peer_session_from(&record, &pair, peer).unwrap();
        assert_eq!(session.addr, endpoints[0]);
        assert_eq!(session.extra, vec![endpoints[1]]);
        assert_eq!(session.candidates(), vec![endpoints[0], endpoints[1]]);
    }

    #[test]
    fn broken_extra_endpoints_are_skipped() {
        let (_, peer, pair) = ids();
        let mut record = our_record(&pair, peer, 0, Uuid::new_v4(), &["203.0.113.7:1".parse().unwrap()], 0);
        record.extra_endpoints = vec![
            Endpoint { ip: "не-адрес".into(), port: 5 },
            Endpoint { ip: "198.51.100.2".into(), port: 0 },
            Endpoint { ip: "198.51.100.2".into(), port: 70000 },
            Endpoint { ip: "198.51.100.2".into(), port: 6 },
        ];
        record.signature = pair.sign_rendezvous(&{
            let mut r = record.clone();
            r.signature.clear();
            r.encode_to_vec()
        }).to_vec();
        assert_eq!(peer_session_from(&record, &pair, peer).unwrap().extra, vec!["198.51.100.2:6".parse().unwrap()]);
    }

    #[test]
    fn peer_session_from_other_peer_is_ignored() {
        let (_, peer, pair) = ids();
        let other = Uuid::new_v4();
        let payload = our_record(&pair, other, 0, Uuid::new_v4(), &["203.0.113.7:1".parse().unwrap()], 0).encode_to_vec();
        assert!(decode_peer_session(&payload, peer, &pair).unwrap().is_none());
    }
}
