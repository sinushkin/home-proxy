//! Режим «VPS»: у сервера белый IP, пробивать ничего не нужно.
//!
//! Клиент заранее знает `ip:порт знакомства` сервера. Вместо STUN и MQTT первый
//! (bootstrap) слот договаривается через этот порт: клиент шлёт свою запись
//! `Rendezvous` слота 0, сервер отвечает своей — «иди на мой порт P». Остальные слоты,
//! как и в обычном режиме, договариваются через виртуал-брокер по живой дыре.
//!
//! Каждая (пере)регистрация слота на сервере занимает случайный свободный порт из
//! диапазона: плохую дыру сервер пробивает заново на новом порту, и клиент узнаёт
//! новый адрес из той же записи `Rendezvous`.
//!
//! Обмен на порту знакомства — обычный `PeerMessage::Lite` с `Rendezvous` внутри,
//! замаскированный XOR-вектором, который обе стороны выводят из пары GUID
//! (`bootstrap_key`). Как и остальная маскировка, это не защита.

use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::codec::{self, XorKey};
use crate::proto::{lite, peer_message, Lite, PeerMessage, Rendezvous};
use crate::rendezvous::{self, PeerSession};

/// Порт знакомства по умолчанию и диапазон портов слотов (10 000 портов вместе с ним).
pub const DEFAULT_BOOTSTRAP_PORT: u16 = 40000;
pub const DEFAULT_SLOT_PORTS: RangeInclusive<u16> = 40001..=49999;

/// Сколько раз пробуем занять случайный порт из диапазона, прежде чем сдаться.
const BIND_ATTEMPTS: usize = 64;

/// XOR-вектор обмена на порту знакомства: одинаков у обеих сторон, зависит от пары GUID.
pub fn bootstrap_key(a: Uuid, b: Uuid) -> XorKey {
    let (a, b) = (a.into_bytes(), b.into_bytes());
    std::array::from_fn(|i| a[i] ^ b[i] ^ 0x5a)
}

fn wrap(record: Rendezvous) -> PeerMessage {
    PeerMessage {
        body: Some(peer_message::Body::Lite(Lite {
            slot: 0,
            payload: Some(lite::Payload::Rendezvous(record)),
        })),
    }
}

/// Достаёт запись слота 0 от ожидаемого пира; всё остальное — `None`.
fn unwrap(data: &[u8], key: &XorKey, peer_id: Uuid) -> Option<PeerSession> {
    let msg = codec::decode(data.to_vec(), key).ok()?;
    let Some(peer_message::Body::Lite(Lite { payload: Some(lite::Payload::Rendezvous(r)), .. })) = msg.body else {
        return None;
    };
    if r.peer_id != peer_id.to_string() || r.slot != 0 {
        return None;
    }
    rendezvous::peer_session_from(&r).ok()
}

/// Случайный свободный порт из диапазона (кроме `exclude`).
pub async fn bind_random_port(ports: &RangeInclusive<u16>, exclude: u16) -> Result<UdpSocket> {
    let (low, high) = (*ports.start(), *ports.end());
    anyhow::ensure!(low <= high, "пустой диапазон портов {low}..={high}");
    let span = u64::from(high - low) + 1;
    for _ in 0..BIND_ATTEMPTS {
        let random = u64::from_le_bytes(Uuid::new_v4().into_bytes()[..8].try_into().expect("8 байт"));
        let port = low + (random % span) as u16;
        if port == exclude {
            continue;
        }
        if let Ok(socket) = UdpSocket::bind(("0.0.0.0", port)).await {
            return Ok(socket);
        }
    }
    anyhow::bail!("не нашёл свободный порт в {low}..={high} за {BIND_ATTEMPTS} попыток")
}

/// Сервер: текущая запись его слота 0 (пока слот не залинкован), её отдают клиенту.
#[derive(Default)]
pub struct ServerBootstrap {
    current: Mutex<Option<Rendezvous>>,
}

impl ServerBootstrap {
    pub fn set(&self, record: Option<Rendezvous>) {
        *self.current.lock().unwrap() = record;
    }

    fn current(&self) -> Option<Rendezvous> {
        self.current.lock().unwrap().clone()
    }
}

/// Сервер: слушает порт знакомства. Запись клиента отдаёт слоту 0, в ответ шлёт
/// свою текущую запись (если слот 0 сейчас ждёт клиента).
pub async fn serve_bootstrap(
    socket: UdpSocket,
    state: std::sync::Arc<ServerBootstrap>,
    key: XorKey,
    client_id: Uuid,
    slot0: mpsc::Sender<PeerSession>,
) {
    let mut buf = [0u8; 1500];
    let mut last_fed: Option<Uuid> = None;
    loop {
        let Ok((n, from)) = socket.recv_from(&mut buf).await else { continue };
        let Some(session) = unwrap(&buf[..n], &key, client_id) else { continue };
        if last_fed != Some(session.session_id) && slot0.try_send(session.clone()).is_ok() {
            log::debug!("знакомство: запись клиента {from}, сессия {}", session.session_id);
            last_fed = Some(session.session_id);
        }
        if let Some(record) = state.current() {
            let _ = socket.send_to(&codec::encode(&wrap(record), &key), from).await;
        }
    }
}

/// Клиент: сокет знакомства и адрес сервера.
pub struct ClientBootstrap {
    pub socket: UdpSocket,
    pub server: SocketAddr,
    pub key: XorKey,
}

impl ClientBootstrap {
    pub async fn new(server: SocketAddr, key: XorKey) -> Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0)).await.context("сокет знакомства")?;
        Ok(Self { socket, server, key })
    }

    /// Раз в секунду шлёт серверу нашу запись слота 0 (пока задачу не остановят).
    pub async fn announce(&self, record: Rendezvous) {
        let packet = codec::encode(&wrap(record), &self.key);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            ticker.tick().await;
            let _ = self.socket.send_to(&packet, self.server).await;
        }
    }

    /// Принимает ответы сервера и отдаёт его запись слоту 0.
    pub async fn receive(&self, server_id: Uuid, slot0: mpsc::Sender<PeerSession>) {
        let mut buf = [0u8; 1500];
        let mut last_fed: Option<Uuid> = None;
        loop {
            let Ok((n, _)) = self.socket.recv_from(&mut buf).await else { continue };
            let Some(session) = unwrap(&buf[..n], &self.key, server_id) else { continue };
            if last_fed != Some(session.session_id) && slot0.try_send(session.clone()).is_ok() {
                log::debug!("знакомство: сервер предлагает {} (сессия {})", session.addr, session.session_id);
                last_fed = Some(session.session_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_key_is_symmetric_and_pair_specific() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(bootstrap_key(a, b), bootstrap_key(b, a));
        assert_ne!(bootstrap_key(a, b), bootstrap_key(a, c));
    }

    #[test]
    fn only_slot_zero_records_of_the_expected_peer_are_accepted() {
        let (me, peer) = (Uuid::new_v4(), Uuid::new_v4());
        let key = bootstrap_key(me, peer);
        let endpoint = [SocketAddr::from(([203, 0, 113, 10], 41234))];
        let record = |id: Uuid, slot: u8| {
            codec::encode(&wrap(rendezvous::our_record(id, slot, Uuid::new_v4(), &endpoint, codec::random_key(), 0)), &key)
        };
        let ok = unwrap(&record(peer, 0), &key, peer).expect("запись пира");
        assert_eq!(ok.addr, endpoint[0]);
        assert!(unwrap(&record(me, 0), &key, peer).is_none(), "своя запись (эхо)");
        assert!(unwrap(&record(peer, 3), &key, peer).is_none(), "не bootstrap-слот");
        assert!(unwrap(&record(peer, 0), &codec::random_key(), peer).is_none(), "чужой вектор");
    }

    #[tokio::test]
    async fn random_ports_stay_in_range_and_skip_the_excluded_one() {
        let range = 45000..=45003;
        let mut held = Vec::new();
        for _ in 0..3 {
            let socket = bind_random_port(&range, 45001).await.unwrap();
            let port = socket.local_addr().unwrap().port();
            assert!(range.contains(&port) && port != 45001);
            held.push(socket);
        }
        assert!(bind_random_port(&range, 45001).await.is_err(), "свободных портов не осталось");
    }
}
