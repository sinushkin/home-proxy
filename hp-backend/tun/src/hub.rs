//! Серверный мост: один TUN на много пиров (`MultiLink`) — телефоны напрямую, роутеры, хосты, а
//! за роутерами их телефоны (`client_id`). Работает на том, кто раздаёт адреса в туннеле (VPS,
//! домашний ПК).
//!
//! Маршрутизация — по таблице выданных адресов: адрес ↔ (пир, клиент за ним). Пакет из TUN уходит
//! по адресу назначения к своему пиру (клиенту за роутером — обёрнутым, со своими номерами
//! потоков). Пакет от пира пишется в TUN, только если его адрес источника выдан именно этому пиру
//! и клиенту: чужой адрес подставить нельзя. Сами адреса выдаёт вызывающий (`assign`).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use connection::multilink::{Incoming, MultiLink};
use connection::pool::PACKET_CAP;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::bridge::{send_routed, BridgeStats, Sequencer};
use crate::packet;
use crate::Tun;

/// Кто за адресом: пир (по GUID его набора дыр) и клиент за ним (телефон за роутером).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    pub peer: Uuid,
    pub client: Option<u8>,
}

struct Target {
    link: Arc<MultiLink>,
    client: Option<u8>,
    sequencer: Sequencer,
}

#[derive(Default)]
struct Routes {
    by_addr: HashMap<Ipv4Addr, Target>,
    by_owner: HashMap<Owner, Ipv4Addr>,
}

/// Запущенный серверный мост; дроп останавливает его задачи.
pub struct Hub {
    tun: Arc<Tun>,
    routes: Arc<Mutex<Routes>>,
    stats: Arc<BridgeStats>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Drop for Hub {
    fn drop(&mut self) {
        for task in self.tasks.lock().unwrap().iter() {
            task.abort();
        }
    }
}

impl Hub {
    pub fn start(tun: Tun) -> Self {
        let tun = Arc::new(tun);
        let routes = Arc::new(Mutex::new(Routes::default()));
        let stats = Arc::new(BridgeStats::default());
        let up = tokio::spawn(uplink(tun.clone(), routes.clone(), stats.clone()));
        Self { tun, routes, stats, tasks: Mutex::new(vec![up]) }
    }

    /// Подключает пира: всё, что от него приходит, пишется в TUN (с проверкой адреса источника).
    pub fn add_link(&self, link: &MultiLink, incoming: mpsc::Receiver<Incoming>) {
        let task = tokio::spawn(downlink(self.tun.clone(), link.peer_id(), incoming, self.routes.clone(), self.stats.clone()));
        self.tasks.lock().unwrap().push(task);
    }

    /// Закрепляет адрес за пиром `link` (и клиентом за ним): пакеты из TUN на этот адрес уходят
    /// туда. Прежние привязки адреса и владельца заменяются.
    pub fn assign(&self, address: Ipv4Addr, link: &Arc<MultiLink>, client: Option<u8>) {
        let owner = Owner { peer: link.peer_id(), client };
        let mut routes = self.routes.lock().unwrap();
        if routes.by_owner.get(&owner) == Some(&address) && routes.by_addr.contains_key(&address) {
            return;
        }
        if let Some(old) = routes.by_owner.insert(owner, address) {
            routes.by_addr.remove(&old);
        }
        if let Some(previous) = routes.by_addr.insert(address, Target { link: link.clone(), client, sequencer: Sequencer::default() }) {
            let previous_owner = Owner { peer: previous.link.peer_id(), client: previous.client };
            if previous_owner != owner {
                routes.by_owner.remove(&previous_owner);
            }
        }
        log::info!(
            "TUN: {address} — пир {}{}",
            owner.peer,
            client.map(|c| format!(", клиент {c}")).unwrap_or_default()
        );
    }

    /// Адрес, закреплённый за владельцем.
    pub fn address_of(&self, owner: Owner) -> Option<Ipv4Addr> {
        self.routes.lock().unwrap().by_owner.get(&owner).copied()
    }

    pub fn stats(&self) -> &BridgeStats {
        &self.stats
    }
}

async fn uplink(tun: Arc<Tun>, routes: Arc<Mutex<Routes>>, stats: Arc<BridgeStats>) {
    let mut buf = [0u8; PACKET_CAP];
    loop {
        let n = match tun.recv(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                log::warn!("TUN: ошибка чтения: {e}");
                return;
            }
        };
        let packet = &buf[..n];
        let info = packet::inspect(packet);
        let target = info.as_ref().and_then(|info| {
            let IpAddr::V4(dst) = info.dst else { return None };
            let mut routes = routes.lock().unwrap();
            let target = routes.by_addr.get_mut(&dst)?;
            let route = target.sequencer.route_inspected(n, Some(info));
            Some((target.link.clone(), target.client, route))
        });
        match target {
            Some((link, client, route)) => send_routed(&link, client, route, packet, &stats).await,
            None => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                log::trace!("TUN: пакет {n} байт — адрес назначения никому не выдан");
            }
        }
    }
}

async fn downlink(
    tun: Arc<Tun>,
    peer: Uuid,
    mut incoming: mpsc::Receiver<Incoming>,
    routes: Arc<Mutex<Routes>>,
    stats: Arc<BridgeStats>,
) {
    while let Some(packet) = incoming.recv().await {
        let owner = Owner { peer, client: packet.wrapped.map(|w| w.client_id) };
        let src = packet::inspect(&packet.payload).and_then(|info| match info.src {
            IpAddr::V4(src) => Some(src),
            IpAddr::V6(_) => None,
        });
        let allowed = src.is_some() && routes.lock().unwrap().by_owner.get(&owner).copied() == src;
        if !allowed {
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            log::debug!("TUN: пакет от {owner:?} с адресом {src:?} отброшен: адрес ему не выдан");
            continue;
        }
        match tun.send(&packet.payload).await {
            Ok(_) => {
                stats.from_peer.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
                log::debug!("TUN: запись {} байт не удалась: {e}", packet.payload.len());
            }
        }
    }
    log::warn!("TUN: канал входящих пира {peer} закрыт");
}
