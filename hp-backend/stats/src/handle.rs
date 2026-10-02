//! Хэндл, который путь данных клонирует к себе и зовёт на каждую отправку/подтверждение.
//! Строго неблокирующий (`try_send`): сбор статистики не должен влиять на пропускную
//! способность, которую он же измеряет (см. PLAN-ML.md, «Ограничения»).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::record::{RecvAck, SentRecord, StatsEvent};

/// Клонируемый хэндл к сборщику (`collector::spawn`).
#[derive(Clone)]
pub struct StatsHandle {
    tx: mpsc::Sender<StatsEvent>,
    // 32-битный атомик: на MIPS32 (роутеры) 64-битных атомиков нет.
    dropped: Arc<AtomicU32>,
}

impl StatsHandle {
    pub(crate) fn new(tx: mpsc::Sender<StatsEvent>, dropped: Arc<AtomicU32>) -> Self {
        Self { tx, dropped }
    }

    /// Пакет ушёл с дыры. Переполнение канала — запись отбрасывается, путь данных не ждёт.
    pub fn record_sent(&self, record: SentRecord) {
        self.try_send(StatsEvent::Sent(record));
    }

    /// Пришёл `PidReport`, одна его запись разобрана.
    pub fn record_ack(&self, ack: RecvAck) {
        self.try_send(StatsEvent::Ack(ack));
    }

    fn try_send(&self, event: StatsEvent) {
        if self.tx.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Сколько записей отброшено из-за переполнения канала с момента старта (контроль: если не
    /// ноль под нагрузкой — `STATS_CHAN_CAP` мал или сборщик не поспевает).
    pub fn dropped(&self) -> u32 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::MsgKind;
    use crate::inner::{InnerInfo, Proto};

    fn sent(pid: u32) -> SentRecord {
        SentRecord {
            pid,
            t_send_unix_ms: 0,
            slot: 0,
            via_relay: false,
            local_port: 0,
            dst_port: 0,
            wire_len: 0,
            payload_len: 0,
            inner: InnerInfo { proto: Proto::Udp, ports: None },
            kind: MsgKind::Data,
            client_id: None,
            flow: None,
            hole_age_ms: 0,
        }
    }

    #[test]
    fn full_channel_drops_instead_of_blocking() {
        let (tx, mut rx) = mpsc::channel(2);
        let handle = StatsHandle::new(tx, Arc::new(AtomicU32::new(0)));
        for pid in 0..5 {
            handle.record_sent(sent(pid));
        }
        assert_eq!(handle.dropped(), 3, "канал ёмкостью 2, пятая запись после двух должна отброситься трижды");
        // Канал не заблокирован: то, что влезло, читается.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}
