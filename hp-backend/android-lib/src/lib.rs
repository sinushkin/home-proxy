//! JNI-обёртка над `hp-client` для Android: `libhomeproxy.so`.
//!
//! Kotlin-сторона — `ru.homeproxy.HomeProxy` (`android-vpn/app/src/main/java`).
//! Одновременно работает не больше одного клиента: `nativeStart` поднимает
//! свой tokio-рантайм, `nativeStop` останавливает его вместе со всеми задачами.
//! Ошибки в Kotlin возвращаются строкой (null — успех), паники ловятся и не
//! роняют процесс.

use std::net::SocketAddr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Mutex, Once};

use anyhow::{Context, Result};
use hp_client::{Client, ClientConfig};
use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use tokio::runtime::{Builder, Runtime};
use uuid::Uuid;

struct Running {
    /// Рантайм держим, пока жив клиент: его остановка убивает все задачи.
    runtime: Runtime,
    client: Client,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
static LOGGER: Once = Once::new();

fn init_logging() {
    LOGGER.call_once(|| {
        #[cfg(target_os = "android")]
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(log::LevelFilter::Info)
                .with_tag("homeproxy"),
        );
    });
}

/// Разбирает настройки из строк, пришедших из Kotlin.
fn parse_config(
    stun: &str,
    mqtt: &str,
    ca_pem: &str,
    my_id: &str,
    peer_id: &str,
    local_port: i32,
) -> Result<ClientConfig> {
    anyhow::ensure!(
        ca_pem.contains("-----BEGIN CERTIFICATE-----"),
        "CA-сертификат брокера не похож на PEM"
    );
    Ok(ClientConfig {
        stun_addrs: hp_client::parse_stun_servers(stun)?,
        mqtt_addr: mqtt.trim().parse::<SocketAddr>().context("MQTT: ожидается ip:порт")?,
        mqtt_ca_pem: ca_pem.as_bytes().to_vec(),
        my_id: my_id.trim().parse::<Uuid>().context("мой GUID некорректен")?,
        peer_id: peer_id.trim().parse::<Uuid>().context("GUID роутера некорректен")?,
        local_port: u16::try_from(local_port).context("локальный порт вне 0..=65535")?,
        reorder_wait_ms: hp_client::DEFAULT_REORDER_WAIT.as_millis() as u32,
        data_holes: 0,
    })
}

fn start(config: ClientConfig) -> Result<()> {
    let mut running = RUNNING.lock().unwrap();
    anyhow::ensure!(running.is_none(), "клиент уже запущен");
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("homeproxy")
        .enable_all()
        .build()
        .context("не удалось создать tokio-рантайм")?;
    let client = runtime.block_on(Client::start(config))?;
    *running = Some(Running { runtime, client });
    Ok(())
}

fn stop() {
    // Рантайм останавливаем вне блокировки, чтобы не держать её на время остановки.
    let previous = RUNNING.lock().unwrap().take();
    if let Some(running) = previous {
        running.runtime.shutdown_background();
        log::info!("клиент остановлен");
    }
}

fn status() -> String {
    match RUNNING.lock().unwrap().as_ref() {
        Some(running) => running.client.status().to_string(),
        None => "остановлен".to_string(),
    }
}

fn live_holes() -> i32 {
    RUNNING
        .lock()
        .unwrap()
        .as_ref()
        .map(|running| i32::try_from(running.client.status().live_holes).unwrap_or(i32::MAX))
        .unwrap_or(0)
}

fn local_port() -> i32 {
    RUNNING
        .lock()
        .unwrap()
        .as_ref()
        .map(|running| i32::from(running.client.local_addr().port()))
        .unwrap_or(0)
}

/// Выполняет `f`, превращая панику в сообщение (в JNI паника через границу — UB).
fn guarded<T>(fallback: impl FnOnce(String) -> T, f: impl FnOnce() -> T) -> T {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "неизвестная паника".to_string());
            log::error!("паника в native-коде: {text}");
            fallback(format!("внутренняя ошибка: {text}"))
        }
    }
}

fn read(env: &mut JNIEnv, value: &JString) -> Result<String> {
    Ok(env.get_string(value).context("не удалось прочитать строку из JVM")?.into())
}

fn to_jstring(env: &mut JNIEnv, text: &str) -> jstring {
    env.new_string(text).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// `HomeProxy.nativeStart(stun, mqtt, caPem, myId, peerId, localPort): String?`
/// Возвращает null при успехе, иначе текст ошибки.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ru_homeproxy_HomeProxy_nativeStart<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    stun: JString<'local>,
    mqtt: JString<'local>,
    ca_pem: JString<'local>,
    my_id: JString<'local>,
    peer_id: JString<'local>,
    local_port: jint,
    reorder_ms: jint,
    data_holes: jint,
) -> jstring {
    init_logging();
    let outcome: Result<()> = guarded(
        |message| Err(anyhow::anyhow!(message)),
        || {
            let mut config = parse_config(
                &read(&mut env, &stun)?,
                &read(&mut env, &mqtt)?,
                &read(&mut env, &ca_pem)?,
                &read(&mut env, &my_id)?,
                &read(&mut env, &peer_id)?,
                local_port,
            )?;
            config.reorder_wait_ms = u32::try_from(reorder_ms).context("reorderMs: ожидается 0 или больше")?;
            config.data_holes = u8::try_from(data_holes).context("dataHoles: ожидается 0..=255")?;
            start(config)
        },
    );
    match outcome {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => {
            log::warn!("запуск не удался: {e:#}");
            to_jstring(&mut env, &format!("{e:#}"))
        }
    }
}

/// `HomeProxy.nativeStop()`
#[unsafe(no_mangle)]
pub extern "system" fn Java_ru_homeproxy_HomeProxy_nativeStop<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) {
    guarded(|_| (), stop);
}

/// `HomeProxy.nativeStatus(): String`
#[unsafe(no_mangle)]
pub extern "system" fn Java_ru_homeproxy_HomeProxy_nativeStatus<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let text = guarded(|message| message, status);
    to_jstring(&mut env, &text)
}

/// `HomeProxy.nativeLiveHoles(): Int` — сколько дыр к роутеру сейчас живо
/// (0, если клиент не запущен). По нему приложение решает, можно ли включать VPN.
#[unsafe(no_mangle)]
pub extern "system" fn Java_ru_homeproxy_HomeProxy_nativeLiveHoles<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jint {
    guarded(|_| 0, live_holes)
}

/// `HomeProxy.nativeLocalPort(): Int` — порт моста для WireGuard (0, если не запущен).
#[unsafe(no_mangle)]
pub extern "system" fn Java_ru_homeproxy_HomeProxy_nativeLocalPort<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jint {
    guarded(|_| 0, local_port)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CA: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";

    #[test]
    fn valid_settings_parse_and_trim() {
        let config =
            parse_config(" 1.2.3.4:3478 , 5.6.7.8:19302", "1.2.3.4:8883\n", CA, &format!(" {A}"), B, 51821).unwrap();
        assert_eq!(config.stun_addrs.len(), 2);
        assert_eq!(config.stun_addrs[0].to_string(), "1.2.3.4:3478");
        assert_eq!(config.my_id.to_string(), A);
        assert_eq!(config.local_port, 51821);
    }

    #[test]
    fn each_bad_field_is_named_in_the_error() {
        let cases = [
            (parse_config("nope", "1.2.3.4:1", CA, A, B, 1), "STUN"),
            (parse_config("1.2.3.4:1", "1.2.3.4", CA, A, B, 1), "MQTT"),
            (parse_config("1.2.3.4:1", "1.2.3.4:1", "text", A, B, 1), "CA"),
            (parse_config("1.2.3.4:1", "1.2.3.4:1", CA, "x", B, 1), "мой GUID"),
            (parse_config("1.2.3.4:1", "1.2.3.4:1", CA, A, "x", 1), "GUID роутера"),
            (parse_config("1.2.3.4:1", "1.2.3.4:1", CA, A, B, 70000), "порт"),
            (parse_config("1.2.3.4:1", "1.2.3.4:1", CA, A, B, -1), "порт"),
        ];
        for (result, expected) in cases {
            let error = format!("{:#}", result.err().expect("должна быть ошибка"));
            assert!(error.contains(expected), "ожидали «{expected}» в: {error}");
        }
    }

    #[test]
    fn port_zero_means_any_free_port() {
        assert_eq!(parse_config("1.2.3.4:1", "1.2.3.4:1", CA, A, B, 0).unwrap().local_port, 0);
    }

    #[test]
    fn panics_are_caught_and_reported() {
        let message = guarded(|message| message, || -> String { panic!("boom") });
        assert!(message.contains("boom"), "{message}");
    }

    #[test]
    fn status_and_port_when_stopped() {
        assert_eq!(status(), "остановлен");
        assert_eq!(local_port(), 0);
        assert_eq!(live_holes(), 0);
        stop(); // остановка неработающего клиента ничего не ломает
    }
}
