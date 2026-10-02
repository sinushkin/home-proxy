//! Модуль статистики пакетов вниз (`vps-server` → `vps-client`/`hp-router`): по-пакетная
//! обратная связь о доставке и задержке, запись в CSV для последующего офлайн-анализа (ML —
//! ищем, фильтрует ли ТСПУ пакеты по каким-то признакам или потери случайны). См. `PLAN-ML.md`
//! в корне репозитория.
//!
//! Подключается к `connection` за опциональной фичей `stats`; без неё в бинарнике (в частности,
//! в сборке `hp-router` под OpenWrt/MIPS32) этого крейта и его кода нет вовсе. Канал от пути
//! данных к сборщику неблокирующий (`StatsHandle::record_*` — `try_send`): сбор статистики не
//! должен влиять на то, что он же измеряет. Никаких 64-битных атомиков (целевая платформа —
//! MIPS32-роутеры): счётчики — `AtomicU32`, время — обычная арифметика `u64`/`i64`, не атомики.

mod collector;
mod csv;
mod handle;
mod inner;
mod record;
mod time;

pub use collector::{spawn, CollectorConfig};
pub use handle::StatsHandle;
pub use inner::{InnerInfo, Proto};
pub use record::{MsgKind, RecvAck, SentRecord, StatsEvent};
pub use time::{offset_from_triple, ServerTime};
