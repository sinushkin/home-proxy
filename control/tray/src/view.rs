//! Статус службы → свойства окна и значок трея.

use std::time::{SystemTime, UNIX_EPOCH};

use hp_control::proto::{HoleStatus, PeerStatus, Status};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::{HoleRow, MainWindow, PeerRow, ServiceState, UiState};

/// Порог потерь: до него дыра хорошая, после `BAD_LOSS` — плохая.
const WARN_LOSS: f32 = 0.02;
const BAD_LOSS: f32 = 0.10;

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Число с разрядами через узкий пробел: 1 234 567.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
}

fn percent(loss: f32) -> String {
    if loss < 0.0 {
        "—".into()
    } else if loss < 0.001 {
        "0%".into()
    } else if loss < 0.1 {
        format!("{:.1}%", loss * 100.0)
    } else {
        format!("{:.0}%", loss * 100.0)
    }
}

fn duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60);
    if d > 0 {
        format!("{d} д {h} ч")
    } else if h > 0 {
        format!("{h} ч {m} мин")
    } else {
        format!("{m} мин")
    }
}

/// Уровень дыры по худшей из потерь в обе стороны; 0 — данных ещё нет.
fn hole_level(hole: &HoleStatus) -> i32 {
    let worst = hole.loss_out.max(hole.loss_in);
    if worst < 0.0 {
        0
    } else if worst < WARN_LOSS {
        3
    } else if worst < BAD_LOSS {
        2
    } else {
        1
    }
}

/// Средние потери по дырам, где они известны: (к телефону, от телефона).
fn mean_loss(peer: &PeerStatus) -> (f32, f32) {
    let mean = |values: Vec<f32>| if values.is_empty() { -1.0 } else { values.iter().sum::<f32>() / values.len() as f32 };
    (
        mean(peer.holes.iter().map(|h| h.loss_out).filter(|l| *l >= 0.0).collect()),
        mean(peer.holes.iter().map(|h| h.loss_in).filter(|l| *l >= 0.0).collect()),
    )
}

/// Сколько дыр в работе считаем здоровым набором: столько держит политика набора как минимум
/// (`PoolPolicy::min_active`); дыр больше — запас, а не норма (набор динамический: 4..10).
const HEALTHY_HOLES: u32 = 4;

fn healthy(peer: &PeerStatus) -> bool {
    peer.live >= HEALTHY_HOLES.min(peer.target)
}

fn peer_level(peer: &PeerStatus) -> i32 {
    if peer.live == 0 {
        if peer.state == "punching" { 2 } else { 1 }
    } else if !healthy(peer) {
        2
    } else {
        3
    }
}

/// Текущее ожидание буфера порядка: «порядок: 8 мс» или «буфер порядка выкл» (`REORDER_WAIT_MS=0`).
fn reorder_text(peer: &PeerStatus) -> String {
    if peer.reorder_wait_ms == 0 { "буфер порядка выкл".into() } else { format!("порядок: {} мс", peer.reorder_wait_ms) }
}

fn state_text(peer: &PeerStatus) -> &'static str {
    match peer.state.as_str() {
        "connected" => "на связи",
        "punching" => "пробиваем дыры",
        _ if is_vps(peer) => "ищем сервер",
        _ => "ищем телефон",
    }
}

fn is_vps(peer: &PeerStatus) -> bool {
    peer.kind == "vps"
}

/// Телефоны (без VPS роутера и без ещё не сопряжённых).
fn phones(status: &Status) -> Vec<&PeerStatus> {
    status.peers.iter().filter(|p| !p.pending && !is_vps(p)).collect()
}

/// Подпись службы в списке: что она сама о себе сообщила (`hp-server`, `vps-client`), пока статуса
/// нет — её адрес из строки подключения.
fn service_label(service: &ServiceState) -> String {
    match &service.status {
        Some(status) if !status.service.is_empty() => status.service.clone(),
        _ => hp_control::parse_connection_string(&service.conn).map(|(addr, _)| addr.to_string()).unwrap_or_default(),
    }
}

/// Уровень и подсказка по одной службе.
fn service_summary(service: &ServiceState) -> (u8, String) {
    let Some(status) = &service.status else {
        return (0, "Home Proxy: нет связи со службой".into());
    };
    let vps = status.peers.iter().find(|p| is_vps(p));
    if status.service == "vps-client" {
        // Клиент VPS-сервера: единственный «пир» — сервер.
        let Some(vps) = vps else { return (1, "vps-client: нет данных о VPS".into()) };
        let text = format!("vps-client: {}/{} дыр до VPS {}", vps.live, vps.target, vps.name);
        return (u8::try_from(peer_level(vps)).unwrap_or(1), text);
    }
    if let Some(vps) = vps
        && vps.live == 0
    {
        return (1, "Home Proxy: роутер без связи с VPS".into());
    }
    let peers = phones(status);
    if peers.is_empty() {
        return (2, "Home Proxy: телефонов нет".into());
    }
    let connected = peers.iter().filter(|p| p.live > 0).count();
    let level = if connected == 0 {
        1
    } else if peers.iter().all(|p| healthy(p)) {
        3
    } else {
        2
    };
    let lines: Vec<String> = peers.iter().map(|p| format!("{}: {}/{} дыр", p.name, p.live, p.target)).collect();
    (level, format!("Home Proxy: на связи {connected} из {}\n{}", peers.len(), lines.join("\n")))
}

/// Уровень и подсказка для значка трея: худшая из служб (служба без связи — красная).
pub fn summary(state: &UiState) -> (u8, String) {
    match state.services.as_slice() {
        [] => (0, "Home Proxy: служба не выбрана".into()),
        [one] => service_summary(one),
        many => {
            let parts: Vec<(u8, String)> = many.iter().map(service_summary).collect();
            let level = parts.iter().map(|(level, _)| (*level).max(1)).min().unwrap_or(0);
            (level, parts.into_iter().map(|(_, text)| text).collect::<Vec<_>>().join("\n"))
        }
    }
}

/// Все пиры всех служб подряд, с номером службы: порядок строк списка в окне.
pub fn peer_rows(state: &UiState) -> Vec<(usize, &PeerStatus)> {
    state
        .services
        .iter()
        .enumerate()
        .flat_map(|(i, service)| service.status.iter().flat_map(move |status| status.peers.iter().map(move |peer| (i, peer))))
        .collect()
}

fn headline(status: &Status) -> String {
    let real = phones(status);
    if status.service == "vps-client" {
        match status.peers.iter().find(|p| is_vps(p)) {
            Some(vps) => format!("VPS {}: {}/{} дыр в работе", vps.name, vps.live, vps.target),
            None => "Нет данных о VPS".to_string(),
        }
    } else if real.is_empty() {
        "Служба работает, телефонов нет".to_string()
    } else {
        format!("На связи телефонов: {} из {}", real.iter().filter(|p| p.live > 0).count(), real.len())
    }
}

fn details(status: &Status) -> String {
    let bind = if status.bind.is_empty() { String::new() } else { format!(" · дыры через {}", status.bind) };
    format!("{} · режим {}{bind} · работает {}", status.service, status.mode, duration(status.uptime_s))
}

fn traffic(status: &Status) -> String {
    let Some(t) = status.traffic.as_ref() else { return String::new() };
    let lan = if status.service == "hp-router" {
        format!("LAN → VPS {} · VPS → LAN {} · ", grouped(t.lan_to_vps), grouped(t.vps_to_lan))
    } else {
        String::new()
    };
    let (to, from) = if status.service == "vps-client" { ("серверу", "сервера") } else { ("телефонам", "телефонов") };
    format!(
        "{lan}пакетов к {to} {} · от {from} {} · TCP по порядку {} · отброшено {}",
        grouped(t.to_peers),
        grouped(t.from_peers),
        grouped(t.ordered),
        grouped(t.dropped)
    )
}

pub fn render(window: &MainWindow, state: &UiState) {
    let (level, _) = summary(state);
    let multi = state.services.len() > 1;
    window.set_overall(i32::from(level));
    window.set_connected(state.services.iter().any(|s| s.status.is_some()));
    let errors: Vec<String> = state
        .services
        .iter()
        .filter_map(|s| s.error.as_ref().map(|e| if multi { format!("{}: {e}", service_label(s)) } else { e.clone() }))
        .collect();
    window.set_error(if errors.is_empty() { String::new() } else { format!("Служба недоступна: {}", errors.join("; ")) }.into());

    let live: Vec<(&ServiceState, &Status)> = state.services.iter().filter_map(|s| s.status.as_ref().map(|st| (s, st))).collect();
    if live.is_empty() {
        if state.services.is_empty() {
            window.set_headline("Служба не выбрана".into());
            window.set_details("Нажмите «Служба…» и вставьте строку подключения homeproxy-control://…".into());
        } else {
            window.set_headline("Нет связи со службой".into());
            window.set_details("Проверьте, что служба запущена. Если строку подключения меняли — вставьте новую («Служба…»).".into());
        }
        window.set_traffic(SharedString::new());
        sync_peers(window, Vec::new());
        render_pairing(window, state);
        return;
    }

    // Несколько служб: в каждой строке подпись службы, чтобы было видно, чьё что.
    let prefixed = |service: &ServiceState, text: String| if multi && !text.is_empty() { format!("{}: {text}", service_label(service)) } else { text };
    window.set_headline(live.iter().map(|(_, st)| headline(st)).collect::<Vec<_>>().join(" · ").into());
    window.set_details(live.iter().map(|(s, st)| prefixed(s, details(st))).collect::<Vec<_>>().join("\n").into());
    window.set_traffic(live.iter().map(|(s, st)| prefixed(s, traffic(st))).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n").into());
    window.set_pairing_supported(live.iter().any(|(_, st)| st.pairing_supported));

    let rows: Vec<PeerRow> = peer_rows(state)
        .into_iter()
        .map(|(si, peer)| {
            let service = &state.services[si];
            let client_of_vps = service.status.as_ref().is_some_and(|st| st.service == "vps-client");
            let (out, inn) = mean_loss(peer);
            let holes: Vec<HoleRow> = peer
                .holes
                .iter()
                .map(|h| HoleRow {
                    slot: h.slot as i32,
                    addr: h.peer_addr.clone().into(),
                    sent: grouped(h.sent).into(),
                    received: grouped(h.received).into(),
                    loss_out: percent(h.loss_out).into(),
                    loss_in: percent(h.loss_in).into(),
                    age: if h.draining { format!("{}, слив", hp_control::age_text(h.age_secs)) } else { hp_control::age_text(h.age_secs) }.into(),
                    level: hole_level(h),
                })
                .collect();
            PeerRow {
                name: peer.name.clone().into(),
                title: if is_vps(peer) && client_of_vps { format!("VPS-сервер {}", peer.name).into() } else if is_vps(peer) { "VPS (шлюз дома)".into() } else { format!("Телефон {}", peer.name).into() },
                service: if multi { service_label(service) } else { String::new() }.into(),
                state: state_text(peer).into(),
                level: peer_level(peer),
                live: peer.live as i32,
                target: peer.target as i32,
                addresses: peer.addresses.join(", ").into(),
                addr_title: if is_vps(peer) { "адрес сервера" } else { "адрес телефона" }.into(),
                reorder: reorder_text(peer).into(),
                registration: hp_control::registration_text(peer, now_unix() * 1000).unwrap_or_default().into(),
                loss: if out < 0.0 && inn < 0.0 { SharedString::new() } else { format!("потери ↑{} ↓{}", percent(out), percent(inn)).into() },
                pending: peer.pending,
                removable: peer.removable,
                expanded: state.expanded.contains(&peer.name),
                holes: ModelRc::new(VecModel::from(holes)),
            }
        })
        .collect();
    sync_peers(window, rows);
    render_pairing(window, state);
}

/// Строки списка обновляются на месте: если заменить модель целиком, Slint пересоздаст карточки
/// и сбросит их внутреннее состояние (подтверждение удаления, прокрутку).
fn sync_peers(window: &MainWindow, rows: Vec<PeerRow>) {
    let model = window.get_peers();
    let Some(model) = model.as_any().downcast_ref::<VecModel<PeerRow>>() else {
        window.set_peers(ModelRc::new(VecModel::from(rows)));
        return;
    };
    for (i, row) in rows.iter().enumerate() {
        if i < model.row_count() {
            model.set_row_data(i, row.clone());
        } else {
            model.push(row.clone());
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
}

/// Подпись под QR: сколько осталось, подключился ли телефон.
fn render_pairing(window: &MainWindow, state: &UiState) {
    let Some((name, expires)) = &state.pairing else { return };
    let peer = peer_rows(state).into_iter().map(|(_, p)| p).find(|p| &p.name == name);
    let (level, text) = match peer {
        Some(p) if !p.pending => (3, format!("Телефон {name} подключён — окно можно закрыть")),
        Some(p) if p.pending && p.live > 0 => (3, format!("Телефон {name} подключается…")),
        _ => {
            let left = expires.saturating_sub(now_unix());
            if left == 0 {
                (1, "Срок кода истёк — закройте окно и добавьте телефон заново".to_string())
            } else {
                (2, format!("Телефон {name} · код действует ещё {}:{:02}", left / 60, left % 60))
            }
        }
    };
    window.set_qr_name(name.clone().into());
    window.set_qr_level(level);
    window.set_qr_status(text.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_and_percent_formatting() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(1234), "1\u{202f}234");
        assert_eq!(grouped(1234567), "1\u{202f}234\u{202f}567");
        assert_eq!(percent(-1.0), "—");
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(0.0123), "1.2%");
        assert_eq!(percent(0.5), "50%");
    }

    #[test]
    fn reorder_wait_text() {
        let peer = |ms| PeerStatus { reorder_wait_ms: ms, ..Default::default() };
        assert_eq!(reorder_text(&peer(0)), "буфер порядка выкл");
        assert_eq!(reorder_text(&peer(8)), "порядок: 8 мс");
    }

    #[test]
    fn hole_levels() {
        let hole = |out, inn| HoleStatus { loss_out: out, loss_in: inn, ..Default::default() };
        assert_eq!(hole_level(&hole(-1.0, -1.0)), 0);
        assert_eq!(hole_level(&hole(0.0, 0.01)), 3);
        assert_eq!(hole_level(&hole(0.05, 0.0)), 2);
        assert_eq!(hole_level(&hole(0.0, 0.3)), 1);
    }

    fn one(status: Status) -> UiState {
        UiState { services: vec![ServiceState { status: Some(status), ..Default::default() }], ..Default::default() }
    }

    #[test]
    fn tray_summary() {
        assert_eq!(summary(&UiState::default()).0, 0, "служб нет");
        assert_eq!(summary(&UiState { services: vec![ServiceState::default()], ..Default::default() }).0, 0, "службы без связи");
        let peer = |name: &str, live| PeerStatus { name: name.into(), live, target: 10, ..Default::default() };
        assert_eq!(summary(&one(Status { peers: vec![peer("a", 10), peer("b", 10)], ..Default::default() })).0, 3);
        // Набор динамический: четыре дыры в работе — норма, меньше — предупреждение.
        assert_eq!(summary(&one(Status { peers: vec![peer("a", 5), peer("b", 4)], ..Default::default() })).0, 3);
        assert_eq!(summary(&one(Status { peers: vec![peer("a", 10), peer("b", 3)], ..Default::default() })).0, 2);
        assert_eq!(summary(&one(Status { peers: vec![peer("a", 0)], ..Default::default() })).0, 1);
        // Роутер: без связи с VPS — красный, даже если телефоны на связи.
        let vps = |live| PeerStatus { name: "VPS".into(), kind: "vps".into(), live, target: 10, ..Default::default() };
        assert_eq!(summary(&one(Status { peers: vec![vps(0), peer("a", 10)], ..Default::default() })).0, 1);
        assert_eq!(summary(&one(Status { peers: vec![vps(10), peer("a", 10)], ..Default::default() })).0, 3);
        // vps-client: один пир — сервер.
        let (level, text) = summary(&one(Status { service: "vps-client".into(), peers: vec![vps(6)], ..Default::default() }));
        assert_eq!(level, 3);
        assert!(text.contains("6/10 дыр до VPS"), "{text}");
        assert_eq!(summary(&one(Status { service: "vps-client".into(), peers: vec![vps(2)], ..Default::default() })).0, 2);
        assert_eq!(summary(&one(Status { service: "vps-client".into(), peers: vec![vps(0)], ..Default::default() })).0, 1);
    }

    #[test]
    fn several_services_show_the_worst_one_and_list_all_peers() {
        let peer = |name: &str, live| PeerStatus { name: name.into(), live, target: 10, ..Default::default() };
        let vps = PeerStatus { name: "VPS".into(), kind: "vps".into(), live: 6, target: 10, ..Default::default() };
        let client = ServiceState { status: Some(Status { service: "vps-client".into(), peers: vec![vps], ..Default::default() }), ..Default::default() };
        let server = ServiceState { status: Some(Status { service: "hp-server".into(), peers: vec![peer("phone", 10)], ..Default::default() }), ..Default::default() };
        let mut state = UiState { services: vec![client, server], ..Default::default() };
        assert_eq!(summary(&state).0, 3);
        let rows = peer_rows(&state);
        assert_eq!(rows.iter().map(|(i, p)| (*i, p.name.as_str())).collect::<Vec<_>>(), vec![(0, "VPS"), (1, "phone")], "обе службы в одном списке");
        // Одна из служб пропала: значок не зелёный, подсказка называет обе.
        state.services[1].status = None;
        let (level, text) = summary(&state);
        assert_eq!(level, 1, "служба без связи красит значок");
        assert!(text.contains("vps-client") && text.contains("нет связи"), "{text}");
        assert_eq!(peer_rows(&state).len(), 1, "у пропавшей службы пиров нет");
    }
}
