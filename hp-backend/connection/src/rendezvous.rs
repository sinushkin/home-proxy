//! «Rendezvous» — французское слово (`rendez-vous`, буквально «явитесь»,
//! «представьте себя»). По-русски: «встреча», «место встречи» —
//! договорённость двух сторон встретиться в заранее известном месте.
//!
//! В сетевом смысле — точка встречи, где два узла находят друг друга, прежде
//! чем связаться напрямую. У нас эту роль играет MQTT-брокер.
//!
//! Слотовая модель. Каждая дыра — это отдельный «слот» (0..N-1): свой сокет,
//! свой STUN-эндпоинт, свой `session_id`. Каждый слот публикуется на топик
//! `home-proxy/rendezvous/{my_peer_id}/{slot}`, а слушаем мы все слоты пира
//! по wildcard `home-proxy/rendezvous/{peer_id}/+`. Слот k у нас пробивается
//! только к слоту k пира.
//!
//! Протокол — MQTT 5: у публикации выставлен `message_expiry_interval`, так
//! что брокер сам удаляет протухшую регистрацию (в MQTT 3.1.1 TTL нет, и
//! запись висела бы вечно). Ручная проверка «не старше минуты» больше не
//! нужна.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use prost::Message as _;
use rumqttc::v5::mqttbytes::v5::{Packet, PublishProperties};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use rumqttc::{TlsConfiguration, Transport};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::codec::XorKey;
use crate::label::Label;
use crate::proto::{Endpoint, Rendezvous};

/// Время жизни регистрации на брокере (MQTT 5 `message_expiry_interval`).
/// Пробиваем чуть дольше (см. менеджер), чтобы окна соседних регистраций
/// перекрывались.
pub const REGISTRATION_TTL: Duration = Duration::from_secs(60);

/// MQTT keep-alive TCP-соединения с брокером: если по нему давно ничего не
/// шлём, клиент отправляет PINGREQ, а брокер отключает клиента после
/// 1,5 × этого значения тишины. К keep-alive UDP-канала между пирами
/// отношения не имеет.
const MQTT_KEEP_ALIVE: Duration = Duration::from_secs(20);

/// Ёмкость канала запросов между `AsyncClient` и `EventLoop`. `publish`/
/// `subscribe` лишь кладут запрос сюда, а в сеть его выносит `event_loop.poll()`
/// (в нашем случае — фоновая задача). Минимум для корректности — 2 (подписка
/// уходит до первого `poll`); 16 — с запасом.
const MQTT_REQUEST_CHANNEL_CAPACITY: usize = 16;

/// Пауза между попытками переподключения к брокеру.
const MQTT_RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Сколько записей пира буферизуем, пока менеджер их разбирает.
const PEER_SESSION_CHANNEL_CAPACITY: usize = 64;

/// Абортит задачу при дропе, чтобы опрос брокера не пережил `Registrar`.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Запись одного слота пира, полученная с брокера.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerSession {
    pub slot: u8,
    pub session_id: Uuid,
    pub addr: SocketAddr,
    /// Вектор, которым пир просит кодировать всё, что мы шлём ему по этой дыре.
    pub key: XorKey,
    /// Другие внешние адреса того же сокета (их видели другие STUN-серверы).
    pub extra: Vec<SocketAddr>,
}

impl PeerSession {
    /// Все адреса, по которым стучимся к пиру: основной, затем дополнительные.
    pub fn candidates(&self) -> Vec<SocketAddr> {
        let mut all = vec![self.addr];
        all.extend(self.extra.iter().copied().filter(|a| *a != self.addr));
        all
    }
}

/// Публикатор наших слотов. Держит MQTT-клиент и фоновую задачу опроса; при
/// дропе задача останавливается, соединение с брокером закрывается.
pub struct Registrar {
    label: Label,
    client: AsyncClient,
    my_peer_id: Uuid,
    _poll_task: AbortOnDrop,
}

fn slot_topic(peer_id: Uuid, slot: u8) -> String {
    format!("home-proxy/rendezvous/{peer_id}/{slot}")
}

fn peer_wildcard(peer_id: Uuid) -> String {
    format!("home-proxy/rendezvous/{peer_id}/+")
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Подключается к брокеру и подписывается на все слоты пира.
///
/// Возвращает `Registrar` (для публикации своих слотов) и приёмник записей
/// пира: как только на любом слоте пира появляется запись — retained, уже
/// лежащая на брокере, или новая публикация, — она приходит в приёмник.
/// Одна и та же сессия пира отдаётся один раз (дедуп по `(slot, session_id)`).
///
/// Фоновая задача опроса и публикации живут, пока жив `Registrar`. Если
/// TCP-соединение с брокером оборвётся, задача завершится, приёмник закроется
/// (менеджер увидит это как конец потока) — автопереподключения здесь нет.
pub async fn connect(
    label: Label,
    mqtt_addr: SocketAddr,
    mqtt_ca_pem: Vec<u8>,
    my_peer_id: Uuid,
    peer_id: Uuid,
) -> Result<(Registrar, mpsc::Receiver<PeerSession>)> {
    let mqtt_options = mqtt_options(mqtt_addr, mqtt_ca_pem, my_peer_id, peer_id)?;

    let (client, mut event_loop) = AsyncClient::new(mqtt_options, MQTT_REQUEST_CHANNEL_CAPACITY);
    client
        .subscribe(peer_wildcard(peer_id), QoS::AtLeastOnce)
        .await
        .context("не удалось поставить подписку на слоты пира")?;

    let (tx, rx) = mpsc::channel(PEER_SESSION_CHANNEL_CAPACITY);

    let poll_label = label.clone();
    let resubscribe_client = client.clone();
    let poll_task = tokio::spawn(async move {
        let label = poll_label;
        let mut seen: HashSet<(u8, Uuid)> = HashSet::new();
        let mut ever_connected = false;
        let mut failing = false;
        loop {
            let event = match event_loop.poll().await {
                Ok(event) => event,
                Err(e) => {
                    // Следующий poll() сам переподключается: телефон меняет сети,
                    // точка доступа засыпает — обрыв не должен убивать клиента.
                    if !failing {
                        log::warn!("{label}рандеву (MQTT): соединение потеряно: {e}; переподключаемся");
                        failing = true;
                    }
                    tokio::time::sleep(MQTT_RECONNECT_DELAY).await;
                    continue;
                }
            };
            let publish = match event {
                Event::Incoming(Packet::ConnAck(_)) => {
                    failing = false;
                    if ever_connected {
                        log::info!("{label}рандеву (MQTT): переподключено к {mqtt_addr}");
                        // Чистая сессия: подписку после переподключения ставим заново.
                        if let Err(e) = resubscribe_client.subscribe(peer_wildcard(peer_id), QoS::AtLeastOnce).await {
                            log::warn!("{label}рандеву (MQTT): не удалось подписаться заново: {e}");
                        }
                    } else {
                        log::info!("{label}рандеву (MQTT): подключено к {mqtt_addr}");
                    }
                    ever_connected = true;
                    continue;
                }
                Event::Incoming(Packet::SubAck(_)) => {
                    log::debug!("{label}рандеву (MQTT): подписка на слоты пира подтверждена");
                    continue;
                }
                Event::Incoming(Packet::Publish(publish)) => publish,
                _ => continue,
            };
            let session = match decode_peer_session(publish.payload.as_ref(), peer_id) {
                Ok(Some(session)) => session,
                Ok(None) => continue,
                Err(e) => {
                    log::warn!("{label}некорректная запись пира на брокере: {e:#}");
                    continue;
                }
            };
            if !seen.insert((session.slot, session.session_id)) {
                continue; // ту же сессию уже отдавали
            }
            log::debug!("{label}рандеву (MQTT): запись пира, слот {}, адрес {}", session.slot, session.addr);
            if tx.send(session).await.is_err() {
                return; // менеджер больше не слушает
            }
        }
    });

    let registrar = Registrar {
        label,
        client,
        my_peer_id,
        _poll_task: AbortOnDrop(poll_task),
    };
    Ok((registrar, rx))
}

/// Параметры подключения к брокеру: TLS с единственным доверенным корнем
/// (`mqtt_ca_pem`, адрес брокера проверяется по SAN сертификата) и
/// идентичность, по которой ACL брокера пускает нас только к нужным топикам:
/// `client_id` — наш GUID (можно писать только в свои слоты), `username` — GUID
/// искомого пира (можно читать только его слоты). Пароль брокер не проверяет.
fn mqtt_options(
    mqtt_addr: SocketAddr,
    mqtt_ca_pem: Vec<u8>,
    my_peer_id: Uuid,
    peer_id: Uuid,
) -> Result<MqttOptions> {
    anyhow::ensure!(
        String::from_utf8_lossy(&mqtt_ca_pem).contains("-----BEGIN CERTIFICATE-----"),
        "CA-сертификат брокера не похож на PEM"
    );
    let mut options = MqttOptions::new(
        my_peer_id.to_string(),
        mqtt_addr.ip().to_string(),
        mqtt_addr.port(),
    );
    options.set_keep_alive(MQTT_KEEP_ALIVE);
    options.set_credentials(peer_id.to_string(), "");
    options.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca: mqtt_ca_pem,
        alpn: None,
        client_auth: None,
    }));
    Ok(options)
}

/// Запись `Rendezvous` о нашем слоте. `endpoints` — внешние адреса сокета, какими
/// его увидели STUN-серверы: первый основной, остальные уходят как дополнительные.
pub fn our_record(
    my_peer_id: Uuid,
    slot: u8,
    session_id: Uuid,
    endpoints: &[SocketAddr],
    key: XorKey,
    registered_at_unix_ms: u64,
) -> Rendezvous {
    let primary = endpoints.first().copied().unwrap_or(SocketAddr::from(([0, 0, 0, 0], 0)));
    Rendezvous {
        public_ip: primary.ip().to_string(),
        public_port: u32::from(primary.port()),
        peer_id: my_peer_id.to_string(),
        registered_at_unix_ms,
        session_id: session_id.to_string(),
        slot: u32::from(slot),
        key: key.to_vec(),
        extra_endpoints: endpoints
            .iter()
            .skip(1)
            .filter(|a| **a != primary)
            .map(|a| Endpoint { ip: a.ip().to_string(), port: u32::from(a.port()) })
            .collect(),
    }
}

/// Преобразует запись `Rendezvous` в `PeerSession`. Используется и для записей
/// из MQTT, и для тех, что пришли напрямую по дыре (виртуал-брокер).
pub fn peer_session_from(r: &Rendezvous) -> Result<PeerSession> {
    Ok(PeerSession {
        slot: u8::try_from(r.slot).context("slot вне диапазона u8")?,
        session_id: r.session_id.parse().context("некорректный session_id")?,
        addr: SocketAddr::new(
            r.public_ip.parse().context("некорректный public_ip")?,
            r.public_port as u16,
        ),
        key: r
            .key
            .as_slice()
            .try_into()
            .context("key должен быть ровно 16 байт")?,
        extra: r
            .extra_endpoints
            .iter()
            .filter_map(|e| {
                let port = u16::try_from(e.port).ok().filter(|p| *p != 0)?;
                Some(SocketAddr::new(e.ip.parse().ok()?, port))
            })
            .collect(),
    })
}

/// Разбирает payload записи пира из MQTT. `Ok(None)` — запись не про этого
/// пира (лишний топик), её просто пропускаем.
fn decode_peer_session(payload: &[u8], peer_id: Uuid) -> Result<Option<PeerSession>> {
    let r = Rendezvous::decode(payload).context("не удалось разобрать Rendezvous")?;
    if r.peer_id != peer_id.to_string() {
        return Ok(None);
    }
    Ok(Some(peer_session_from(&r)?))
}

impl Registrar {
    /// Публикует (или обновляет) регистрацию нашего слота: адрес, `session_id`,
    /// XOR-вектор и TTL. Retained — чтобы пир, подписавшийся позже, тоже увидел.
    pub async fn publish_slot(
        &self,
        slot: u8,
        session_id: Uuid,
        endpoints: &[SocketAddr],
        key: XorKey,
    ) -> Result<()> {
        anyhow::ensure!(!endpoints.is_empty(), "нет ни одного внешнего адреса для публикации");
        let payload =
            our_record(self.my_peer_id, slot, session_id, endpoints, key, unix_ms_now()).encode_to_vec();

        let properties = PublishProperties {
            message_expiry_interval: Some(REGISTRATION_TTL.as_secs() as u32),
            ..Default::default()
        };

        log::debug!("{}рандеву (MQTT): публикуем слот {slot} ({endpoints:?})", self.label);
        self.client
            .publish_with_properties(
                slot_topic(self.my_peer_id, slot),
                QoS::AtLeastOnce,
                true,
                payload,
                properties,
            )
            .await
            .context("не удалось опубликовать регистрацию слота")
    }

    /// Стирает retained-запись слота (пустой payload с retain).
    pub async fn clear_slot(&self, slot: u8) -> Result<()> {
        self.client
            .publish(slot_topic(self.my_peer_id, slot), QoS::AtLeastOnce, true, Vec::new())
            .await
            .context("не удалось стереть регистрацию слота")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_session_decodes_from_own_payload() {
        let session_id = Uuid::new_v4();
        let peer_id = Uuid::new_v4();
        let payload = Rendezvous {
            public_ip: "203.0.113.7".to_string(),
            public_port: 40000,
            peer_id: peer_id.to_string(),
            registered_at_unix_ms: 0,
            session_id: session_id.to_string(),
            slot: 3,
            key: vec![7; 16],
            extra_endpoints: vec![],
        }
        .encode_to_vec();

        let got = decode_peer_session(&payload, peer_id).unwrap().unwrap();
        assert_eq!(
            got,
            PeerSession {
                slot: 3,
                session_id,
                addr: "203.0.113.7:40000".parse().unwrap(),
                key: [7; 16],
                extra: vec![],
            }
        );
    }

    #[test]
    fn key_of_wrong_length_is_rejected() {
        let peer_id = Uuid::new_v4();
        for len in [0, 15, 17] {
            let payload = Rendezvous {
                public_ip: "203.0.113.7".to_string(),
                public_port: 40000,
                peer_id: peer_id.to_string(),
                registered_at_unix_ms: 0,
                session_id: Uuid::new_v4().to_string(),
                slot: 0,
                key: vec![1; len],
                extra_endpoints: vec![],
            }
            .encode_to_vec();

            assert!(decode_peer_session(&payload, peer_id).is_err(), "длина {len}");
        }
    }

    #[test]
    fn mqtt_identity_matches_broker_acl() {
        let (me, peer) = (Uuid::new_v4(), Uuid::new_v4());
        let ca = b"-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n".to_vec();

        let options = mqtt_options("203.0.113.9:8883".parse().unwrap(), ca, me, peer).unwrap();

        assert_eq!(options.client_id(), me.to_string());
        assert_eq!(options.credentials(), Some((peer.to_string(), String::new())));
        assert!(matches!(options.transport(), Transport::Tls(_)));
    }

    #[test]
    fn non_pem_ca_is_rejected() {
        let addr = "203.0.113.9:8883".parse().unwrap();
        assert!(mqtt_options(addr, b"not a cert".to_vec(), Uuid::new_v4(), Uuid::new_v4()).is_err());
    }

    #[test]
    fn extra_endpoints_round_trip_and_become_candidates() {
        let endpoints: Vec<SocketAddr> = vec![
            "203.0.113.7:40000".parse().unwrap(),
            "198.51.100.2:41000".parse().unwrap(),
            "203.0.113.7:40000".parse().unwrap(), // повтор основного отбрасывается
        ];
        let record = our_record(Uuid::new_v4(), 3, Uuid::new_v4(), &endpoints, [9; 16], 0);
        assert_eq!(record.extra_endpoints.len(), 1);

        let session = peer_session_from(&record).unwrap();
        assert_eq!(session.addr, endpoints[0]);
        assert_eq!(session.extra, vec![endpoints[1]]);
        assert_eq!(session.candidates(), vec![endpoints[0], endpoints[1]]);
    }

    #[test]
    fn broken_extra_endpoints_are_skipped() {
        let mut record = our_record(Uuid::new_v4(), 0, Uuid::new_v4(), &["203.0.113.7:1".parse().unwrap()], [0; 16], 0);
        record.extra_endpoints = vec![
            Endpoint { ip: "не-адрес".into(), port: 5 },
            Endpoint { ip: "198.51.100.2".into(), port: 0 },
            Endpoint { ip: "198.51.100.2".into(), port: 70000 },
            Endpoint { ip: "198.51.100.2".into(), port: 6 },
        ];
        assert_eq!(peer_session_from(&record).unwrap().extra, vec!["198.51.100.2:6".parse().unwrap()]);
    }

    #[test]
    fn peer_session_from_other_peer_is_ignored() {
        let payload = Rendezvous {
            public_ip: "203.0.113.7".to_string(),
            public_port: 40000,
            peer_id: Uuid::new_v4().to_string(),
            registered_at_unix_ms: 0,
            session_id: Uuid::new_v4().to_string(),
            slot: 0,
            key: vec![0; 16],
            extra_endpoints: vec![],
        }
        .encode_to_vec();

        assert!(decode_peer_session(&payload, Uuid::new_v4()).unwrap().is_none());
    }
}
