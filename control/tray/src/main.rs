//! hp-tray — трей home-proxy для Windows и Linux: значок с цветом состояния, окно на Slint со
//! статусом службы, телефонами, дырами и потерями, сопряжение нового телефона по QR.
//!
//! Тонкий клиент протокола управления (`hp-control`): GUID телефонов не хранит, всё берёт у
//! службы (`hp-server` на ПК или `hp-router` на роутере). Единственное, что трей помнит, —
//! строку подключения `homeproxy-control://<ip:порт>/<ключ>`: пользователь вставляет её при
//! первом запуске (кнопка «Служба…» — сменить), трей хранит её в `tray.conf` в каталоге
//! настроек пользователя (права 600). По файлам службы трей не ходит.
//!
//! Запуск: `hp-tray [--connect строка] [--hidden]`. `--connect` — сразу запомнить строку;
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

/// Текущая строка подключения; её смена переподключает подписку на статус.
type Connection = watch::Receiver<Option<String>>;

/// Что окно помнит между статусами.
#[derive(Default)]
pub struct UiState {
    pub status: Option<Status>,
    pub error: Option<String>,
    /// Строка подключения задана.
    pub configured: bool,
    pub expanded: HashSet<String>,
    /// Показанный QR: имя телефона и срок пакета.
    pub pairing: Option<(String, u64)>,
}

fn usage() -> ! {
    eprintln!("использование: hp-tray [--connect строка-подключения] [--hidden]");
    std::process::exit(2);
}

/// Файл настроек трея: `$XDG_CONFIG_HOME/home-proxy/tray.conf` (или `~/.config/…`), на Windows —
/// `%APPDATA%\home-proxy\tray.conf`.
fn config_file() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|dir| dir.join("home-proxy").join("tray.conf"))
}

fn load_connection() -> Option<String> {
    let text = std::fs::read_to_string(config_file()?).ok()?;
    let line = text.lines().map(str::trim).find(|l| l.starts_with(hp_control::CONNECTION_PREFIX))?;
    hp_control::parse_connection_string(line).ok().map(|_| line.to_string())
}

fn save_connection(connection: &str) -> Result<()> {
    let path = config_file().context("не найден каталог настроек пользователя")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    hp_control::write_private(&path, &format!("{connection}\n")).with_context(|| format!("запись {}", path.display()))
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
            _ => usage(),
        }
    }
    parsed
}

async fn connect(connection: &Connection) -> Result<Client> {
    let text = connection.borrow().clone().context("строка подключения не задана")?;
    let (client, _) = Client::connect_string(&text).await?;
    Ok(client)
}

/// Подписка на статус по строке `text`: возвращается только с ошибкой (обрыв, чужой ключ).
async fn subscribe(text: Option<String>, ui: &slint::Weak<MainWindow>, state: &Arc<Mutex<UiState>>, tray: &tray::Tray) -> Result<()> {
    let text = text.context("строка подключения не задана")?;
    let (mut client, _) = Client::connect_string(&text).await?;
    client.send(Body::Subscribe(Subscribe { interval_ms: STATUS_INTERVAL_MS })).await?;
    loop {
        if let Body::Status(status) = client.recv().await? {
            {
                let mut state = state.lock().unwrap();
                state.status = Some(status);
                state.error = None;
                state.configured = true;
            }
            refresh(ui, state, tray);
        }
    }
}

/// Подписка на статус; при обрыве или смене строки подключения — переподключение.
async fn status_loop(mut connection: Connection, ui: slint::Weak<MainWindow>, state: Arc<Mutex<UiState>>, tray: tray::Tray) {
    loop {
        let text = connection.borrow_and_update().clone();
        let configured = text.is_some();
        let changed = tokio::select! {
            result = subscribe(text, &ui, &state, &tray) => {
                if let Err(e) = result {
                    let mut locked = state.lock().unwrap();
                    locked.status = None;
                    locked.configured = configured;
                    locked.error = configured.then(|| format!("{e:#}"));
                }
                refresh(&ui, &state, &tray);
                false
            }
            _ = connection.changed() => true,
        };
        if !changed {
            tokio::select! {
                _ = tokio::time::sleep(RECONNECT) => {}
                _ = connection.changed() => {}
            }
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
    if let Some(text) = &args.connect {
        hp_control::parse_connection_string(text)?;
        save_connection(text)?;
    }
    let (connection_tx, connection) = watch::channel(load_connection());
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
                window.set_setup_can_cancel(connection.borrow().is_some());
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
                    save_connection(&text)?;
                    anyhow::Ok(welcome)
                }
                .await;
                if checked.is_ok() {
                    let _ = connection_tx.send(Some(text));
                }
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
            let name = state.status.as_ref().and_then(|s| s.peers.get(index as usize)).map(|p| p.name.clone());
            if let Some(name) = name
                && !state.expanded.remove(&name)
            {
                state.expanded.insert(name);
            }
            view::render(&window, &state);
        }
    });

    window.on_add_phone({
        let (ui, state, connection, runtime) = (window.as_weak(), state.clone(), connection.clone(), runtime.handle().clone());
        move || {
            if let Some(window) = ui.upgrade() {
                window.set_busy(true);
            }
            let (ui, state, connection) = (ui.clone(), state.clone(), connection.clone());
            runtime.spawn(async move {
                let result = async {
                    let mut client = connect(&connection).await?;
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
        let (ui, connection, runtime) = (window.as_weak(), connection.clone(), runtime.handle().clone());
        move |name| {
            let (ui, connection, name) = (ui.clone(), connection.clone(), name.to_string());
            runtime.spawn(async move {
                let result = async {
                    let mut client = connect(&connection).await?;
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

    let configured = connection.borrow().is_some();
    state.lock().unwrap().configured = configured;
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
