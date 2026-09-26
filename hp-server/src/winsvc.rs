//! Служба Windows `homeproxy-server`: регистрация в диспетчере служб и работа под
//! его управлением (запуск, остановка, выключение ПК).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

use crate::settings::Settings;

pub const SERVICE_NAME: &str = "homeproxy-server";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// Файл настроек, с которым диспетчер запустил службу (`service_main` не получает
/// ничего, кроме строковых аргументов).
static CONFIG: OnceLock<Option<PathBuf>> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Отдаёт поток диспетчеру служб; возвращается, когда служба остановлена.
pub fn run_dispatcher(config: Option<PathBuf>) -> Result<()> {
    CONFIG.set(config).ok();
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("диспетчер служб")
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        let config = CONFIG.get().cloned().flatten();
        report_fatal(config.as_deref(), &format!("служба остановлена с ошибкой: {e:#}"));
    }
}

/// Пока логи не подняты (или упали настройки), сообщаем об ошибке в `server-error.log`
/// рядом с файлом настроек: у службы нет ни консоли, ни stderr.
fn report_fatal(config: Option<&Path>, message: &str) {
    log::error!("{message}");
    let dir = config.and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default();
    let line = format!("{message}\r\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("server-error.log"))
        .and_then(|mut file| std::io::Write::write_all(&mut file, line.as_bytes()));
}

fn status(state: ServiceState, exit_code: u32) -> ServiceStatus {
    let accepted = match state {
        ServiceState::Running => ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        _ => ServiceControlAccept::empty(),
    };
    ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: state,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(exit_code),
        checkpoint: 0,
        wait_hint: Duration::from_secs(if state == ServiceState::StopPending { 10 } else { 0 }),
        process_id: None,
    }
}

fn run_service() -> Result<()> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let stop_tx = Mutex::new(Some(stop_tx));
    let handle = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            if let Some(tx) = stop_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .context("регистрация обработчика управления службой")?;

    handle.set_service_status(status(ServiceState::Running, 0))?;
    let config = CONFIG.get().cloned().flatten();
    let result = Settings::load(config.as_deref()).and_then(|settings| {
        crate::run_blocking(&settings, async {
            let _ = stop_rx.await;
        })
    });
    let exit_code = u32::from(result.is_err());
    handle.set_service_status(status(ServiceState::StopPending, 0))?;
    handle.set_service_status(status(ServiceState::Stopped, exit_code))?;
    result
}

/// Регистрирует службу: автозапуск, при сбое перезапуск через 5 с.
pub fn install(config: Option<PathBuf>) -> Result<()> {
    let config = config.context("install: укажите --config путь\\к\\server.env")?;
    let config = std::path::absolute(&config).context("путь к настройкам")?;
    anyhow::ensure!(config.is_file(), "нет файла настроек {}", config.display());
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
        .context("нужны права администратора")?;
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "home-proxy server".into(),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec!["--service".into(), "--config".into(), config.clone().into_os_string()],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS;
    let service = manager.create_service(&info, access).context("создание службы")?;
    service.set_description("Мост «дыры UDP -> WireGuard» проекта home-proxy")?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(5),
        }]),
    })?;
    println!("служба {SERVICE_NAME} создана, настройки: {}", config.display());
    println!("запуск: sc start {SERVICE_NAME}");
    Ok(())
}

/// Останавливает и удаляет службу.
pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("диспетчер служб")?;
    let access = ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS;
    let service = manager.open_service(SERVICE_NAME, access).context("служба не найдена")?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        for _ in 0..50 {
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    service.delete().context("удаление службы")?;
    println!("служба {SERVICE_NAME} удалена");
    Ok(())
}
