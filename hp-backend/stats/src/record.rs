//! Записи, которыми путь данных (`connection::multilink`) и приём `PidReport` кормят сборщик
//! (`collector`). См. PLAN-ML.md.

use crate::inner::InnerInfo;

/// Тип сообщения, которым ушёл пакет.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgKind {
    Data,
    Wrapped,
    Ordered,
}

impl MsgKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MsgKind::Data => "data",
            MsgKind::Wrapped => "wrapped",
            MsgKind::Ordered => "ordered",
        }
    }
}

/// Пакет вниз ушёл с дыры: то, что известно сразу, без ожидания подтверждения.
/// Время — `t_send_unix_ms` (часы сервера, `SystemTime::now()`; синхронизация с клиентом не
/// нужна — сервер и так в своих часах, это и есть эталон, см. `ServerTime` на клиенте).
#[derive(Clone, Copy, Debug)]
pub struct SentRecord {
    pub pid: u32,
    pub t_send_unix_ms: u64,
    pub slot: u32,
    pub via_relay: bool,
    pub local_port: u16,
    pub dst_port: u16,
    pub wire_len: u16,
    pub payload_len: u16,
    pub inner: InnerInfo,
    pub kind: MsgKind,
    pub client_id: Option<u8>,
    pub flow: Option<u32>,
    /// Возраст дыры (слота) на момент отправки, мс.
    pub hole_age_ms: u32,
}

/// Клиент подтвердил приём пакета `pid` (из разобранного `PidReport`), время — уже в часах
/// сервера (клиент перевёл через `ServerTime`).
#[derive(Clone, Copy, Debug)]
pub struct RecvAck {
    pub pid: u32,
    pub recv_server_ms: u64,
    /// Сколько пакет ждал в буфере порядка (`reorder`) недостающего предшественника; 0 — не ждал
    /// или буфер порядка выключен.
    pub reorder_wait_ms: u16,
    pub out_of_order: bool,
    /// Когда сам `PidReport` пришёл на сервер (часы сервера: сервер это видит напрямую, без
    /// перевода). Даёт `ack_rtt_ms` — кросс-проверку `flight_ms`, не требующую `ServerTime`
    /// вообще: от отправки пакета до прихода отчёта о нём, целиком в часах сервера.
    pub report_arrival_unix_ms: u64,
}

/// То, что идёт по каналу в сборщик.
#[derive(Clone, Copy, Debug)]
pub enum StatsEvent {
    Sent(SentRecord),
    Ack(RecvAck),
}
