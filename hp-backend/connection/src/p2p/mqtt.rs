//! MQTT-рандеву (P2P): точка встречи, где два узла находят друг друга, прежде чем связаться напрямую.
//! Запись каждой дыры публикуется на топик `home-proxy/rendezvous/{моё имя}/{номер дыры}`, а
//! слушаем мы все дыры пира по wildcard `home-proxy/rendezvous/{имя пира}/+`. MQTT нужен только пока
//! живых дыр нет (первая): остальные дыры договариваются по уже пробитым (виртуал-брокер, `p2p`).
//! Имя — первая группа GUID (`auth::peer_name`): полный GUID — секрет пары, на брокер он не
//! попадает. Запись подписана ключом из секрета пары (`rendezvous`): чужую, подложенную на брокер
//! под нашим именем, пир не примет.
//!
//! Протокол — MQTT 5: у публикации выставлен `message_expiry_interval`, так что брокер сам удаляет
//! протухшую регистрацию (в MQTT 3.1.1 TTL нет, и запись висела бы вечно).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use prost::Message as _;
use rumqttc::v5::mqttbytes::v5::{Packet, PublishProperties};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use rumqttc::{TlsConfiguration, Transport};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::auth::{peer_name, PairSecret};
use crate::label::Label;
use crate::rendezvous::{decode_peer_session, our_record, PeerRegistration, PeerSession};

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




/// Публикатор наших слотов. Держит MQTT-клиент и фоновую задачу опроса; при
/// дропе задача останавливается, соединение с брокером закрывается.
pub struct Registrar {
    label: Label,
    client: AsyncClient,
    my_peer_id: Uuid,
    pair: PairSecret,
    last_peer: Arc<Mutex<Option<PeerRegistration>>>,
    _poll_task: AbortOnDrop,
}

/// Сколько пар (дыра, сессия) пира помним для отсечения повторов.
const SEEN_LIMIT: usize = 4096;

fn slot_topic(peer_id: Uuid, slot: crate::multilink::SlotId) -> String {
    format!("home-proxy/rendezvous/{}/{slot}", peer_name(&peer_id))
}

fn peer_wildcard(peer_id: Uuid) -> String {
    format!("home-proxy/rendezvous/{}/+", peer_name(&peer_id))
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
    let pair = PairSecret::new(my_peer_id, peer_id);
    let poll_pair = pair.clone();

    let (client, mut event_loop) = AsyncClient::new(mqtt_options, MQTT_REQUEST_CHANNEL_CAPACITY);
    client
        .subscribe(peer_wildcard(peer_id), QoS::AtLeastOnce)
        .await
        .context("не удалось поставить подписку на слоты пира")?;

    let (tx, rx) = mpsc::channel(PEER_SESSION_CHANNEL_CAPACITY);

    let last_peer = Arc::new(Mutex::new(None));
    let poll_last_peer = last_peer.clone();
    let poll_label = label.clone();
    let resubscribe_client = client.clone();
    let poll_task = tokio::spawn(async move {
        let label = poll_label;
        let mut seen: HashSet<(crate::multilink::SlotId, Uuid)> = HashSet::new();
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
            let session = match decode_peer_session(publish.payload.as_ref(), peer_id, &poll_pair) {
                Ok(Some((session, registered_at_unix_ms))) => {
                    *poll_last_peer.lock().unwrap() = Some(PeerRegistration { addr: session.addr, registered_at_unix_ms });
                    session
                }
                Ok(None) => continue,
                Err(e) => {
                    log::warn!("{label}некорректная запись пира на брокере: {e:#}");
                    continue;
                }
            };
            // Номера дыр динамические и не повторяются: чтобы набор не рос вечно, при переполнении
            // забываем старое (повторная выдача давней записи безвредна — пробив по ней не пройдёт).
            if seen.len() >= SEEN_LIMIT {
                seen.clear();
            }
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
        pair,
        last_peer,
        _poll_task: AbortOnDrop(poll_task),
    };
    Ok((registrar, rx))
}

/// Параметры подключения к брокеру: TLS с единственным доверенным корнем
/// (`mqtt_ca_pem`, адрес брокера проверяется по SAN сертификата) и
/// идентичность, по которой ACL брокера пускает нас только к нужным топикам:
/// `client_id` — наше имя (можно писать только в свои слоты), `username` — имя
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
        peer_name(&my_peer_id),
        mqtt_addr.ip().to_string(),
        mqtt_addr.port(),
    );
    options.set_keep_alive(MQTT_KEEP_ALIVE);
    options.set_credentials(peer_name(&peer_id), "");
    options.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca: mqtt_ca_pem,
        alpn: None,
        client_auth: None,
    }));
    Ok(options)
}





impl Registrar {
    /// Последняя запись пира, пришедшая с брокера; `None` — ни одной не было.
    pub fn last_peer_registration(&self) -> Option<PeerRegistration> {
        *self.last_peer.lock().unwrap()
    }

    /// Публикует (или обновляет) подписанную регистрацию нашего слота: адрес, `session_id` и
    /// TTL. Retained — чтобы пир, подписавшийся позже, тоже увидел.
    pub async fn publish_slot(&self, slot: crate::multilink::SlotId, session_id: Uuid, endpoints: &[SocketAddr]) -> Result<()> {
        anyhow::ensure!(!endpoints.is_empty(), "нет ни одного внешнего адреса для публикации");
        let payload =
            our_record(&self.pair, self.my_peer_id, slot, session_id, endpoints, unix_ms_now()).encode_to_vec();

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
    pub async fn clear_slot(&self, slot: crate::multilink::SlotId) -> Result<()> {
        self.client
            .publish(slot_topic(self.my_peer_id, slot), QoS::AtLeastOnce, true, Vec::new())
            .await
            .context("не удалось стереть регистрацию слота")
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (Uuid, Uuid, PairSecret) {
        let (me, peer) = (Uuid::new_v4(), Uuid::new_v4());
        (me, peer, PairSecret::new(me, peer))
    }

    #[test]
    fn mqtt_identity_uses_names_matching_broker_acl() {
        let (me, peer, _) = ids();
        let ca = b"-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n".to_vec();

        let options = mqtt_options("203.0.113.9:8883".parse().unwrap(), ca, me, peer).unwrap();

        assert_eq!(options.client_id(), peer_name(&me));
        assert_eq!(options.credentials(), Some((peer_name(&peer), String::new())));
        assert!(matches!(options.transport(), Transport::Tls(_)));
        assert_eq!(slot_topic(me, 4), format!("home-proxy/rendezvous/{}/4", peer_name(&me)));
    }

    #[test]
    fn non_pem_ca_is_rejected() {
        let addr = "203.0.113.9:8883".parse().unwrap();
        assert!(mqtt_options(addr, b"not a cert".to_vec(), Uuid::new_v4(), Uuid::new_v4()).is_err());
    }
}
