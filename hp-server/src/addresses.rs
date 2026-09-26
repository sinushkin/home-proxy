//! Книга адресов в туннеле: сервер раздаёт их клиентам и помнит, кому что выдал.
//!
//! Подсеть — из адреса сервера в TUN (`TUN_ADDR`, `10.80.0.1/16`). Роутеры и хосты получают адреса
//! из первой `/24` подсети (`10.80.0.2`–`10.80.0.254`), телефоны — из остальных (`10.80.1.1` и
//! дальше); подсеть `/24` и уже — один общий пул. Адрес закрепляется за ключом клиента (имя пира
//! или «роутер + номер телефона») и при переподключении выдаётся тот же; книга хранится в файле
//! (строки `ключ адрес`), чтобы адреса пережили перезапуск сервера.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use connection::proto::AddressKind;

pub struct AddressBook {
    server: Ipv4Addr,
    prefix: u8,
    file: Option<PathBuf>,
    assigned: HashMap<String, Ipv4Addr>,
}

impl AddressBook {
    /// Книга для сервера с адресом `server/prefix`; `file` — где хранить выданное (`None` —
    /// только в памяти). Существующий файл читается.
    pub fn load(server: Ipv4Addr, prefix: u8, file: Option<PathBuf>) -> Result<Self> {
        anyhow::ensure!((8..=30).contains(&prefix), "подсеть туннеля /{prefix}: нужна от /8 до /30");
        let mut assigned = HashMap::new();
        if let Some(path) = &file
            && let Ok(text) = std::fs::read_to_string(path)
        {
            for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let (key, address) = line.split_once(' ').with_context(|| format!("{}: строка «{line}»", path.display()))?;
                let address: Ipv4Addr = address.trim().parse().with_context(|| format!("{}: адрес в «{line}»", path.display()))?;
                assigned.insert(key.to_string(), address);
            }
        }
        Ok(Self { server, prefix, file, assigned })
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Адрес клиента `key`: прежний, если уже выдавали, иначе первый свободный из пула `kind`.
    pub fn get_or_assign(&mut self, key: &str, kind: AddressKind) -> Result<Ipv4Addr> {
        if let Some(address) = self.assigned.get(key)
            && self.contains(*address)
        {
            return Ok(*address);
        }
        let used: HashSet<Ipv4Addr> = self.assigned.values().copied().collect();
        let address = self
            .pool(kind)
            .find(|a| *a != self.server && !used.contains(a))
            .with_context(|| format!("в подсети {}/{} кончились адреса", self.server, self.prefix))?;
        self.assigned.insert(key.to_string(), address);
        self.save();
        Ok(address)
    }

    fn mask(&self) -> u32 {
        u32::MAX << (32 - u32::from(self.prefix))
    }

    fn contains(&self, address: Ipv4Addr) -> bool {
        u32::from(address) & self.mask() == u32::from(self.server) & self.mask()
    }

    /// Адреса пула по порядку: без адресов сети и широковещательного, и (в широкой подсети) без
    /// `.0` и `.255` в каждой `/24` — чтобы не путать.
    fn pool(&self, kind: AddressKind) -> impl Iterator<Item = Ipv4Addr> {
        let network = u32::from(self.server) & self.mask();
        let broadcast = network | !self.mask();
        let (first, last) = if self.prefix >= 24 {
            (network + 1, broadcast - 1)
        } else {
            match kind {
                AddressKind::Host => (network + 1, network + 254),
                AddressKind::Phone => (network + 256, broadcast - 1),
            }
        };
        let wide = self.prefix < 24;
        (first..=last).filter(move |a| !wide || !matches!(a & 0xff, 0 | 255)).map(Ipv4Addr::from)
    }

    fn save(&self) {
        let Some(path) = &self.file else { return };
        let mut lines: Vec<String> = self.assigned.iter().map(|(k, a)| format!("{k} {a}")).collect();
        lines.sort();
        let text = format!("# адреса в туннеле, выданные клиентам (ключ адрес)\n{}\n", lines.join("\n"));
        if let Err(e) = std::fs::write(path, text) {
            log::warn!("не удалось сохранить адреса в {}: {e}", path.display());
        }
    }
}

/// Запасные DNS: всегда в конце списка, который сервер отдаёт клиентам.
pub const FALLBACK_DNS: [Ipv4Addr; 2] = [Ipv4Addr::new(8, 8, 8, 8), Ipv4Addr::new(1, 1, 1, 1)];

/// DNS для клиентов: `configured` (`DNS=` из настроек, через запятую), иначе резолверы этой машины
/// (`/run/systemd/resolve/resolv.conf` — настоящие адреса за systemd-resolved, иначе
/// `/etc/resolv.conf`) без loopback-адресов (с телефона они недостижимы); в конце — 8.8.8.8 и
/// 1.1.1.1. Повторы убираются.
pub fn client_dns(configured: Option<&str>) -> anyhow::Result<Vec<Ipv4Addr>> {
    let own = match configured {
        Some(list) => list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<Ipv4Addr>().with_context(|| format!("DNS: «{s}» — не IPv4")))
            .collect::<anyhow::Result<Vec<_>>>()?,
        None => ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"]
            .iter()
            .find_map(|path| std::fs::read_to_string(path).ok().map(|text| nameservers(&text)).filter(|list| !list.is_empty()))
            .unwrap_or_default(),
    };
    let mut all = Vec::new();
    for address in own.into_iter().chain(FALLBACK_DNS) {
        if !all.contains(&address) {
            all.push(address);
        }
    }
    Ok(all)
}

/// IPv4-адреса `nameserver` из `resolv.conf`, кроме loopback.
fn nameservers(text: &str) -> Vec<Ipv4Addr> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|rest| rest.split_whitespace().next()?.parse::<Ipv4Addr>().ok())
        .filter(|a| !a.is_loopback() && !a.is_unspecified())
        .collect()
}

/// Ключ клиента: пир напрямую — по имени его GUID, клиент за роутером — роутер и номер.
pub fn client_key(peer_name: &str, client: Option<u8>) -> String {
    match client {
        None => format!("peer:{peer_name}"),
        Some(client) => format!("via:{peer_name}:{client}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(prefix: u8) -> AddressBook {
        AddressBook::load(Ipv4Addr::new(10, 80, 0, 1), prefix, None).unwrap()
    }

    #[test]
    fn hosts_and_phones_come_from_separate_pools_and_stay_put() {
        let mut b = book(16);
        assert_eq!(b.get_or_assign("peer:router1", AddressKind::Host).unwrap(), Ipv4Addr::new(10, 80, 0, 2));
        assert_eq!(b.get_or_assign("peer:router2", AddressKind::Host).unwrap(), Ipv4Addr::new(10, 80, 0, 3));
        assert_eq!(b.get_or_assign("via:router1:1", AddressKind::Phone).unwrap(), Ipv4Addr::new(10, 80, 1, 1));
        assert_eq!(b.get_or_assign("via:router2:1", AddressKind::Phone).unwrap(), Ipv4Addr::new(10, 80, 1, 2), "у телефонов разных роутеров разные адреса");
        assert_eq!(b.get_or_assign("peer:router1", AddressKind::Host).unwrap(), Ipv4Addr::new(10, 80, 0, 2), "повторный запрос — тот же адрес");
    }

    #[test]
    fn narrow_subnet_is_one_pool_without_the_server_address() {
        let mut b = AddressBook::load(Ipv4Addr::new(10, 80, 0, 1), 30, None).unwrap();
        assert_eq!(b.get_or_assign("a", AddressKind::Phone).unwrap(), Ipv4Addr::new(10, 80, 0, 2));
        assert!(b.get_or_assign("b", AddressKind::Host).is_err(), "в /30 только два адреса узлов");
    }

    #[test]
    fn wide_pools_skip_dot_zero_and_dot_255() {
        let mut b = book(16);
        for i in 0..254 {
            b.get_or_assign(&format!("via:r:{i}"), AddressKind::Phone).unwrap(); // 10.80.1.1–254
        }
        assert_eq!(b.get_or_assign("next", AddressKind::Phone).unwrap(), Ipv4Addr::new(10, 80, 2, 1));
    }

    #[test]
    fn dns_comes_from_settings_or_resolv_conf_with_fallbacks_last() {
        assert_eq!(
            client_dns(Some("192.168.3.1, 1.1.1.1")).unwrap(),
            vec![Ipv4Addr::new(192, 168, 3, 1), Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)],
            "свой первым, запасные в конце, без повторов"
        );
        assert!(client_dns(Some("resolver")).is_err());
        let conf = "# systemd-resolved\nnameserver 127.0.0.53\nnameserver 192.168.3.1\nnameserver ::1\nsearch lan\n";
        assert_eq!(nameservers(conf), vec![Ipv4Addr::new(192, 168, 3, 1)], "loopback и IPv6 отброшены");
        assert!(client_dns(None).unwrap().ends_with(&FALLBACK_DNS));
    }

    #[test]
    fn the_book_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("hp-addresses-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
        let mut b = AddressBook::load(Ipv4Addr::new(10, 80, 0, 1), 16, Some(path.clone())).unwrap();
        b.get_or_assign("via:router1:3", AddressKind::Phone).unwrap();
        let first = b.get_or_assign("peer:pc", AddressKind::Host).unwrap();
        let mut again = AddressBook::load(Ipv4Addr::new(10, 80, 0, 1), 16, Some(path.clone())).unwrap();
        assert_eq!(again.get_or_assign("peer:pc", AddressKind::Host).unwrap(), first);
        assert_eq!(again.get_or_assign("peer:new", AddressKind::Host).unwrap(), Ipv4Addr::new(10, 80, 0, 3));
        std::fs::remove_file(path).unwrap();
    }
}
