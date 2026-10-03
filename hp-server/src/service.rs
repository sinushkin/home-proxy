//! Работающая служба: пиры со своими наборами дыр, раздача адресов, сопряжение новых телефонов
//! и снимок состояния для протокола управления (`control`).
//!
//! Пиры бывают трёх видов: из настроек (`MY_ID`/`PEER_ID`, `PEER_<n>_*`), сопряжённые через трей
//! (хранятся в `peers.state`) и ожидающий сопряжения — не больше одного: набор дыр уже
//! запущен, телефон ещё ни разу не подключался. Ожидающий становится постоянным при первой живой
//! дыре; если за время жизни пакета сопряжения телефон так и не пришёл — останавливается.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use connection::auth::peer_name;
use connection::multilink::{ConnState, Control, Discovery, MultiLink, MultiLinkOptions, TARGET_LINKS};
use connection::proto::{AddressAssign, AddressKind};
use hp_control::proto;
use hp_tun::device::PacketDevice;
use hp_tun::hub::Hub;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::addresses::{client_key, AddressBook};
use crate::{Mode, Peer};

/// Сколько живёт пакет сопряжения (и ожидающий телефон).
pub const PAIRING_TTL: Duration = Duration::from_secs(10 * 60);

/// Откуда пир.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// Из файла настроек: удалить можно только там.
    Settings,
    /// Из файла клиентов, который следит за собой на ходу (`vps-server`, `clients.txt`): удаляется
    /// вместе со строкой в файле.
    File,
    /// Сопряжён через трей, хранится в `peers.state`.
    Paired,
    /// Ждёт первого подключения до этого момента (unix-время).
    Pending(u64),
}

struct Entry {
    peer: Peer,
    name: String,
    link: Arc<MultiLink>,
    origin: Origin,
    _addresses: AbortOnDrop,
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Общее для всех пиров службы.
pub struct Service<D: PacketDevice> {
    hub: Arc<Hub<D>>,
    book: Arc<Mutex<AddressBook>>,
    dns: Arc<Vec<Vec<u8>>>,
    options: MultiLinkOptions,
    /// Способ встречи для новых телефонов (STUN + MQTT); `None` — сопряжение не поддерживается.
    pairing: Option<Discovery>,
    peers_file: Option<PathBuf>,
    entries: Mutex<Vec<Entry>>,
    started: Instant,
    mode: Mode,
    bind: Option<IpAddr>,
    /// hp-stats (фича `stats`, PLAN-ML.md): прикрепляется к каждому новому набору дыр (`add`).
    #[cfg(feature = "stats")]
    stats: Option<hp_stats::StatsHandle>,
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl<D: PacketDevice> Service<D> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hub: Hub<D>,
        book: AddressBook,
        dns: Vec<std::net::Ipv4Addr>,
        options: MultiLinkOptions,
        pairing: Option<Discovery>,
        peers_file: Option<PathBuf>,
        mode: Mode,
        bind: Option<IpAddr>,
        #[cfg(feature = "stats")] stats: Option<hp_stats::StatsHandle>,
    ) -> Self {
        Self {
            hub: Arc::new(hub),
            book: Arc::new(Mutex::new(book)),
            dns: Arc::new(dns.iter().map(|a| a.octets().to_vec()).collect()),
            options,
            pairing,
            peers_file,
            entries: Mutex::new(Vec::new()),
            started: Instant::now(),
            mode,
            bind,
            #[cfg(feature = "stats")]
            stats,
        }
    }

    pub fn hub(&self) -> &Hub<D> {
        &self.hub
    }

    /// Пиры из настроек (с их способом встречи).
    pub async fn add_configured(&self, peer: Peer, discovery: Discovery) -> Result<()> {
        self.add(peer, discovery, Origin::Settings).await
    }

    /// Пир из файла клиентов, добавленный во время работы.
    pub async fn add_from_file(&self, peer: Peer, discovery: Discovery) -> Result<()> {
        self.add(peer, discovery, Origin::File).await
    }

    /// Сопряжённые раньше пиры из `peers.state` (нужен способ встречи для сопряжения).
    pub async fn add_paired_from_file(&self) -> Result<()> {
        let (Some(path), Some(discovery)) = (self.peers_file.as_ref(), self.pairing.as_ref()) else { return Ok(()) };
        let Ok(text) = std::fs::read_to_string(path) else { return Ok(()) };
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let parsed = (parts.next().map(str::parse::<Uuid>), parts.next().map(str::parse::<Uuid>));
            let (Some(Ok(my_id)), Some(Ok(peer_id))) = parsed else {
                log::warn!("{}:{}: ожидается «GUID-службы GUID-телефона», строка пропущена", path.display(), n + 1);
                continue;
            };
            if self.entries.lock().unwrap().iter().any(|e| e.peer.my_id == my_id) {
                continue;
            }
            self.add(Peer { my_id, peer_id }, discovery.clone(), Origin::Paired).await?;
        }
        Ok(())
    }

    async fn add(&self, peer: Peer, discovery: Discovery, origin: Origin) -> Result<()> {
        let name = peer_name(&peer.peer_id);
        log::info!("сервер: я {} ищу пира {name}", peer_name(&peer.my_id));
        let (link, incoming) = MultiLink::start_discovery(&name, discovery, peer.my_id, peer.peer_id, self.options).await?;
        #[cfg(feature = "stats")]
        if let Some(stats) = &self.stats {
            link.attach_stats(stats.clone());
        }
        let link = Arc::new(link);
        self.hub.add_link(&link, incoming);
        let control = link.take_control().expect("приёмник служебных сообщений забираем один раз");
        let addresses = tokio::spawn(serve_addresses(link.clone(), control, self.hub.clone(), self.book.clone(), self.dns.clone()));
        self.entries.lock().unwrap().push(Entry { peer, name, link, origin, _addresses: AbortOnDrop(addresses) });
        Ok(())
    }

    /// Пакет сопряжения нового телефона. Пока прежний ожидающий телефон не подключился, пакет
    /// выдаётся для него же (с продлённым сроком): лишних наборов дыр не копится.
    pub async fn create_pairing(&self) -> Result<proto::Pairing> {
        let Some(Discovery::StunMqtt { stun_addrs, mqtt_addr, mqtt_ca_pem }) = self.pairing.clone() else {
            anyhow::bail!("сопряжение доступно только в режиме STUN + MQTT");
        };
        let expires = now_unix() + PAIRING_TTL.as_secs();
        let reused = {
            let mut entries = self.entries.lock().unwrap();
            entries.iter_mut().find(|e| matches!(e.origin, Origin::Pending(_))).map(|e| {
                e.origin = Origin::Pending(expires);
                e.peer
            })
        };
        let peer = match reused {
            Some(peer) => peer,
            None => {
                let peer = Peer { my_id: Uuid::new_v4(), peer_id: Uuid::new_v4() };
                let discovery = Discovery::StunMqtt { stun_addrs: stun_addrs.clone(), mqtt_addr, mqtt_ca_pem: mqtt_ca_pem.clone() };
                self.add(peer, discovery, Origin::Pending(expires)).await?;
                peer
            }
        };
        log::info!("сопряжение: ждём телефон {} до {expires} (unix)", peer_name(&peer.peer_id));
        Ok(pairing(peer, &stun_addrs, mqtt_addr, mqtt_ca_pem, expires))
    }

    /// Удаляет сопряжённого (или ожидающего) пира по имени.
    pub fn remove_peer(&self, name: &str) -> Result<()> {
        let removed = {
            let mut entries = self.entries.lock().unwrap();
            let index = entries.iter().position(|e| e.name == name).with_context(|| format!("нет пира {name}"))?;
            anyhow::ensure!(entries[index].origin != Origin::Settings, "пир {name} задан в файле настроек — удалите его там");
            entries.remove(index)
        };
        self.hub.remove_peer(removed.peer.peer_id);
        log::info!("пир {name} удалён");
        self.save_paired()?;
        Ok(())
    }

    /// Раз в секунду: ожидающий телефон подключился — сохранить; истёк — остановить.
    pub fn housekeeping(&self) {
        let now = now_unix();
        let mut promoted = false;
        {
            let mut entries = self.entries.lock().unwrap();
            entries.retain_mut(|e| {
                let Origin::Pending(expires) = e.origin else { return true };
                if e.link.live_count() > 0 {
                    log::info!("сопряжение: телефон {} подключился", e.name);
                    e.origin = Origin::Paired;
                    promoted = true;
                    true
                } else if now > expires {
                    log::info!("сопряжение: телефон {} не подключился, пакет истёк", e.name);
                    false
                } else {
                    true
                }
            });
        }
        if promoted && let Err(e) = self.save_paired() {
            log::warn!("не удалось сохранить сопряжённых пиров: {e:#}");
        }
    }

    fn save_paired(&self) -> Result<()> {
        let Some(path) = self.peers_file.as_ref() else { return Ok(()) };
        let mut text = String::from("# Сопряжённые телефоны: GUID службы и GUID телефона. Секрет пары — не публиковать.\n");
        for e in self.entries.lock().unwrap().iter().filter(|e| e.origin == Origin::Paired) {
            text.push_str(&format!("{} {}\n", e.peer.my_id, e.peer.peer_id));
        }
        hp_control::write_private(path, &text).with_context(|| format!("запись {}", path.display()))
    }

    /// Живые дыры по пирам — для лога.
    pub fn live_counts(&self) -> Vec<usize> {
        self.entries.lock().unwrap().iter().map(|e| e.link.live_count()).collect()
    }

    /// Снимок для протокола управления.
    pub fn status(&self) -> proto::Status {
        let (to_peers, ordered, from_peers, dropped) = self.hub.stats().snapshot();
        let entries = self.entries.lock().unwrap();
        let peers = entries
            .iter()
            .map(|e| {
                let status = e.link.status();
                proto::PeerStatus {
                    name: e.name.clone(),
                    state: match status.state {
                        ConnState::Rendezvous => "rendezvous",
                        ConnState::Punching => "punching",
                        ConnState::Connected(_) => "connected",
                    }
                    .into(),
                    live: status.holes.len() as u32,
                    target: TARGET_LINKS,
                    holes: status
                        .holes
                        .iter()
                        .map(|h| proto::HoleStatus {
                            slot: h.slot,
                            peer_addr: h.peer_addr.map(|a| a.to_string()).unwrap_or_default(),
                            sent: h.sent,
                            received: h.received,
                            loss_out: h.loss_out.unwrap_or(-1.0),
                            loss_in: h.loss_in.unwrap_or(-1.0),
                        })
                        .collect(),
                    addresses: self.hub.addresses_of(e.peer.peer_id).iter().map(|a| a.to_string()).collect(),
                    pending: matches!(e.origin, Origin::Pending(_)),
                    pending_expires_unix: match e.origin {
                        Origin::Pending(t) => t,
                        _ => 0,
                    },
                    removable: !matches!(e.origin, Origin::Settings | Origin::File),
                    kind: "phone".into(),
                    reorder_wait_ms: status.reorder_wait_ms,
                    registered_addr: status.peer_registration.map(|r| r.addr.to_string()).unwrap_or_default(),
                    registered_at_unix_ms: status.peer_registration.map_or(0, |r| r.registered_at_unix_ms),
                }
            })
            .collect();
        proto::Status {
            service: "hp-server".into(),
            uptime_s: self.started.elapsed().as_secs(),
            mode: match self.mode {
                Mode::Tun => "tun",
                Mode::Netstack => "netstack",
            }
            .into(),
            bind: self.bind.map(|ip| ip.to_string()).unwrap_or_default(),
            peers,
            traffic: Some(proto::Traffic { to_peers: to_peers.into(), ordered: ordered.into(), from_peers: from_peers.into(), dropped: dropped.into(), ..Default::default() }),
            pairing_supported: self.pairing.is_some(),
        }
    }
}

/// Пакет сопряжения: `peer.my_id` — GUID службы, `peer.peer_id` — телефона (общий для
/// `hp-server` и `hp-router`).
pub fn pairing(peer: Peer, stun_addrs: &[SocketAddr], mqtt_addr: SocketAddr, mqtt_ca_pem: Vec<u8>, expires: u64) -> proto::Pairing {
    let stun: Vec<String> = stun_addrs.iter().map(SocketAddr::to_string).collect();
    let bundle = proto::PairingBundle {
        version: 1,
        pc_guid: peer.my_id.to_string(),
        phone_guid: peer.peer_id.to_string(),
        stun: stun.join(","),
        mqtt: mqtt_addr.to_string(),
        mqtt_ca_pem,
        expires_unix: expires,
    };
    proto::Pairing { uri: hp_control::pairing_uri(&bundle), bundle: Some(bundle), name: peer_name(&peer.peer_id) }
}

/// Отвечает на запросы адреса пира `link` (и клиентов за ним): выдаёт адрес из книги и
/// закрепляет его в мосту.
async fn serve_addresses<D: PacketDevice>(
    link: Arc<MultiLink>,
    mut control: mpsc::Receiver<Control>,
    hub: Arc<Hub<D>>,
    book: Arc<Mutex<AddressBook>>,
    dns: Arc<Vec<Vec<u8>>>,
) {
    while let Some(message) = control.recv().await {
        let Control::AddressRequest(request) = message else { continue };
        let Ok(client) = request.client_id.map(u8::try_from).transpose() else {
            log::warn!("запрос адреса с client_id {:?} вне 0..=255", request.client_id);
            continue;
        };
        let kind = AddressKind::try_from(request.kind).unwrap_or(AddressKind::Host);
        let key = client_key(&peer_name(&link.peer_id()), client);
        let assigned = {
            let mut book = book.lock().unwrap();
            book.get_or_assign(&key, kind).map(|address| (address, book.prefix()))
        };
        match assigned {
            Ok((address, prefix)) => {
                hub.assign(address, &link, client);
                let reply = AddressAssign {
                    address: address.octets().to_vec(),
                    prefix: u32::from(prefix),
                    client_id: request.client_id,
                    dns: dns.as_ref().clone(),
                };
                let _ = link.send_control(Control::AddressAssign(reply)).await;
            }
            Err(e) => log::warn!("адрес для {key}: {e:#}"),
        }
    }
}

impl<D: PacketDevice> hp_control::server::Controlled for Service<D> {
    fn service(&self) -> &'static str {
        "hp-server"
    }

    fn status(&self) -> proto::Status {
        Service::status(self)
    }

    async fn create_pairing(&self) -> Result<proto::Pairing> {
        Service::create_pairing(self).await
    }

    fn remove_peer(&self, name: &str) -> Result<()> {
        Service::remove_peer(self, name)
    }
}
