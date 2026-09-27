//! hp-tray — трей home-proxy для Windows и Linux: значок с цветом состояния, окно на Slint со
//! статусом службы, телефонами, дырами и потерями, сопряжение нового телефона по QR.
//!
//! Тонкий клиент протокола управления (`hp-control`): своих ключей и GUID не хранит, всё берёт у
//! `hp-server` по TCP (`--addr`, по умолчанию 127.0.0.1:47001) с токеном из `control.token`.
//!
//! Запуск: `hp-tray [--addr ip:порт] [--token-file путь] [--hidden]`.
//! `--hidden` — не показывать окно при старте (для автозапуска: окно открывается из трея).

#![cfg_attr(windows, windows_subsystem = "windows")]

mod icon;
mod qr;
mod tray;
mod view;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use hp_control::proto::control_message::Body;
use hp_control::proto::{CreatePairing, RemovePeer, Status, Subscribe};
use hp_control::Client;
use slint::ComponentHandle;

slint::include_modules!();

/// Как часто служба присылает статус.
const STATUS_INTERVAL_MS: u32 = 1000;
/// Пауза перед повторным подключением к службе.
const RECONNECT: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct Config {
    addr: SocketAddr,
    token_file: PathBuf,
    hidden: bool,
}

/// Что окно помнит между статусами.
#[derive(Default)]
pub struct UiState {
    pub status: Option<Status>,
    pub error: Option<String>,
    pub expanded: HashSet<String>,
    /// Показанный QR: имя телефона и срок пакета.
    pub pairing: Option<(String, u64)>,
}

fn usage() -> ! {
    eprintln!("использование: hp-tray [--addr ip:порт] [--token-file путь] [--hidden]");
    std::process::exit(2);
}

/// Токен ищется: `--token-file`, `HP_CONTROL_TOKEN_FILE`, `control.token` рядом с программой,
/// `control.token` в текущем каталоге.
fn default_token_file() -> PathBuf {
    if let Ok(path) = std::env::var("HP_CONTROL_TOKEN_FILE") {
        return path.into();
    }
    let beside = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join(hp_control::TOKEN_FILE)));
    match beside {
        Some(path) if path.is_file() => path,
        _ => PathBuf::from(hp_control::TOKEN_FILE),
    }
}

fn parse_args() -> Config {
    let mut addr = std::env::var("HP_CONTROL_ADDR").unwrap_or_else(|_| hp_control::DEFAULT_ADDR.into());
    let mut token_file = None;
    let mut hidden = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => addr = args.next().unwrap_or_else(|| usage()),
            "--token-file" => token_file = Some(PathBuf::from(args.next().unwrap_or_else(|| usage()))),
            "--hidden" => hidden = true,
            _ => usage(),
        }
    }
    let addr = addr.parse().unwrap_or_else(|_| usage());
    Config { addr, token_file: token_file.unwrap_or_else(default_token_file), hidden }
}

async fn connect(config: &Config) -> Result<Client> {
    let token = hp_control::read_token(&config.token_file)?;
    let (client, _) = Client::connect(config.addr, &token).await?;
    Ok(client)
}

/// Подписка на статус; при обрыве — переподключение.
async fn status_loop(config: Config, ui: slint::Weak<MainWindow>, state: Arc<Mutex<UiState>>, tray: tray::Tray) {
    loop {
        let result: Result<()> = async {
            let mut client = connect(&config).await?;
            client.send(Body::Subscribe(Subscribe { interval_ms: STATUS_INTERVAL_MS })).await?;
            loop {
                if let Body::Status(status) = client.recv().await? {
                    {
                        let mut state = state.lock().unwrap();
                        state.status = Some(status);
                        state.error = None;
                    }
                    refresh(&ui, &state, &tray);
                }
            }
        }
        .await;
        if let Err(e) = result {
            {
                let mut state = state.lock().unwrap();
                state.status = None;
                state.error = Some(format!("{e:#}"));
            }
            refresh(&ui, &state, &tray);
        }
        tokio::time::sleep(RECONNECT).await;
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
    let config = parse_args();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let window = MainWindow::new().context("не удалось создать окно")?;
    let state = Arc::new(Mutex::new(UiState::default()));
    let tray = tray::Tray::start(runtime.handle(), window.as_weak()).unwrap_or_else(|e| {
        eprintln!("значок в трее недоступен: {e:#}; окно закрывать не стоит — только из него");
        tray::Tray::none()
    });
    let tray_available = tray.available();

    runtime.spawn(status_loop(config.clone(), window.as_weak(), state.clone(), tray.clone()));

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
        let (ui, state, config, runtime) = (window.as_weak(), state.clone(), config.clone(), runtime.handle().clone());
        move || {
            if let Some(window) = ui.upgrade() {
                window.set_busy(true);
            }
            let (ui, state, config) = (ui.clone(), state.clone(), config.clone());
            runtime.spawn(async move {
                let result = async {
                    let mut client = connect(&config).await?;
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
        let (ui, config, runtime) = (window.as_weak(), config.clone(), runtime.handle().clone());
        move |name| {
            let (ui, config, name) = (ui.clone(), config.clone(), name.to_string());
            runtime.spawn(async move {
                let result = async {
                    let mut client = connect(&config).await?;
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

    view::render(&window, &state.lock().unwrap());
    if !config.hidden || !tray_available {
        window.show()?;
    }
    slint::run_event_loop_until_quit()?;
    drop(tray);
    runtime.shutdown_timeout(Duration::from_millis(200));
    Ok(())
}
