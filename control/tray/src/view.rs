//! Статус службы → свойства окна и значок трея.

use std::time::{SystemTime, UNIX_EPOCH};

use hp_control::proto::{HoleStatus, PeerStatus, Status};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::{HoleRow, MainWindow, PeerRow, UiState};

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

fn peer_level(peer: &PeerStatus) -> i32 {
    if peer.live == 0 {
        if peer.state == "punching" { 2 } else { 1 }
    } else if peer.live < peer.target {
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

/// Уровень и подсказка для значка трея.
pub fn summary(state: &UiState) -> (u8, String) {
    let Some(status) = &state.status else {
        let text = if state.configured { "Home Proxy: нет связи со службой" } else { "Home Proxy: служба не выбрана" };
        return (0, text.into());
    };
    let vps = status.peers.iter().find(|p| is_vps(p));
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
    } else if peers.iter().all(|p| p.live == p.target) {
        3
    } else {
        2
    };
    let lines: Vec<String> = peers.iter().map(|p| format!("{}: {}/{} дыр", p.name, p.live, p.target)).collect();
    (level, format!("Home Proxy: на связи {connected} из {}\n{}", peers.len(), lines.join("\n")))
}

pub fn render(window: &MainWindow, state: &UiState) {
    let (level, _) = summary(state);
    window.set_overall(i32::from(level));
    window.set_connected(state.status.is_some());
    window.set_error(state.error.clone().map(|e| format!("Служба недоступна: {e}")).unwrap_or_default().into());

    let Some(status) = &state.status else {
        if state.configured {
            window.set_headline("Нет связи со службой".into());
            window.set_details("Проверьте, что служба запущена. Если строку подключения меняли — вставьте новую («Служба…»).".into());
        } else {
            window.set_headline("Служба не выбрана".into());
            window.set_details("Нажмите «Служба…» и вставьте строку подключения homeproxy-control://…".into());
        }
        window.set_traffic(SharedString::new());
        sync_peers(window, Vec::new());
        render_pairing(window, state);
        return;
    };

    let real = phones(status);
    let connected = real.iter().filter(|p| p.live > 0).count();
    window.set_headline(
        if real.is_empty() { "Служба работает, телефонов нет".to_string() } else { format!("На связи телефонов: {connected} из {}", real.len()) }.into(),
    );
    let bind = if status.bind.is_empty() { String::new() } else { format!(" · дыры через {}", status.bind) };
    window.set_details(format!("{} · режим {}{bind} · работает {}", status.service, status.mode, duration(status.uptime_s)).into());
    window.set_traffic(
        status
            .traffic
            .as_ref()
            .map(|t| {
                let lan = if status.service == "hp-router" {
                    format!("LAN → VPS {} · VPS → LAN {} · ", grouped(t.lan_to_vps), grouped(t.vps_to_lan))
                } else {
                    String::new()
                };
                format!(
                    "{lan}пакетов к телефонам {} · от телефонов {} · TCP по порядку {} · отброшено {}",
                    grouped(t.to_peers),
                    grouped(t.from_peers),
                    grouped(t.ordered),
                    grouped(t.dropped)
                )
            })
            .unwrap_or_default()
            .into(),
    );
    window.set_pairing_supported(status.pairing_supported);

    let rows: Vec<PeerRow> = status
        .peers
        .iter()
        .map(|peer| {
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
                    level: hole_level(h),
                })
                .collect();
            PeerRow {
                name: peer.name.clone().into(),
                title: if is_vps(peer) { "VPS (шлюз дома)".into() } else { format!("Телефон {}", peer.name).into() },
                state: state_text(peer).into(),
                level: peer_level(peer),
                live: peer.live as i32,
                target: peer.target as i32,
                addresses: peer.addresses.join(", ").into(),
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
    let peer = state.status.as_ref().and_then(|s| s.peers.iter().find(|p| &p.name == name));
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

    #[test]
    fn tray_summary() {
        let mut state = UiState::default();
        assert_eq!(summary(&state).0, 0);
        let peer = |name: &str, live| PeerStatus { name: name.into(), live, target: 10, ..Default::default() };
        state.status = Some(Status { peers: vec![peer("a", 10), peer("b", 10)], ..Default::default() });
        assert_eq!(summary(&state).0, 3);
        state.status = Some(Status { peers: vec![peer("a", 10), peer("b", 3)], ..Default::default() });
        assert_eq!(summary(&state).0, 2);
        state.status = Some(Status { peers: vec![peer("a", 0)], ..Default::default() });
        assert_eq!(summary(&state).0, 1);
        // Роутер: без связи с VPS — красный, даже если телефоны на связи.
        let vps = |live| PeerStatus { name: "VPS".into(), kind: "vps".into(), live, target: 10, ..Default::default() };
        state.status = Some(Status { peers: vec![vps(0), peer("a", 10)], ..Default::default() });
        assert_eq!(summary(&state).0, 1);
        state.status = Some(Status { peers: vec![vps(10), peer("a", 10)], ..Default::default() });
        assert_eq!(summary(&state).0, 3);
    }
}
