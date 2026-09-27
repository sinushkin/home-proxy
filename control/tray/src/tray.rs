//! Значок в области уведомлений: цвет — состояние, подсказка — телефоны и дыры, меню — открыть
//! окно, добавить телефон, выйти. Linux — `ksni` (StatusNotifierItem по D-Bus: KDE, GNOME с
//! расширением AppIndicator, XFCE и др., без GTK); Windows — `tray-icon`.
//!
//! `Tray::set` можно звать из любого потока; действия меню выполняются в потоке окна Slint.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use slint::ComponentHandle;

use crate::MainWindow;

fn show(ui: &slint::Weak<MainWindow>) {
    let _ = ui.upgrade_in_event_loop(|window| {
        let _ = window.show();
    });
}

fn add_phone(ui: &slint::Weak<MainWindow>) {
    let _ = ui.upgrade_in_event_loop(|window| {
        let _ = window.show();
        window.invoke_add_phone();
    });
}

fn quit() {
    let _ = slint::invoke_from_event_loop(|| {
        let _ = slint::quit_event_loop();
    });
}

#[derive(Clone)]
pub struct Tray {
    backend: Option<Arc<Backend>>,
    last: Arc<Mutex<Option<(u8, String)>>>,
}

impl Tray {
    /// Без значка (его негде показать): окно — единственный интерфейс.
    pub fn none() -> Self {
        Self { backend: None, last: Arc::default() }
    }

    pub fn available(&self) -> bool {
        self.backend.is_some()
    }

    /// Обновить цвет и подсказку (повтор того же состояния ничего не делает).
    pub fn set(&self, level: u8, tooltip: String) {
        let Some(backend) = &self.backend else { return };
        {
            let mut last = self.last.lock().unwrap();
            if last.as_ref() == Some(&(level, tooltip.clone())) {
                return;
            }
            *last = Some((level, tooltip.clone()));
        }
        backend.set(level, tooltip);
    }

    /// Создаётся в потоке окна до `run_event_loop`.
    pub fn start(runtime: &tokio::runtime::Handle, ui: slint::Weak<MainWindow>) -> Result<Self> {
        Ok(Self { backend: Some(Arc::new(Backend::start(runtime, ui)?)), last: Arc::default() })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use ksni::menu::StandardItem;
    use ksni::{MenuItem, TrayMethods};

    struct HpTray {
        level: u8,
        tooltip: String,
        ui: slint::Weak<MainWindow>,
    }

    impl ksni::Tray for HpTray {
        fn id(&self) -> String {
            "home-proxy".into()
        }

        fn title(&self) -> String {
            "Home Proxy".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            // ksni ждёт ARGB32 в сетевом порядке байт.
            let rgba = crate::icon::rgba(self.level);
            let data = rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
            let size = crate::icon::SIZE as i32;
            vec![ksni::Icon { width: size, height: size, data }]
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: "Home Proxy".into(), description: self.tooltip.clone(), ..Default::default() }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            show(&self.ui);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem { label: "Открыть".into(), activate: Box::new(|t: &mut Self| show(&t.ui)), ..Default::default() }.into(),
                StandardItem { label: "Добавить телефон".into(), activate: Box::new(|t: &mut Self| add_phone(&t.ui)), ..Default::default() }
                    .into(),
                MenuItem::Separator,
                StandardItem { label: "Выход".into(), activate: Box::new(|_: &mut Self| quit()), ..Default::default() }.into(),
            ]
        }
    }

    pub struct Backend {
        handle: ksni::Handle<HpTray>,
        runtime: tokio::runtime::Handle,
    }

    impl Backend {
        pub fn start(runtime: &tokio::runtime::Handle, ui: slint::Weak<MainWindow>) -> Result<Self> {
            let tray = HpTray { level: 0, tooltip: "Home Proxy: подключение к службе…".into(), ui };
            let handle = runtime.block_on(tray.spawn())?;
            Ok(Self { handle, runtime: runtime.clone() })
        }

        pub fn set(&self, level: u8, tooltip: String) {
            let handle = self.handle.clone();
            self.runtime.spawn(async move {
                handle
                    .update(|t| {
                        t.level = level;
                        t.tooltip = tooltip;
                    })
                    .await;
            });
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::cell::RefCell;
    use std::time::Duration;

    use super::*;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    struct Native {
        icon: TrayIcon,
        _poll: slint::Timer,
    }

    thread_local! {
        // TrayIcon живёт в потоке окна (Win32 требует тот же поток, что крутит сообщения).
        static NATIVE: RefCell<Option<Native>> = const { RefCell::new(None) };
    }

    fn icon(level: u8) -> Option<Icon> {
        Icon::from_rgba(crate::icon::rgba(level), crate::icon::SIZE, crate::icon::SIZE).ok()
    }

    pub struct Backend;

    impl Backend {
        pub fn start(_runtime: &tokio::runtime::Handle, ui: slint::Weak<MainWindow>) -> Result<Self> {
            let open = MenuItem::new("Открыть", true, None);
            let add = MenuItem::new("Добавить телефон", true, None);
            let exit = MenuItem::new("Выход", true, None);
            let menu = Menu::new();
            menu.append(&open)?;
            menu.append(&add)?;
            menu.append(&PredefinedMenuItem::separator())?;
            menu.append(&exit)?;
            let tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_tooltip("Home Proxy")
                .with_icon(icon(0).ok_or_else(|| anyhow::anyhow!("значок"))?)
                .build()?;
            let (open, add, exit) = (open.id().clone(), add.id().clone(), exit.id().clone());
            // События значка и меню приходят в каналы tray-icon: разбираем их таймером в потоке окна.
            let poll = slint::Timer::default();
            poll.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
                while let Ok(event) = MenuEvent::receiver().try_recv() {
                    if event.id == open {
                        show(&ui);
                    } else if event.id == add {
                        add_phone(&ui);
                    } else if event.id == exit {
                        quit();
                    }
                }
                while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        show(&ui);
                    }
                }
            });
            NATIVE.with(|n| *n.borrow_mut() = Some(Native { icon: tray, _poll: poll }));
            Ok(Self)
        }

        pub fn set(&self, level: u8, tooltip: String) {
            let _ = slint::invoke_from_event_loop(move || {
                NATIVE.with(|n| {
                    if let Some(native) = n.borrow().as_ref() {
                        let _ = native.icon.set_icon(icon(level));
                        // Подсказка Windows — до 127 символов.
                        let short: String = tooltip.chars().take(120).collect();
                        let _ = native.icon.set_tooltip(Some(short));
                    }
                });
            });
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::*;

    pub struct Backend;

    impl Backend {
        pub fn start(_runtime: &tokio::runtime::Handle, _ui: slint::Weak<MainWindow>) -> Result<Self> {
            anyhow::bail!("значок в трее есть только на Linux и Windows")
        }

        pub fn set(&self, _level: u8, _tooltip: String) {}
    }
}

use platform::Backend;
