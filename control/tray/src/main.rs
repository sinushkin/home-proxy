//! hp-tray — трей home-proxy для Windows и Linux: значок с цветом состояния, окно на Slint со
//! статусом службы, телефонами, дырами и потерями, сопряжение нового телефона по QR.
//!
//! Тонкий клиент протокола управления (`hp-control`): GUID телефонов не хранит, всё берёт у
//! службы (`hp-server` на ПК или `hp-router` на роутере). Единственное, что трей помнит, —
//! строки подключения `homeproxy-control://<ip:порт>/<ключ>`: пользователь вставляет их при
//! первом запуске (кнопка «Служба…» добавляет ещё одну службу; та же служба с новым ключом
//! заменяет прежнюю строку), трей хранит их в `tray.conf` в каталоге настроек пользователя
//! (по строке на службу, права 600). Окно показывает все службы сразу (`hp-server` и `vps-client`
//! вместе, например). По файлам службы трей не ходит.
//!
//! Запуск: `hp-tray [--connect строка] [--config файл] [--hidden]` (`--config` — свой файл настроек,
//! чтобы вести отдельный набор служб). `--connect` — сразу добавить строку к списку;
//! `--hidden` — не показывать окно при старте (для автозапуска: окно открывается из трея).

#![cfg_attr(windows, windows_subsystem = "windows")]

mod icon;
mod qr;
mod tray;
mod view;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use hp_control::proto::control_message::Body;
use hp_control::proto::{CreatePairing, RemovePeer, Status, Subscribe};
use hp_control::Client;
use slint::ComponentHandle;
use tokio::sync::watch;

slint::include_modules!();

/// Как часто служба присылает статус.
const STATUS_INTERVAL_MS: u32 = 1000;
/// Пауза перед повторным подключением к службе.
const RECONNECT: Duration = Duration::from_secs(2);
/// Если статус не пришёл столько — считаем соединение мёртвым и переподключаемся. TCP без
/// keepalive не замечает part потерянный FIN/RST (например, роутер перезапустился в момент
/// сетевой переналадки): чтение просто зависает навсегда на formально ещё «открытом» сокете.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Строки подключения ко всем службам; их смена переподключает подписки на статус.
type Connections = watch::Receiver<Vec<String>>;

/// Одна служба, к которой подключён трей (`hp-server`, `vps-client`, `hp-router`).
#[derive(Default)]
pub struct ServiceState {
    pub conn: String,
    pub status: Option<Status>,
    pub error: Option<String>,
}

/// Что окно помнит между статусами.
#[derive(Default)]
pub struct UiState {
    /// Службы в порядке строк подключения; пусто — ничего не выбрано.
    pub services: Vec<ServiceState>,
    pub expanded: HashSet<String>,
    /// Показанный QR: имя телефона и срок пакета.
    pub pairing: Option<(String, u64)>,
}

fn usage() -> ! {
    eprintln!("использование: hp-tray [--connect строка-подключения] [--config файл-настроек] [--hidden]");
    std::process::exit(2);
}

/// Файл настроек, заданный `--config` (чтобы рядом работал второй трей, например для другой службы).
static CONFIG_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Файл настроек трея: `$XDG_CONFIG_HOME/home-proxy/tray.conf` (или `~/.config/…`), на Windows —
/// `%APPDATA%\home-proxy\tray.conf`.
fn config_file() -> Option<PathBuf> {
    if let Some(path) = CONFIG_OVERRIDE.get() {
        return Some(path.clone());
    }
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|dir| dir.join("home-proxy").join("tray.conf"))
}

/// Строки подключения из `tray.conf` (по одной в строке; повреждённые пропускаются).
fn load_connections() -> Vec<String> {
    let Some(text) = config_file().and_then(|path| std::fs::read_to_string(path).ok()) else { return Vec::new() };
    text.lines()
        .map(str::trim)
        .filter(|l| l.starts_with(hp_control::CONNECTION_PREFIX) && hp_control::parse_connection_string(l).is_ok())
        .map(String::from)
        .collect()
}

fn save_connections(list: &[String]) -> Result<()> {
    let path = config_file().context("не найден каталог настроек пользователя")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text: String = list.iter().map(|c| format!("{c}\n")).collect();
    hp_control::write_private(&path, &text).with_context(|| format!("запись {}", path.display()))
}

/// Добавляет строку к списку; служба с тем же адресом заменяется (у неё сменился ключ).
fn merge_connection(list: &mut Vec<String>, new: &str) -> Result<()> {
    let (addr, _) = hp_control::parse_connection_string(new)?;
    let new = new.trim().to_string();
    match list.iter().position(|c| hp_control::parse_connection_string(c).is_ok_and(|(a, _)| a == addr)) {
        Some(i) => list[i] = new,
        None => list.push(new),
    }
    Ok(())
}

struct Args {
    connect: Option<String>,
    hidden: bool,
}

fn parse_args() -> Args {
    let mut parsed = Args { connect: None, hidden: false };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--connect" => parsed.connect = Some(args.next().unwrap_or_else(|| usage())),
            "--hidden" => parsed.hidden = true,
            "--config" => {
                let _ = CONFIG_OVERRIDE.set(PathBuf::from(args.next().unwrap_or_else(|| usage())));
            }
            _ => usage(),
        }
    }
    parsed
}

async fn connect(text: Option<String>) -> Result<Client> {
    let text = text.context("нет службы, которая это умеет")?;
    let (client, _) = Client::connect_string(&text).await?;
    Ok(client)
}

/// Подписка на статус службы номер `index`: возвращается только с ошибкой (обрыв, чужой ключ).
async fn subscribe(index: usize, text: &str, ui: &slint::Weak<MainWindow>, state: &Arc<Mutex<UiState>>, tray: &tray::Tray) -> Result<()> {
    let (mut client, _) = Client::connect_string(text).await?;
    client.send(Body::Subscribe(Subscribe { interval_ms: STATUS_INTERVAL_MS })).await?;
    loop {
        let body = tokio::time::timeout(STATUS_TIMEOUT, client.recv())
            .await
            .map_err(|_| anyhow::anyhow!("служба молчит дольше {}с", STATUS_TIMEOUT.as_secs()))??;
        if let Body::Status(status) = body {
            set_service(state, index, text, |service| {
                service.status = Some(status);
                service.error = None;
            });
            refresh(ui, state, tray);
        }
    }
}

/// Меняет службу `index`, если список за это время не сменился (в нём та же строка).
fn set_service(state: &Arc<Mutex<UiState>>, index: usize, text: &str, change: impl FnOnce(&mut ServiceState)) {
    let mut state = state.lock().unwrap();
    if let Some(service) = state.services.get_mut(index).filter(|s| s.conn == text) {
        change(service);
    }
}

/// Подписка на одну службу с переподключением при обрыве.
async fn service_loop(index: usize, text: String, ui: slint::Weak<MainWindow>, state: Arc<Mutex<UiState>>, tray: tray::Tray) {
    loop {
        if let Err(e) = subscribe(index, &text, &ui, &state, &tray).await {
            set_service(&state, index, &text, |service| {
                service.status = None;
                service.error = Some(format!("{e:#}"));
            });
            refresh(&ui, &state, &tray);
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

/// Подписки на статус всех служб; смена списка строк подключения пересоздаёт их.
async fn status_loop(mut connections: Connections, ui: slint::Weak<MainWindow>, state: Arc<Mutex<UiState>>, tray: tray::Tray) {
    loop {
        let list = connections.borrow_and_update().clone();
        state.lock().unwrap().services = list.iter().map(|conn| ServiceState { conn: conn.clone(), ..Default::default() }).collect();
        refresh(&ui, &state, &tray);
        // Задачи останавливаются вместе с набором (дроп `JoinSet`), когда список поменялся.
        let mut tasks = tokio::task::JoinSet::new();
        for (index, conn) in list.into_iter().enumerate() {
            tasks.spawn(service_loop(index, conn, ui.clone(), state.clone(), tray.clone()));
        }
        if connections.changed().await.is_err() {
            return;
        }
    }
}

/// Перерисовать окно и значок по текущему состоянию (из любого потока).
fn refresh(ui: &slint::Weak<MainWindow>, state: &Arc<Mutex<UiState>>, tray: &tray::Tray) {
    let (level, tooltip) = view::summary(&state.lock().unwrap());
    tray.set(level, tooltip);
    let state = state.clone();
    let _ = ui.upgrade_in_event_loop(move |window| view::render(&window, &state.lock().unwrap()));
}

fn main() -> Result<()> {
    let args = parse_args();
    let mut list = load_connections();
    if let Some(text) = &args.connect {
        merge_connection(&mut list, text)?;
        save_connections(&list)?;
    }
    let (connection_tx, connection) = watch::channel(list);
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let window = MainWindow::new().context("не удалось создать окно")?;
    let state = Arc::new(Mutex::new(UiState::default()));
    let tray = tray::Tray::start(runtime.handle(), window.as_weak()).unwrap_or_else(|e| {
        eprintln!("значок в трее недоступен: {e:#}; окно закрывать не стоит — только из него");
        tray::Tray::none()
    });
    let tray_available = tray.available();

    runtime.spawn(status_loop(connection.clone(), window.as_weak(), state.clone(), tray.clone()));

    window.on_open_setup({
        let ui = window.as_weak();
        let connection = connection.clone();
        move || {
            if let Some(window) = ui.upgrade() {
                window.set_setup_error(Default::default());
                window.set_setup_can_cancel(!connection.borrow().is_empty());
                window.set_setup_visible(true);
            }
        }
    });

    window.on_connect_with({
        let (ui, runtime, connection_tx) = (window.as_weak(), runtime.handle().clone(), Arc::new(connection_tx));
        move |text| {
            let text = text.trim().to_string();
            if let Some(window) = ui.upgrade() {
                window.set_setup_busy(true);
                window.set_setup_error(Default::default());
            }
            let (ui, connection_tx) = (ui.clone(), connection_tx.clone());
            runtime.spawn(async move {
                // Строку проверяем подключением и только потом запоминаем.
                let checked = async {
                    let (_, welcome) = Client::connect_string(&text).await?;
                    // Служба добавляется к уже подключённым; с тем же адресом — заменяет прежнюю строку.
                    let mut list = connection_tx.borrow().clone();
                    merge_connection(&mut list, &text)?;
                    save_connections(&list)?;
                    anyhow::Ok((welcome, list))
                }
                .await;
                let checked = match checked {
                    Ok((welcome, list)) => {
                        let _ = connection_tx.send(list);
                        Ok(welcome)
                    }
                    Err(e) => Err(e),
                };
                let _ = ui.upgrade_in_event_loop(move |window| {
                    window.set_setup_busy(false);
                    match checked {
                        Ok(_) => window.set_setup_visible(false),
                        Err(e) => window.set_setup_error(format!("{e:#}").into()),
                    }
                });
            });
        }
    });

    window.on_toggle_peer({
        let (ui, state) = (window.as_weak(), state.clone());
        move |index| {
            let Some(window) = ui.upgrade() else { return };
            let mut state = state.lock().unwrap();
            let name = view::peer_rows(&state).get(index as usize).map(|(_, p)| p.name.clone());
            if let Some(name) = name
                && !state.expanded.remove(&name)
            {
                state.expanded.insert(name);
            }
            view::render(&window, &state);
        }
    });

    window.on_add_phone({
        let (ui, state, runtime) = (window.as_weak(), state.clone(), runtime.handle().clone());
        move || {
            if let Some(window) = ui.upgrade() {
                window.set_busy(true);
            }
            // Пару выпускает та служба, что это умеет (hp-server или hp-router).
            let target = state.lock().unwrap().services.iter().find(|s| s.status.as_ref().is_some_and(|st| st.pairing_supported)).map(|s| s.conn.clone());
            let (ui, state) = (ui.clone(), state.clone());
            runtime.spawn(async move {
                let result = async {
                    let mut client = connect(target).await?;
                    match client.request(Body::CreatePairing(CreatePairing {})).await? {
                        Body::Pairing(pairing) => Ok(pairing),
                        other => anyhow::bail!("неожиданный ответ: {other:?}"),
                    }
                }
                .await;
                let _ = ui.upgrade_in_event_loop(move |window| {
                    window.set_busy(false);
                    match result {
                        Ok(pairing) => {
                            let expires = pairing.bundle.as_ref().map(|b| b.expires_unix).unwrap_or(0);
                            match qr::image(&pairing.uri) {
                                Ok(image) => {
                                    window.set_qr_image(image);
                                    state.lock().unwrap().pairing = Some((pairing.name, expires));
                                    view::render(&window, &state.lock().unwrap());
                                    window.set_qr_visible(true);
                                    let _ = window.show();
                                }
                                Err(e) => window.set_error(format!("QR-код: {e:#}").into()),
                            }
                        }
                        Err(e) => window.set_error(format!("Добавить телефон: {e:#}").into()),
                    }
                });
            });
        }
    });

    window.on_close_qr({
        let (ui, state) = (window.as_weak(), state.clone());
        move || {
            state.lock().unwrap().pairing = None;
            if let Some(window) = ui.upgrade() {
                window.set_qr_visible(false);
                // Код — секрет пары: из памяти окна его убираем.
                window.set_qr_image(slint::Image::default());
            }
        }
    });

    window.on_remove_peer({
        let (ui, state, runtime) = (window.as_weak(), state.clone(), runtime.handle().clone());
        move |name| {
            // Удаляет та служба, у которой этот телефон есть.
            let target = state
                .lock()
                .unwrap()
                .services
                .iter()
                .find(|s| s.status.as_ref().is_some_and(|st| st.peers.iter().any(|p| p.name.as_str() == name.as_str() && p.removable)))
                .map(|s| s.conn.clone());
            let (ui, name) = (ui.clone(), name.to_string());
            runtime.spawn(async move {
                let result = async {
                    let mut client = connect(target).await?;
                    client.request(Body::RemovePeer(RemovePeer { name: name.clone() })).await
                }
                .await;
                if let Err(e) = result {
                    let _ = ui.upgrade_in_event_loop(move |window| window.set_error(format!("Удалить {name}: {e:#}").into()));
                }
            });
        }
    });

    // Отсчёт срока QR-кода.
    let countdown = slint::Timer::default();
    countdown.start(slint::TimerMode::Repeated, Duration::from_secs(1), {
        let (ui, state) = (window.as_weak(), state.clone());
        move || {
            if let Some(window) = ui.upgrade()
                && window.get_qr_visible()
            {
                view::render(&window, &state.lock().unwrap());
            }
        }
    });

    // Закрытие окна при живом трее прячет его; без трея — выход.
    window.window().on_close_requested(move || {
        if tray_available {
            slint::CloseRequestResponse::HideWindow
        } else {
            let _ = slint::quit_event_loop();
            slint::CloseRequestResponse::HideWindow
        }
    });

    let configured = !connection.borrow().is_empty();
    state.lock().unwrap().services = connection.borrow().iter().map(|conn| ServiceState { conn: conn.clone(), ..Default::default() }).collect();
    view::render(&window, &state.lock().unwrap());
    if !configured {
        // Первый запуск: сначала строка подключения.
        window.set_setup_visible(true);
    }
    if !args.hidden || !tray_available || !configured {
        window.show()?;
    }
    slint::run_event_loop_until_quit()?;
    drop(tray);
    runtime.shutdown_timeout(Duration::from_millis(200));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(addr: &str, key: char) -> String {
        format!("{}{addr}/{}", hp_control::CONNECTION_PREFIX, key.to_string().repeat(22))
    }

    #[test]
    fn a_new_service_is_added_and_the_same_address_is_replaced() {
        let mut list = vec![conn("192.168.3.1:47001", 'A')];
        merge_connection(&mut list, &conn("127.0.0.1:47002", 'B')).unwrap();
        assert_eq!(list.len(), 2, "другой адрес — другая служба");
        merge_connection(&mut list, &format!("  {}  ", conn("192.168.3.1:47001", 'C'))).unwrap();
        assert_eq!(list, vec![conn("192.168.3.1:47001", 'C'), conn("127.0.0.1:47002", 'B')], "тот же адрес — новый ключ на месте старого");
        assert!(merge_connection(&mut list, "мусор").is_err());
        assert_eq!(list.len(), 2);
    }
}
