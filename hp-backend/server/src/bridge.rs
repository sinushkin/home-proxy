//! Мост «клиенты роутера -> WireGuard» (по образцу `server-rs`).
//!
//! Клиентов различает [`ClientKey`]: за роутером это `client_id` (u8) из
//! `WrappedData`, а телефон, подключённый к серверу напрямую (без роутера),
//! присылает обычную `Data` и считается клиентом [`ClientKey::Direct`]. Для
//! каждого нового клиента мост заводит свой локальный UDP-сокет `127.0.0.1:0` и
//! шлёт с него пакеты WireGuard'у: тот видит разных клиентов как разные адреса
//! источника и отвечает каждому туда, откуда пришло. Ответ WireGuard, пришедший
//! на сокет клиента, уходит обратно тем же путём (`Reply`). Неактивные клиенты
//! удаляются по таймауту (`cleanup`).

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

const REPLY_BUFFER: usize = 4096;

/// Кто прислал пакет: клиент за роутером (`client_id`) или телефон напрямую.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ClientKey {
    /// Телефон без роутера: обычная `Data`, ответ тоже обычной `Data`.
    Direct,
    /// Клиент за роутером: `WrappedData` с этим `client_id`.
    Routed(u8),
}

impl fmt::Display for ClientKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientKey::Direct => write!(f, "прямой"),
            ClientKey::Routed(client_id) => write!(f, "{client_id}"),
        }
    }
}

/// Куда отправлять ответы WireGuard'а (в проде — обратно по дырам).
pub trait Reply: Send + Sync + 'static {
    fn send(&self, client: ClientKey, payload: Vec<u8>) -> impl Future<Output = Result<()>> + Send;
}

struct Client {
    socket: Arc<UdpSocket>,
    last_activity: Arc<Mutex<Instant>>,
    reader: JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

pub struct Bridge<R: Reply> {
    wg_addr: SocketAddr,
    reply: Arc<R>,
    inactive_timeout: Duration,
    clients: Mutex<HashMap<ClientKey, Client>>,
}

impl<R: Reply> Bridge<R> {
    pub fn new(wg_addr: SocketAddr, reply: R, inactive_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            wg_addr,
            reply: Arc::new(reply),
            inactive_timeout,
            clients: Mutex::new(HashMap::new()),
        })
    }

    /// Пакет от клиента: отправляет его WireGuard'у с локального сокета этого
    /// клиента (сокет создаётся при первом пакете).
    pub async fn send_to_wireguard(&self, client: ClientKey, payload: &[u8]) -> Result<()> {
        let socket = self.socket_for(client).await?;
        socket
            .send(payload)
            .await
            .with_context(|| format!("клиент {client}: не удалось отправить в WireGuard {}", self.wg_addr))?;
        Ok(())
    }

    /// Удаляет клиентов, от которых давно нет ни пакетов, ни ответов.
    /// Возвращает, сколько удалено.
    pub fn cleanup(&self) -> usize {
        let mut clients = self.clients.lock().unwrap();
        let inactive: Vec<ClientKey> = clients
            .iter()
            .filter(|(_, client)| client.last_activity.lock().unwrap().elapsed() > self.inactive_timeout)
            .map(|(client, _)| *client)
            .collect();
        for client in &inactive {
            clients.remove(client);
            log::info!("клиент {client} удалён по неактивности");
        }
        inactive.len()
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Локальный адрес сокета клиента, с которого он виден WireGuard'у.
    #[cfg(test)]
    pub fn local_addr(&self, client: ClientKey) -> Option<SocketAddr> {
        self.clients.lock().unwrap().get(&client).and_then(|c| c.socket.local_addr().ok())
    }

    fn touch(&self, client: ClientKey) -> Option<Arc<UdpSocket>> {
        let clients = self.clients.lock().unwrap();
        let client = clients.get(&client)?;
        *client.last_activity.lock().unwrap() = Instant::now();
        Some(client.socket.clone())
    }

    async fn socket_for(&self, client_key: ClientKey) -> Result<Arc<UdpSocket>> {
        if let Some(socket) = self.touch(client_key) {
            return Ok(socket);
        }

        // Сокет «подключён» к WireGuard: посторонние отправители на него не пройдут.
        let socket = Arc::new(
            UdpSocket::bind("127.0.0.1:0")
                .await
                .with_context(|| format!("клиент {client_key}: не удалось создать локальный сокет"))?,
        );
        socket
            .connect(self.wg_addr)
            .await
            .with_context(|| format!("клиент {client_key}: connect к WireGuard {}", self.wg_addr))?;
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        let reader = tokio::spawn(read_replies(
            client_key,
            socket.clone(),
            last_activity.clone(),
            self.reply.clone(),
        ));
        let client = Client { socket: socket.clone(), last_activity, reader };

        match self.clients.lock().unwrap().entry(client_key) {
            Entry::Occupied(existing) => Ok(existing.get().socket.clone()), // `client` дропнется, reader остановится
            Entry::Vacant(slot) => {
                log::info!(
                    "новый клиент {client_key}: локальный адрес {}",
                    socket.local_addr().map(|a| a.to_string()).unwrap_or_default()
                );
                slot.insert(client);
                Ok(socket)
            }
        }
    }
}

/// Читает ответы WireGuard'а на сокете клиента и отправляет их дальше.
async fn read_replies<R: Reply>(
    client: ClientKey,
    socket: Arc<UdpSocket>,
    last_activity: Arc<Mutex<Instant>>,
    reply: Arc<R>,
) {
    let mut buf = vec![0u8; REPLY_BUFFER];
    loop {
        match socket.recv(&mut buf).await {
            Ok(len) => {
                *last_activity.lock().unwrap() = Instant::now();
                if let Err(e) = reply.send(client, buf[..len].to_vec()).await {
                    log::debug!("клиент {client}: ответ {len} байт потерян: {e:#}");
                }
            }
            Err(e) => {
                log::warn!("клиент {client}: ошибка приёма от WireGuard: {e}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    /// Собирает ответы, которые ушли бы роутеру.
    struct Collect(mpsc::UnboundedSender<(ClientKey, Vec<u8>)>);

    impl Reply for Collect {
        async fn send(&self, client: ClientKey, payload: Vec<u8>) -> Result<()> {
            let _ = self.0.send((client, payload));
            Ok(())
        }
    }

    async fn fake_wireguard() -> (UdpSocket, SocketAddr) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        (socket, addr)
    }

    async fn recv(wg: &UdpSocket) -> (Vec<u8>, SocketAddr) {
        let mut buf = [0u8; 2048];
        let (len, from) = timeout(Duration::from_secs(2), wg.recv_from(&mut buf))
            .await
            .expect("WireGuard ничего не получил")
            .unwrap();
        (buf[..len].to_vec(), from)
    }

    #[tokio::test]
    async fn each_client_gets_its_own_endpoint_and_replies_return_to_it() {
        let (wg, wg_addr) = fake_wireguard().await;
        let (tx, mut replies) = mpsc::unbounded_channel();
        let bridge = Bridge::new(wg_addr, Collect(tx), Duration::from_secs(60));

        bridge.send_to_wireguard(ClientKey::Routed(1), b"a1").await.unwrap();
        bridge.send_to_wireguard(ClientKey::Routed(2), b"b1").await.unwrap();
        bridge.send_to_wireguard(ClientKey::Routed(1), b"a2").await.unwrap();
        let (first, from_1) = recv(&wg).await;
        let (second, from_2) = recv(&wg).await;
        let (third, from_1_again) = recv(&wg).await;

        assert_eq!((first.as_slice(), second.as_slice(), third.as_slice()), (&b"a1"[..], &b"b1"[..], &b"a2"[..]));
        assert_ne!(from_1, from_2, "клиенты должны быть видны WireGuard'у как разные адреса");
        assert_eq!(from_1, from_1_again, "один клиент — один и тот же адрес");
        assert_eq!(bridge.local_addr(ClientKey::Routed(1)), Some(from_1));
        assert_eq!(bridge.client_count(), 2);

        wg.send_to(b"reply-for-2", from_2).await.unwrap();
        wg.send_to(b"reply-for-1", from_1).await.unwrap();
        let mut got = Vec::new();
        for _ in 0..2 {
            got.push(timeout(Duration::from_secs(2), replies.recv()).await.unwrap().unwrap());
        }
        got.sort();
        assert_eq!(
            got,
            vec![
                (ClientKey::Routed(1), b"reply-for-1".to_vec()),
                (ClientKey::Routed(2), b"reply-for-2".to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn direct_client_is_separate_from_routed_ones() {
        let (wg, wg_addr) = fake_wireguard().await;
        let (tx, mut replies) = mpsc::unbounded_channel();
        let bridge = Bridge::new(wg_addr, Collect(tx), Duration::from_secs(60));

        bridge.send_to_wireguard(ClientKey::Direct, b"direct").await.unwrap();
        bridge.send_to_wireguard(ClientKey::Routed(0), b"routed-0").await.unwrap();
        let (_, direct_from) = recv(&wg).await;
        let (_, routed_from) = recv(&wg).await;
        assert_ne!(direct_from, routed_from, "прямой клиент и client_id 0 — разные клиенты");

        wg.send_to(b"answer", direct_from).await.unwrap();
        let (client, payload) = timeout(Duration::from_secs(2), replies.recv()).await.unwrap().unwrap();
        assert_eq!((client, payload.as_slice()), (ClientKey::Direct, &b"answer"[..]));
        assert_eq!(bridge.client_count(), 2);
    }

    #[tokio::test]
    async fn inactive_clients_are_removed_and_recreated_on_next_packet() {
        let (wg, wg_addr) = fake_wireguard().await;
        let (tx, _replies) = mpsc::unbounded_channel();
        let bridge = Bridge::new(wg_addr, Collect(tx), Duration::from_millis(50));

        bridge.send_to_wireguard(ClientKey::Routed(7), b"x").await.unwrap();
        recv(&wg).await;
        assert_eq!(bridge.cleanup(), 0, "свежий клиент не удаляется");

        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(bridge.cleanup(), 1);
        assert_eq!(bridge.client_count(), 0);

        bridge.send_to_wireguard(ClientKey::Routed(7), b"y").await.unwrap();
        let (payload, _) = recv(&wg).await;
        assert_eq!(payload, b"y");
        assert_eq!(bridge.client_count(), 1);
    }

    #[tokio::test]
    async fn replies_from_foreign_sources_are_ignored() {
        let (wg, wg_addr) = fake_wireguard().await;
        let (tx, mut replies) = mpsc::unbounded_channel();
        let bridge = Bridge::new(wg_addr, Collect(tx), Duration::from_secs(60));

        bridge.send_to_wireguard(ClientKey::Routed(1), b"hi").await.unwrap();
        recv(&wg).await;
        let client_addr = bridge.local_addr(ClientKey::Routed(1)).unwrap();

        let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        stranger.send_to(b"spoof", client_addr).await.unwrap();

        assert!(
            timeout(Duration::from_millis(200), replies.recv()).await.is_err(),
            "пакет не от WireGuard дошёл до клиента"
        );
    }
}
