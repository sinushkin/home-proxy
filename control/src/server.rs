//! Сервер протокола управления — общий для `hp-server` и `hp-router`: служба реализует
//! `Controlled`, `serve` принимает соединения, проводит рукопожатие и отвечает на запросы.
//!
//! Ключ читается из файла на каждое новое соединение: новая строка подключения (LuCI,
//! `--new-connection-string`) действует сразу, без перезапуска службы.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::net::{TcpListener, TcpStream};

use crate::proto::control_message::Body;
use crate::proto::{self, ControlMessage, Pairing, Status};
use crate::{load_or_create_key, message, secure, PROTOCOL_VERSION};

/// Сколько ждать рукопожатия и `Hello` от нового соединения.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Чаще этого статус по подписке не шлём.
const MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Что служба умеет по протоколу управления.
pub trait Controlled: Send + Sync + 'static {
    /// Имя службы для `Welcome` (`hp-server`, `hp-router`).
    fn service(&self) -> &'static str;
    fn status(&self) -> Status;
    fn create_pairing(&self) -> impl Future<Output = Result<Pairing>> + Send;
    fn remove_peer(&self, name: &str) -> Result<()>;
}

pub async fn serve<C: Controlled>(listener: TcpListener, key_file: PathBuf, controlled: Arc<C>) {
    loop {
        let (stream, from) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("управление: accept: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let key = match load_or_create_key(&key_file) {
            Ok(key) => key,
            Err(e) => {
                log::warn!("управление: {e:#}");
                continue;
            }
        };
        let controlled = controlled.clone();
        tokio::spawn(async move {
            if let Err(e) = connection(stream, from, &key, controlled.as_ref()).await {
                log::debug!("управление: {from}: {e:#}");
            }
        });
    }
}

async fn connection<C: Controlled>(stream: TcpStream, from: SocketAddr, key: &str, controlled: &C) -> Result<()> {
    stream.set_nodelay(true)?;
    let (reader, writer) = stream.into_split();
    let opened = tokio::time::timeout(HELLO_TIMEOUT, async {
        let (mut reader, writer) = secure::server(reader, writer, key).await?;
        let hello = reader.recv().await;
        anyhow::Ok((reader, writer, hello))
    })
    .await;
    let Ok(Ok((mut reader, mut writer, hello))) = opened else { anyhow::bail!("нет рукопожатия") };
    let hello = match hello {
        Ok(Some(ControlMessage { body: Some(Body::Hello(hello)) })) => hello,
        Err(_) => {
            // Ключ у клиента другой (устаревшая строка подключения) — сам ключ в лог не пишем.
            log::warn!("управление: {from}: чужой ключ — строка подключения устарела?");
            return Ok(());
        }
        _ => anyhow::bail!("нет Hello"),
    };
    if hello.version != PROTOCOL_VERSION {
        let text = format!("версия протокола {} не поддерживается (служба — {PROTOCOL_VERSION})", hello.version);
        writer.send(&message(Body::Error(proto::Error { message: text }))).await?;
        return Ok(());
    }
    let welcome = proto::Welcome { version: PROTOCOL_VERSION, service: controlled.service().into(), build: env!("CARGO_PKG_VERSION").into() };
    writer.send(&message(Body::Welcome(welcome))).await?;
    log::debug!("управление: {from}: подключён");

    let mut subscription: Option<tokio::time::Interval> = None;
    loop {
        let incoming = tokio::select! {
            frame = reader.recv() => frame?,
            _ = async { subscription.as_mut().expect("есть подписка").tick().await }, if subscription.is_some() => {
                writer.send(&message(Body::Status(controlled.status()))).await?;
                continue;
            }
        };
        let Some(ControlMessage { body: Some(body) }) = incoming else { return Ok(()) };
        let reply = match body {
            Body::GetStatus(_) => Body::Status(controlled.status()),
            Body::Subscribe(s) => {
                subscription = (s.interval_ms > 0).then(|| {
                    let mut interval = tokio::time::interval(Duration::from_millis(u64::from(s.interval_ms)).max(MIN_INTERVAL));
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    interval
                });
                // Первый тик подписки сработает сразу и пришлёт статус.
                continue;
            }
            Body::CreatePairing(_) => match controlled.create_pairing().await {
                Ok(pairing) => Body::Pairing(pairing),
                Err(e) => Body::Error(proto::Error { message: format!("{e:#}") }),
            },
            Body::RemovePeer(r) => match controlled.remove_peer(&r.name) {
                Ok(()) => Body::Done(proto::Done {}),
                Err(e) => Body::Error(proto::Error { message: format!("{e:#}") }),
            },
            _ => Body::Error(proto::Error { message: "запрос не поддерживается".into() }),
        };
        writer.send(&message(reply)).await?;
    }
}
