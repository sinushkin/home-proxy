//! Протокол управления (`hp-control`): TCP на `CONTROL_ADDR` (по умолчанию 127.0.0.1:47001),
//! первое сообщение — `Hello` с токеном из `control.token`. Дальше — запросы статуса,
//! подписка (статус раз в `interval_ms`), сопряжение и удаление телефона.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use hp_control::proto::control_message::Body;
use hp_control::proto::{self, ControlMessage};
use hp_control::{message, read_frame, token_matches, write_frame, PROTOCOL_VERSION};
use hp_tun::device::PacketDevice;
use tokio::net::{TcpListener, TcpStream};

use crate::service::Service;

/// Сколько ждать `Hello` от нового соединения.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Чаще этого статус по подписке не шлём.
const MIN_INTERVAL: Duration = Duration::from_millis(200);

pub async fn serve<D: PacketDevice>(listener: TcpListener, token: Arc<String>, service: Arc<Service<D>>) {
    loop {
        let (stream, from) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("управление: accept: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let (token, service) = (token.clone(), service.clone());
        tokio::spawn(async move {
            if let Err(e) = connection(stream, from, &token, &service).await {
                log::debug!("управление: {from}: {e:#}");
            }
        });
    }
}

async fn connection<D: PacketDevice>(mut stream: TcpStream, from: SocketAddr, token: &str, service: &Service<D>) -> Result<()> {
    stream.set_nodelay(true)?;
    let hello = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut stream)).await;
    let Ok(Ok(Some(ControlMessage { body: Some(Body::Hello(hello)) }))) = hello else {
        anyhow::bail!("нет Hello");
    };
    if !token_matches(token, &hello.token) {
        // Токен не пишем в лог ни свой, ни чужой.
        log::warn!("управление: {from}: неверный токен");
        return Ok(());
    }
    if hello.version != PROTOCOL_VERSION {
        let text = format!("версия протокола {} не поддерживается (служба — {PROTOCOL_VERSION})", hello.version);
        write_frame(&mut stream, &message(Body::Error(proto::Error { message: text }))).await?;
        return Ok(());
    }
    let welcome = proto::Welcome { version: PROTOCOL_VERSION, service: "hp-server".into(), build: env!("CARGO_PKG_VERSION").into() };
    write_frame(&mut stream, &message(Body::Welcome(welcome))).await?;
    log::debug!("управление: {from}: подключён");

    let (mut reader, mut writer) = stream.into_split();
    let mut subscription: Option<tokio::time::Interval> = None;
    loop {
        let incoming = tokio::select! {
            frame = read_frame(&mut reader) => frame?,
            _ = async { subscription.as_mut().expect("есть подписка").tick().await }, if subscription.is_some() => {
                write_frame(&mut writer, &message(Body::Status(service.status()))).await?;
                continue;
            }
        };
        let Some(ControlMessage { body: Some(body) }) = incoming else { return Ok(()) };
        let reply = match body {
            Body::GetStatus(_) => Body::Status(service.status()),
            Body::Subscribe(s) => {
                subscription = (s.interval_ms > 0).then(|| {
                    let mut interval = tokio::time::interval(Duration::from_millis(u64::from(s.interval_ms)).max(MIN_INTERVAL));
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    interval
                });
                // Первый тик подписки сработает сразу и пришлёт статус.
                continue;
            }
            Body::CreatePairing(_) => match service.create_pairing().await {
                Ok(pairing) => Body::Pairing(pairing),
                Err(e) => Body::Error(proto::Error { message: format!("{e:#}") }),
            },
            Body::RemovePeer(r) => match service.remove_peer(&r.name) {
                Ok(()) => Body::Done(proto::Done {}),
                Err(e) => Body::Error(proto::Error { message: format!("{e:#}") }),
            },
            // Без Debug-вывода запроса: повторный Hello несёт токен.
            _ => Body::Error(proto::Error { message: "запрос не поддерживается".into() }),
        };
        write_frame(&mut writer, &message(reply)).await?;
    }
}
