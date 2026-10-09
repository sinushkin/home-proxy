//! Протокол управления службой home-proxy (`hp-server`, `hp-router`): сообщения (`proto`),
//! защищённый канал (`secure`), сервер (`server`), клиент, строка подключения и ссылка
//! сопряжения для QR. Описание — `control/README.md`.
//!
//! **Строка подключения** — `homeproxy-control://<ip:порт>/<ключ>`: адрес службы и ключ канала
//! (32 случайных байта в base64url). Служба хранит ключ у себя (`control.key`, права 600) и
//! показывает строку по запросу (`hp-server --connection-string`, LuCI на роутере); трей
//! получает её от пользователя один раз и хранит в своих настройках. По сети ключ не
//! передаётся: из него выводятся ключи шифрования сессии.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

pub mod secure;
pub mod server;

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/hp.control.rs"));
}

use proto::control_message::Body;
use proto::{ControlMessage, Hello, PairingBundle, Welcome};

/// Версия протокола в `Hello`/`Welcome` (2 — канал шифруется ключом строки подключения).
pub const PROTOCOL_VERSION: u32 = 2;
/// Адрес управления по умолчанию: только loopback.
pub const DEFAULT_ADDR: &str = "127.0.0.1:47001";
/// Имя файла ключа рядом с настройками службы.
pub const KEY_FILE: &str = "control.key";
/// Самый длинный кадр: статус десятков пиров — единицы килобайт.
pub const MAX_FRAME: usize = 256 * 1024;
/// Схема ссылки сопряжения (QR и deep link Android).
pub const PAIRING_PREFIX: &str = "homeproxy://pair?d=";
/// Схема строки подключения трея к службе.
pub const CONNECTION_PREFIX: &str = "homeproxy-control://";

/// Сообщение с телом `body`.
pub fn message(body: Body) -> ControlMessage {
    ControlMessage { body: Some(body) }
}

/// Читает один кадр (`u32` BE длина + байты); `None` — соединение закрыто между кадрами.
pub async fn read_raw<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
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
    Ok(Some(buf))
}

pub async fn write_raw<W: AsyncWrite + Unpin>(writer: &mut W, body: &[u8]) -> Result<()> {
    anyhow::ensure!(body.len() <= MAX_FRAME, "кадр {} байт длиннее {MAX_FRAME}", body.len());
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    writer.write_all(&frame).await?;
    Ok(())
}

/// Новый ключ: 32 байта из CSPRNG ОС (через v4 UUID), base64url.
pub fn new_key() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64url_encode(&bytes)
}

/// Ключ из файла службы; если файла нет — создаёт новый (на Unix — с правами 600).
pub fn load_or_create_key(path: &Path) -> Result<String> {
    if let Ok(text) = std::fs::read_to_string(path) {
        let key = text.trim().to_string();
        anyhow::ensure!(!key.is_empty(), "файл ключа {} пуст", path.display());
        return Ok(key);
    }
    replace_key(path)
}

/// Новый ключ вместо прежнего: старые строки подключения перестают работать.
pub fn replace_key(path: &Path) -> Result<String> {
    let key = new_key();
    write_private(path, &format!("{key}\n")).with_context(|| format!("не удалось записать ключ {}", path.display()))?;
    Ok(key)
}

/// `homeproxy-control://<ip:порт>/<ключ>`.
pub fn connection_string(addr: SocketAddr, key: &str) -> String {
    format!("{CONNECTION_PREFIX}{addr}/{key}")
}

/// Адрес и ключ из строки подключения.
pub fn parse_connection_string(text: &str) -> Result<(SocketAddr, String)> {
    let rest = text.trim().strip_prefix(CONNECTION_PREFIX).context("строка подключения начинается с homeproxy-control://")?;
    let (addr, key) = rest.split_once('/').context("в строке подключения нет ключа")?;
    let addr = addr.parse().context("адрес службы в строке подключения: ожидается ip:порт")?;
    anyhow::ensure!(key.len() >= 16 && base64url_decode(key).is_ok(), "ключ в строке подключения повреждён");
    Ok((addr, key.to_string()))
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

/// Последняя регистрация пира на MQTT-брокере для людей: «регистрация 3 мин назад с
/// 203.0.113.7:40000»; `None` — поля пусты (не регистрировался или режим без MQTT). `now_unix_ms` —
/// наши часы, время в записи — по часам пира: небольшое расхождение показывается как «только что».
pub fn registration_text(peer: &proto::PeerStatus, now_unix_ms: u64) -> Option<String> {
    if peer.registered_at_unix_ms == 0 && peer.registered_addr.is_empty() {
        return None;
    }
    let age = now_unix_ms.saturating_sub(peer.registered_at_unix_ms) / 1000;
    let when = match age {
        0..5 => "только что".to_string(),
        5..120 => format!("{age} с назад"),
        120..7200 => format!("{} мин назад", age / 60),
        7200..172_800 => format!("{} ч назад", age / 3600),
        _ => format!("{} д назад", age / 86400),
    };
    Some(format!("регистрация {when} с {}", peer.registered_addr))
}

/// Возраст дыры для людей: «42 с», «3 м 05 с», «2 ч 07 м».
pub fn age_text(secs: u32) -> String {
    match secs {
        0..60 => format!("{secs} с"),
        60..3600 => format!("{} м {:02} с", secs / 60, secs % 60),
        _ => format!("{} ч {:02} м", secs / 3600, secs % 3600 / 60),
    }
}

/// Соединение со службой после рукопожатия и `Hello`.
pub struct Client {
    reader: secure::SealedReader<OwnedReadHalf>,
    writer: secure::SealedWriter<OwnedWriteHalf>,
}

impl Client {
    /// Подключение по строке подключения.
    pub async fn connect_string(connection: &str) -> Result<(Self, Welcome)> {
        let (addr, key) = parse_connection_string(connection)?;
        Self::connect(addr, &key).await
    }

    pub async fn connect(addr: SocketAddr, key: &str) -> Result<(Self, Welcome)> {
        let stream = tokio::time::timeout(std::time::Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow::anyhow!("служба {addr} не отвечает"))?
            .with_context(|| format!("нет связи со службой {addr}"))?;
        stream.set_nodelay(true)?;
        let (reader, writer) = stream.into_split();
        let (reader, writer) = secure::client(reader, writer, key).await?;
        let mut client = Self { reader, writer };
        client.send(Body::Hello(Hello { version: PROTOCOL_VERSION })).await?;
        match client.reader.recv().await {
            Ok(Some(ControlMessage { body: Some(Body::Welcome(welcome)) })) => Ok((client, welcome)),
            Ok(Some(ControlMessage { body: Some(Body::Error(e)) })) => anyhow::bail!("служба отказала: {}", e.message),
            Ok(None) | Err(_) => anyhow::bail!("служба не приняла ключ: строка подключения устарела или от другой службы"),
            Ok(Some(_)) => anyhow::bail!("неожиданный ответ на Hello"),
        }
    }

    pub async fn send(&mut self, body: Body) -> Result<()> {
        self.writer.send(&message(body)).await
    }

    /// Следующее сообщение службы; ошибка — если соединение закрыто.
    pub async fn recv(&mut self) -> Result<Body> {
        let message = self.reader.recv().await?.context("служба закрыла соединение")?;
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
    fn registration_text_shows_age_and_address() {
        let peer = |at, addr: &str| proto::PeerStatus { registered_at_unix_ms: at, registered_addr: addr.into(), ..Default::default() };
        assert_eq!(registration_text(&peer(0, ""), 1_000_000), None);
        let now = 1_700_000_000_000;
        assert_eq!(registration_text(&peer(now + 2000, "203.0.113.7:40000"), now).unwrap(), "регистрация только что с 203.0.113.7:40000");
        assert_eq!(registration_text(&peer(now - 42_000, "203.0.113.7:1"), now).unwrap(), "регистрация 42 с назад с 203.0.113.7:1");
        assert_eq!(registration_text(&peer(now - 600_000, "203.0.113.7:1"), now).unwrap(), "регистрация 10 мин назад с 203.0.113.7:1");
        assert_eq!(registration_text(&peer(now - 3 * 3_600_000, "203.0.113.7:1"), now).unwrap(), "регистрация 3 ч назад с 203.0.113.7:1");
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
    fn keys_and_connection_strings() {
        let key = new_key();
        assert_eq!(key.len(), 43);
        assert_ne!(key, new_key());
        let text = connection_string("192.168.1.1:47001".parse().unwrap(), &key);
        assert_eq!(text, format!("homeproxy-control://192.168.1.1:47001/{key}"));
        assert_eq!(parse_connection_string(&format!("  {text}\n")).unwrap(), ("192.168.1.1:47001".parse().unwrap(), key));
        assert!(parse_connection_string("192.168.1.1:47001/abc").is_err());
        assert!(parse_connection_string("homeproxy-control://192.168.1.1:47001").is_err());
        assert!(parse_connection_string("homeproxy-control://router:47001/aaaaaaaaaaaaaaaaaaaa").is_err());
    }

    #[tokio::test]
    async fn raw_frames_keep_boundaries() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_raw(&mut a, b"one").await.unwrap();
        write_raw(&mut a, b"second").await.unwrap();
        drop(a);
        assert_eq!(read_raw(&mut b).await.unwrap().unwrap(), b"one");
        assert_eq!(read_raw(&mut b).await.unwrap().unwrap(), b"second");
        assert!(read_raw(&mut b).await.unwrap().is_none());
    }

    #[test]
    fn hole_age_is_human_readable() {
        assert_eq!(age_text(0), "0 с");
        assert_eq!(age_text(42), "42 с");
        assert_eq!(age_text(60), "1 м 00 с");
        assert_eq!(age_text(185), "3 м 05 с");
        assert_eq!(age_text(3600), "1 ч 00 м");
        assert_eq!(age_text(7620), "2 ч 07 м");
    }
}
