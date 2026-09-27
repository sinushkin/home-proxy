//! Защищённый канал управления: ключ из строки подключения, по сети он не передаётся.
//!
//! Рукопожатие: клиент шлёт кадр `HPC1 ‖ nonce_c` (16 случайных байт), служба отвечает кадром
//! `nonce_s`. Из ключа и обоих nonce выводятся два ключа сессии (SHA-256), по одному на
//! направление; дальше каждый кадр — protobuf, зашифрованный ChaCha20-Poly1305, nonce шифра —
//! номер кадра в своём направлении (TCP сохраняет порядок; повтор или подмена кадра ломают
//! тег). Первое зашифрованное сообщение клиента — `Hello`: если служба не может его
//! расшифровать, ключ у клиента другой, и соединение закрывается. Ответ `Welcome` доказывает
//! клиенту, что и служба знает ключ.
//!
//! Шифр и хеш — те же, что у подписи дыр (`connection::auth`): на роутере новых зависимостей нет.

use anyhow::{Context, Result};
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use prost::Message;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::proto::ControlMessage;
use crate::{read_raw, write_raw};

/// Начало первого кадра клиента: протокол и его версия.
const MAGIC: &[u8; 4] = b"HPC1";
const NONCE_LEN: usize = 16;
const TAG_LEN: usize = 16;

/// Ключ сессии одного направления.
fn session_key(secret: &str, direction: &[u8], client: &[u8; NONCE_LEN], server: &[u8; NONCE_LEN]) -> ChaCha20Poly1305 {
    let mut h = Sha256::new();
    h.update(b"home-proxy control v1\0");
    h.update(direction);
    h.update(client);
    h.update(server);
    h.update(Sha256::digest(secret.trim().as_bytes()));
    ChaCha20Poly1305::new(&h.finalize())
}

fn frame_nonce(counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&counter.to_le_bytes());
    nonce.into()
}

fn random_nonce() -> [u8; NONCE_LEN] {
    *uuid::Uuid::new_v4().as_bytes()
}

/// Отправляющая половина: шифрует каждое сообщение своим номером.
pub struct SealedWriter<W> {
    inner: W,
    cipher: ChaCha20Poly1305,
    counter: u64,
}

/// Принимающая половина.
pub struct SealedReader<R> {
    inner: R,
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl<W: AsyncWrite + Unpin> SealedWriter<W> {
    pub async fn send(&mut self, message: &ControlMessage) -> Result<()> {
        let mut buf = message.encode_to_vec();
        let tag = self.cipher.encrypt_in_place_detached(&frame_nonce(self.counter), b"", &mut buf).map_err(|_| anyhow::anyhow!("шифрование кадра"))?;
        self.counter += 1;
        buf.extend_from_slice(&tag);
        write_raw(&mut self.inner, &buf).await
    }
}

impl<R: AsyncRead + Unpin> SealedReader<R> {
    /// Следующее сообщение; `None` — соединение закрыто между кадрами. Ошибка — кадр подделан
    /// или ключ другой.
    pub async fn recv(&mut self) -> Result<Option<ControlMessage>> {
        let Some(mut buf) = read_raw(&mut self.inner).await? else { return Ok(None) };
        anyhow::ensure!(buf.len() >= TAG_LEN, "короткий кадр");
        let tag = Tag::clone_from_slice(&buf[buf.len() - TAG_LEN..]);
        buf.truncate(buf.len() - TAG_LEN);
        self.cipher
            .decrypt_in_place_detached(&frame_nonce(self.counter), b"", &mut buf, &tag)
            .map_err(|_| anyhow::anyhow!("кадр не расшифровывается: другой ключ или подмена"))?;
        self.counter += 1;
        Ok(Some(ControlMessage::decode(buf.as_slice()).context("битый кадр управления")?))
    }
}

/// Рукопожатие со стороны клиента.
pub async fn client<R, W>(mut reader: R, mut writer: W, secret: &str) -> Result<(SealedReader<R>, SealedWriter<W>)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let client_nonce = random_nonce();
    let mut first = MAGIC.to_vec();
    first.extend_from_slice(&client_nonce);
    write_raw(&mut writer, &first).await?;
    let reply = read_raw(&mut reader).await?.context("служба закрыла соединение")?;
    let server_nonce: [u8; NONCE_LEN] = reply.as_slice().try_into().map_err(|_| anyhow::anyhow!("это не служба home-proxy (другое рукопожатие)"))?;
    Ok((
        SealedReader { inner: reader, cipher: session_key(secret, b"s2c", &client_nonce, &server_nonce), counter: 0 },
        SealedWriter { inner: writer, cipher: session_key(secret, b"c2s", &client_nonce, &server_nonce), counter: 0 },
    ))
}

/// Рукопожатие со стороны службы. Ключ проверяется первым сообщением клиента (`Hello`).
pub async fn server<R, W>(mut reader: R, mut writer: W, secret: &str) -> Result<(SealedReader<R>, SealedWriter<W>)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let first = read_raw(&mut reader).await?.context("нет рукопожатия")?;
    anyhow::ensure!(first.len() == MAGIC.len() + NONCE_LEN && first.starts_with(MAGIC), "чужое рукопожатие");
    let client_nonce: [u8; NONCE_LEN] = first[MAGIC.len()..].try_into().expect("длина проверена");
    let server_nonce = random_nonce();
    write_raw(&mut writer, &server_nonce).await?;
    Ok((
        SealedReader { inner: reader, cipher: session_key(secret, b"c2s", &client_nonce, &server_nonce), counter: 0 },
        SealedWriter { inner: writer, cipher: session_key(secret, b"s2c", &client_nonce, &server_nonce), counter: 0 },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message;
    use crate::proto::control_message::Body;
    use crate::proto::{GetStatus, Hello};

    async fn pair(client_secret: &str, server_secret: &str) -> (Result<Option<ControlMessage>>, Vec<u8>) {
        let (a, b) = tokio::io::duplex(4096);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (server_secret, client_secret) = (server_secret.to_string(), client_secret.to_string());
        let server = tokio::spawn(async move {
            let (mut r, _w) = server(br, bw, &server_secret).await.unwrap();
            r.recv().await
        });
        let (_r, mut w) = client(ar, aw, &client_secret).await.unwrap();
        let hello = message(Body::Hello(Hello { version: crate::PROTOCOL_VERSION }));
        w.send(&hello).await.unwrap();
        (server.await.unwrap(), hello.encode_to_vec())
    }

    #[tokio::test]
    async fn same_key_decrypts_other_key_fails() {
        let (got, _) = pair("key-1", "key-1").await;
        assert!(matches!(got.unwrap().unwrap().body, Some(Body::Hello(_))));
        let (got, _) = pair("key-1", "key-2").await;
        assert!(got.is_err(), "с чужим ключом кадр не расшифровывается");
    }

    #[tokio::test]
    async fn frames_are_not_plaintext_and_replay_fails() {
        let (a, b) = tokio::io::duplex(4096);
        let (ar, aw) = tokio::io::split(a);
        let (mut br, mut bw) = tokio::io::split(b);
        let client = tokio::spawn(async move {
            let (_r, mut w) = client(ar, aw, "secret").await.unwrap();
            let status = message(Body::GetStatus(GetStatus {}));
            w.send(&status).await.unwrap();
            w.send(&status).await.unwrap();
        });
        // Сторона службы вручную: видим сырые кадры.
        let first = read_raw(&mut br).await.unwrap().unwrap();
        let server_nonce = [7u8; NONCE_LEN];
        write_raw(&mut bw, &server_nonce).await.unwrap();
        client.await.unwrap();
        let c1 = read_raw(&mut br).await.unwrap().unwrap();
        let c2 = read_raw(&mut br).await.unwrap().unwrap();
        assert_ne!(c1, c2, "одинаковые сообщения шифруются по-разному (номер кадра)");
        let client_nonce: [u8; NONCE_LEN] = first[4..].try_into().unwrap();
        let cipher = session_key("secret", b"c2s", &client_nonce, &server_nonce);
        // Второй кадр под номером первого — подмена порядка не проходит.
        let mut body = c2[..c2.len() - TAG_LEN].to_vec();
        let tag = Tag::clone_from_slice(&c2[c2.len() - TAG_LEN..]);
        assert!(cipher.decrypt_in_place_detached(&frame_nonce(0), b"", &mut body, &tag).is_err());
    }
}
