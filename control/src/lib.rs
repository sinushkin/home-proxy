//! Протокол управления службой home-proxy (`hp-server`): сообщения (`proto`), кадрирование,
//! токен доступа, клиент и ссылка сопряжения для QR. Сервер протокола живёт в самой службе,
//! клиент — в трее (`control/tray`). Описание — `control/README.md`.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/hp.control.rs"));
}

use proto::control_message::Body;
use proto::{ControlMessage, Hello, PairingBundle, Welcome};

/// Версия протокола в `Hello`/`Welcome`.
pub const PROTOCOL_VERSION: u32 = 1;
/// Адрес управления по умолчанию: только loopback.
pub const DEFAULT_ADDR: &str = "127.0.0.1:47001";
/// Имя файла токена рядом с настройками службы.
pub const TOKEN_FILE: &str = "control.token";
/// Самый длинный кадр: статус десятков пиров — единицы килобайт.
pub const MAX_FRAME: usize = 256 * 1024;
/// Схема ссылки сопряжения (QR и deep link Android).
pub const PAIRING_PREFIX: &str = "homeproxy://pair?d=";

/// Сообщение с телом `body`.
pub fn message(body: Body) -> ControlMessage {
    ControlMessage { body: Some(body) }
}

/// Читает один кадр; `None` — соединение закрыто между кадрами.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<ControlMessage>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    anyhow::ensure!(len <= MAX_FRAME, "кадр {len} байт длиннее {MAX_FRAME}");
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.context("обрыв посреди кадра")?;
    Ok(Some(ControlMessage::decode(buf.as_slice()).context("битый кадр управления")?))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, message: &ControlMessage) -> Result<()> {
    let body = message.encode_to_vec();
    anyhow::ensure!(body.len() <= MAX_FRAME, "кадр {} байт длиннее {MAX_FRAME}", body.len());
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame).await?;
    Ok(())
}

/// Сравнение токенов за время, не зависящее от места первого расхождения.
pub fn token_matches(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

/// Новый случайный токен: 32 байта из CSPRNG ОС (через v4 UUID), в hex.
pub fn new_token() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Токен из файла; если файла нет — создаёт новый (на Unix — с правами 600).
pub fn load_or_create_token(path: &Path) -> Result<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let token = text.trim().to_string();
        anyhow::ensure!(!token.is_empty(), "файл токена {} пуст", path.display());
        return Ok(token);
    }
    let token = new_token();
    write_private(path, &token).with_context(|| format!("не удалось записать токен {}", path.display()))?;
    Ok(token)
}

/// Токен из файла (для клиента).
pub fn read_token(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("не удалось прочитать токен {}", path.display()))?;
    Ok(text.trim().to_string())
}

/// Пишет файл, доступный только владельцу (на Unix — 600; на Windows права наследуются от каталога).
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options.open(path)?.write_all(contents.as_bytes())
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url без `=` (RFC 4648 §5) — для ссылки в QR.
pub fn base64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, &b)| acc | (u32::from(b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(BASE64URL[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

pub fn base64url_decode(text: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = BASE64URL.iter().position(|&x| x == c).with_context(|| format!("символ {:?} не из base64url", c as char))?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// Ссылка сопряжения: `homeproxy://pair?d=<base64url(PairingBundle)>`.
pub fn pairing_uri(bundle: &PairingBundle) -> String {
    format!("{PAIRING_PREFIX}{}", base64url_encode(&bundle.encode_to_vec()))
}

pub fn parse_pairing_uri(uri: &str) -> Result<PairingBundle> {
    let data = uri.strip_prefix(PAIRING_PREFIX).context("не ссылка сопряжения homeproxy://pair")?;
    Ok(PairingBundle::decode(base64url_decode(data)?.as_slice())?)
}

/// Соединение с `hp-server` после успешного `Hello`.
pub struct Client {
    stream: TcpStream,
}

impl Client {
    pub async fn connect(addr: SocketAddr, token: &str) -> Result<(Self, Welcome)> {
        let mut stream = TcpStream::connect(addr).await.with_context(|| format!("нет связи со службой {addr}"))?;
        stream.set_nodelay(true)?;
        let hello = message(Body::Hello(Hello { token: token.to_string(), version: PROTOCOL_VERSION }));
        write_frame(&mut stream, &hello).await?;
        match read_frame(&mut stream).await? {
            Some(ControlMessage { body: Some(Body::Welcome(welcome)) }) => Ok((Self { stream }, welcome)),
            Some(ControlMessage { body: Some(Body::Error(e)) }) => anyhow::bail!("служба отказала: {}", e.message),
            None => anyhow::bail!("служба закрыла соединение (неверный токен?)"),
            Some(other) => anyhow::bail!("неожиданный ответ на Hello: {other:?}"),
        }
    }

    pub async fn send(&mut self, body: Body) -> Result<()> {
        write_frame(&mut self.stream, &message(body)).await
    }

    /// Следующее сообщение службы; ошибка — если соединение закрыто.
    pub async fn recv(&mut self) -> Result<Body> {
        let message = read_frame(&mut self.stream).await?.context("служба закрыла соединение")?;
        message.body.context("пустое сообщение")
    }

    /// Запрос — ответ; `Error` службы становится ошибкой.
    pub async fn request(&mut self, body: Body) -> Result<Body> {
        self.send(body).await?;
        match self.recv().await? {
            Body::Error(e) => anyhow::bail!("{}", e.message),
            reply => Ok(reply),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_roundtrip_all_lengths() {
        let data: Vec<u8> = (0..=255u8).collect();
        for n in 0..40 {
            let encoded = base64url_encode(&data[..n]);
            assert!(!encoded.contains('='));
            assert_eq!(base64url_decode(&encoded).unwrap(), &data[..n], "длина {n}");
        }
        assert_eq!(base64url_encode(b"\xfb\xff"), "-_8");
        assert!(base64url_decode("a+b").is_err());
    }

    #[test]
    fn pairing_uri_roundtrip() {
        let bundle = PairingBundle {
            version: 1,
            pc_guid: "00000000-0000-4000-8000-000000000001".into(),
            phone_guid: "00000000-0000-4000-8000-000000000002".into(),
            stun: "203.0.113.10:3499".into(),
            mqtt: "203.0.113.10:8883".into(),
            mqtt_ca_pem: b"-----BEGIN CERTIFICATE-----".to_vec(),
            expires_unix: 1_900_000_000,
        };
        let uri = pairing_uri(&bundle);
        assert!(uri.starts_with(PAIRING_PREFIX));
        assert_eq!(parse_pairing_uri(&uri).unwrap(), bundle);
    }

    #[test]
    fn tokens() {
        assert!(token_matches("abc", "abc"));
        assert!(!token_matches("abc", "abd"));
        assert!(!token_matches("abc", "abcd"));
        assert!(!token_matches("abc", ""));
        let token = new_token();
        assert_eq!(token.len(), 64);
        assert_ne!(token, new_token());
    }

    #[tokio::test]
    async fn frames_keep_boundaries() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &message(Body::GetStatus(proto::GetStatus {}))).await.unwrap();
        write_frame(&mut a, &message(Body::Subscribe(proto::Subscribe { interval_ms: 1000 }))).await.unwrap();
        drop(a);
        assert!(matches!(read_frame(&mut b).await.unwrap().unwrap().body, Some(Body::GetStatus(_))));
        assert!(matches!(read_frame(&mut b).await.unwrap().unwrap().body, Some(Body::Subscribe(s)) if s.interval_ms == 1000));
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }
}
