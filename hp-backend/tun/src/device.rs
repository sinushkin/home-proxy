//! Устройство IP-пакетов для мостов (`bridge`, `hub`): TUN ядра или канал в памяти.
//!
//! Мост не зависит от того, куда уходят пакеты: в интерфейс TUN (Linux, OpenWrt, Android) или в
//! собственный сетевой стек процесса (`channel_pair`: второй конец — `StackEnd` с
//! `AsyncRead`/`AsyncWrite` по пакету за вызов, как у TUN; так работает `hp-netstack` на Windows,
//! где нет ни TUN, ни удобного NAT).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, Mutex};

/// Читает и пишет по одному IP-пакету за вызов.
pub trait PacketDevice: Send + Sync + 'static {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
    fn send(&self, packet: &[u8]) -> impl Future<Output = io::Result<usize>> + Send;
}

#[cfg(any(target_os = "linux", target_os = "android"))]
impl PacketDevice for crate::Tun {
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send {
        crate::Tun::recv(self, buf)
    }

    fn send(&self, packet: &[u8]) -> impl Future<Output = io::Result<usize>> + Send {
        crate::Tun::send(self, packet)
    }
}

/// Сколько пакетов держит канал в каждую сторону.
const CHANNEL_CAPACITY: usize = 1024;

/// Канальное устройство со стороны моста: `send` отдаёт пакет стеку, `recv` берёт его ответы.
pub struct ChannelDevice {
    to_stack: mpsc::Sender<Vec<u8>>,
    from_stack: Mutex<mpsc::Receiver<Vec<u8>>>,
}

/// Другой конец канала — для сетевого стека: чтение отдаёт по одному пакету от моста, запись
/// одним вызовом — один пакет мосту.
pub struct StackEnd {
    from_bridge: mpsc::Receiver<Vec<u8>>,
    to_bridge: mpsc::Sender<Vec<u8>>,
}

/// Пара «устройство моста — конец для стека».
pub fn channel_pair() -> (ChannelDevice, StackEnd) {
    let (to_stack, from_bridge) = mpsc::channel(CHANNEL_CAPACITY);
    let (to_bridge, from_stack) = mpsc::channel(CHANNEL_CAPACITY);
    (ChannelDevice { to_stack, from_stack: Mutex::new(from_stack) }, StackEnd { from_bridge, to_bridge })
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "канал устройства закрыт")
}

impl PacketDevice for ChannelDevice {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let packet = self.from_stack.lock().await.recv().await.ok_or_else(closed)?;
        let n = packet.len().min(buf.len());
        buf[..n].copy_from_slice(&packet[..n]);
        Ok(n)
    }

    async fn send(&self, packet: &[u8]) -> io::Result<usize> {
        self.to_stack.send(packet.to_vec()).await.map_err(|_| closed())?;
        Ok(packet.len())
    }
}

impl AsyncRead for StackEnd {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.from_bridge.poll_recv(cx) {
            Poll::Ready(Some(packet)) => {
                let n = packet.len().min(buf.remaining());
                buf.put_slice(&packet[..n]);
                Poll::Ready(Ok(()))
            }
            // Мост закрылся: для стека это конец потока.
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for StackEnd {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        // Как у TUN: переполненная очередь — пакет теряется, а не блокирует стек.
        match self.to_bridge.try_send(buf.to_vec()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Poll::Ready(Ok(buf.len())),
            Err(mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(closed())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn packets_keep_their_boundaries_both_ways() {
        let (device, mut stack) = channel_pair();
        device.send(b"one").await.unwrap();
        device.send(b"second").await.unwrap();
        let mut buf = [0u8; 64];
        let n = stack.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"one");
        let n = stack.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"second");

        stack.write_all(b"reply").await.unwrap();
        let n = device.recv(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"reply");
    }
}
