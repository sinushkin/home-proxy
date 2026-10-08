//! TUN через Wintun: сессия отдаёт пакеты блокирующим вызовом, поэтому чтение — в отдельном
//! потоке, который кладёт пакеты в канал tokio. Запись быстрая и не блокирует (кольцо драйвера).

use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};

use crate::device::PacketDevice;
use crate::platform::TunDevice;
use crate::TunConfig;

/// Сколько пакетов ждёт приёма из потока чтения.
const READ_QUEUE: usize = 1024;
/// Кольцо драйвера в каждую сторону (степень двойки между 128 КиБ и 64 МиБ).
const RING_CAPACITY: u32 = 4 * 1024 * 1024;
const TUNNEL_TYPE: &str = "HomeProxy";

pub struct Tun {
    session: Arc<wintun::Session>,
    packets: Mutex<mpsc::Receiver<Vec<u8>>>,
    name: String,
}

fn other(e: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(e)
}

/// `wintun.dll`: `WINTUN_DLL` (путь), иначе рядом с исполняемым файлом, иначе обычный поиск DLL.
fn load_library() -> io::Result<wintun::Wintun> {
    let candidates = std::env::var_os("WINTUN_DLL").map(std::path::PathBuf::from).into_iter().chain(
        std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join("wintun.dll"))),
    );
    let mut last = None;
    for path in candidates.chain(Some("wintun.dll".into())) {
        // SAFETY: загружаем библиотеку Wintun по пути администратора; её инициализация — код WireGuard.
        match unsafe { wintun::load_from_path(&path) } {
            Ok(library) => return Ok(library),
            Err(e) => last = Some(format!("{}: {e}", path.display())),
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, format!("не загрузился wintun.dll ({}); положите его рядом с .exe или задайте WINTUN_DLL", last.unwrap_or_default())))
}

fn prefix_mask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(prefix.min(32))) };
    Ipv4Addr::from(bits)
}

impl Tun {
    /// Создаёт адаптер Wintun (или берёт существующий с тем же именем), задаёт адрес и MTU.
    /// `config.up` значения не имеет: адаптер работает, пока открыта сессия.
    pub fn create(config: &TunConfig) -> io::Result<Self> {
        let library = load_library()?;
        let name = if config.name.is_empty() { "hp0".to_string() } else { config.name.clone() };
        let adapter = match wintun::Adapter::open(&library, &name) {
            Ok(adapter) => adapter,
            Err(_) => wintun::Adapter::create(&library, &name, TUNNEL_TYPE, None).map_err(other)?,
        };
        if let Some(mtu) = config.mtu {
            adapter.set_mtu(usize::from(mtu)).map_err(other)?;
        }
        if let Some((address, prefix)) = config.address {
            adapter.set_network_addresses_tuple(address.into(), prefix_mask(prefix).into(), None).map_err(other)?;
        }
        let session = Arc::new(adapter.start_session(RING_CAPACITY).map_err(other)?);

        let (tx, rx) = mpsc::channel(READ_QUEUE);
        let reader = session.clone();
        std::thread::Builder::new()
            .name("wintun-read".into())
            .spawn(move || {
                // Ошибка — сессию закрыли (`shutdown` в Drop): выходим, канал закроется.
                while let Ok(packet) = reader.receive_blocking() {
                    if tx.blocking_send(packet.bytes().to_vec()).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { session, packets: Mutex::new(rx), name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Читает один IP-пакет в `buf`; длина пакета.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let packet = self.packets.lock().await.recv().await.ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "сессия Wintun закрыта"))?;
        let len = packet.len().min(buf.len());
        buf[..len].copy_from_slice(&packet[..len]);
        Ok(len)
    }

    /// Пишет один IP-пакет. Кольцо драйвера переполнено — ошибка (мост считает пакет потерянным).
    pub async fn send(&self, packet: &[u8]) -> io::Result<usize> {
        let size = u16::try_from(packet.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "пакет длиннее 65535 байт"))?;
        let mut out = self.session.allocate_send_packet(size).map_err(other)?;
        out.bytes_mut().copy_from_slice(packet);
        self.session.send_packet(out);
        Ok(packet.len())
    }
}

impl Drop for Tun {
    fn drop(&mut self) {
        // Будит поток чтения; адаптер закроется, когда поток отпустит сессию.
        let _ = self.session.shutdown();
    }
}

impl PacketDevice for Tun {
    fn recv(&self, buf: &mut [u8]) -> impl std::future::Future<Output = io::Result<usize>> + Send {
        Tun::recv(self, buf)
    }

    fn send(&self, packet: &[u8]) -> impl std::future::Future<Output = io::Result<usize>> + Send {
        Tun::send(self, packet)
    }
}

impl TunDevice for Tun {
    fn create(config: &TunConfig) -> io::Result<Self> {
        Tun::create(config)
    }

    fn name(&self) -> &str {
        Tun::name(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_masks() {
        assert_eq!(prefix_mask(16), Ipv4Addr::new(255, 255, 0, 0));
        assert_eq!(prefix_mask(32), Ipv4Addr::new(255, 255, 255, 255));
        assert_eq!(prefix_mask(0), Ipv4Addr::new(0, 0, 0, 0));
    }
}
