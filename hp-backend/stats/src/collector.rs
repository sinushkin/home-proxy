//! Сборщик: получает `StatsEvent` из канала, сопоставляет отправленные пакеты с подтверждениями
//! по `pid`, списывает неподтверждённые по тайм-ауту как потерянные, пишет строки CSV
//! (`csv::RotatingCsv`). Одна задача на процесс сервера. См. PLAN-ML.md, раздел «Сборщик и CSV».

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::csv::{Row, RotatingCsv};
use crate::handle::StatsHandle;
use crate::record::{RecvAck, SentRecord, StatsEvent};

/// Настройки сборщика (из `STATS_*` в `vps.env`, см. PLAN-ML.md §5).
#[derive(Debug, Clone)]
pub struct CollectorConfig {
    /// Куда писать CSV; создаётся, если не существует.
    pub out_dir: PathBuf,
    /// Ёмкость канала между путём данных и сборщиком.
    pub channel_capacity: usize,
    /// Через сколько неподтверждённый пакет считаем потерянным (≫ RTT).
    pub loss_timeout: Duration,
    /// Ротация файла по размеру.
    pub max_file_bytes: u64,
    /// Как часто проверяем pending на тайм-аут.
    pub sweep_interval: Duration,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::from("stats/out"),
            channel_capacity: 4096,
            loss_timeout: Duration::from_secs(3),
            max_file_bytes: 64 * 1024 * 1024,
            sweep_interval: Duration::from_millis(200),
        }
    }
}

struct Pending {
    record: SentRecord,
    inserted_at: Instant,
    /// Темп на отправке: мс с предыдущего отправленного пакета (считается на вставке, не
    /// зависит от того, подтвердится этот пакет или спишется по тайм-ауту).
    send_gap_ms: Option<u64>,
}

/// Запускает сборщик: неблокирующий `StatsHandle` для пути данных и фоновую задачу. Дроп
/// хэндла (и закрытие канала) останавливает задачу после дозаписи того, что в pending.
pub fn spawn(config: CollectorConfig) -> std::io::Result<(StatsHandle, tokio::task::JoinHandle<()>)> {
    std::fs::create_dir_all(&config.out_dir)?;
    let writer = RotatingCsv::create(&config.out_dir, config.max_file_bytes)?;
    let (tx, rx) = mpsc::channel(config.channel_capacity);
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let handle = StatsHandle::new(tx, dropped);
    let task = tokio::spawn(run(rx, writer, config));
    Ok((handle, task))
}

async fn run(mut rx: mpsc::Receiver<StatsEvent>, mut writer: RotatingCsv, config: CollectorConfig) {
    let mut pending: HashMap<u32, Pending> = HashMap::new();
    let mut last_send_unix_ms: Option<u64> = None;
    let mut last_recv_server_ms: Option<u64> = None;
    let mut sweep = tokio::time::interval(config.sweep_interval);
    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(StatsEvent::Sent(record)) => {
                        let send_gap_ms = last_send_unix_ms.map(|prev| record.t_send_unix_ms.saturating_sub(prev));
                        last_send_unix_ms = Some(record.t_send_unix_ms);
                        pending.insert(record.pid, Pending { record, inserted_at: Instant::now(), send_gap_ms });
                    }
                    Some(StatsEvent::Ack(ack)) => {
                        handle_ack(&mut pending, &mut last_recv_server_ms, ack, &mut writer);
                    }
                    // Канал закрыт: все хэндлы (и сам сборщик — косвенно) дропнуты.
                    None => break,
                }
            }
            _ = sweep.tick() => {
                sweep_losses(&mut pending, config.loss_timeout, &mut writer);
                writer.flush();
            }
        }
    }
    // Остаток в pending — пакеты, для которых мы так и не узнали исход; списываем как
    // потерянные, не дожидаясь тайм-аута (процесс останавливается).
    for (_, p) in pending.drain() {
        writer.write_row(&Row::lost(&p.record, p.send_gap_ms));
    }
    writer.flush();
}

fn handle_ack(pending: &mut HashMap<u32, Pending>, last_recv_server_ms: &mut Option<u64>, ack: RecvAck, writer: &mut RotatingCsv) {
    let Some(p) = pending.remove(&ack.pid) else {
        // Отчёт о `pid`, который мы уже списали по тайм-ауту (отчёт запоздал), о чужом/дубле —
        // не ошибка, молча игнорируем.
        return;
    };
    let recv_gap_ms = last_recv_server_ms.map(|prev| ack.recv_server_ms.saturating_sub(prev));
    *last_recv_server_ms = Some(ack.recv_server_ms);
    let flight_ms = ack.recv_server_ms as i64 - p.record.t_send_unix_ms as i64;
    let ack_rtt_ms = ack.report_arrival_unix_ms.saturating_sub(p.record.t_send_unix_ms);
    let row = Row::delivered(&p.record, flight_ms, ack_rtt_ms, recv_gap_ms, ack.reorder_wait_ms, ack.out_of_order, p.send_gap_ms);
    writer.write_row(&row);
}

fn sweep_losses(pending: &mut HashMap<u32, Pending>, timeout: Duration, writer: &mut RotatingCsv) {
    let now = Instant::now();
    let expired: Vec<u32> = pending.iter().filter(|(_, p)| now.duration_since(p.inserted_at) >= timeout).map(|(&pid, _)| pid).collect();
    for pid in expired {
        if let Some(p) = pending.remove(&pid) {
            writer.write_row(&Row::lost(&p.record, p.send_gap_ms));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inner::{InnerInfo, Proto};
    use crate::record::MsgKind;
    use std::fs;

    fn sent(pid: u32, t_send_unix_ms: u64) -> SentRecord {
        SentRecord {
            pid,
            t_send_unix_ms,
            slot: 1,
            via_relay: false,
            local_port: 40001,
            dst_port: 51000,
            wire_len: 100,
            payload_len: 80,
            inner: InnerInfo { proto: Proto::Udp, ports: None },
            kind: MsgKind::Data,
            client_id: None,
            flow: None,
            hole_age_ms: 0,
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hp-stats-collector-{name}-{}", std::process::id()))
    }

    fn read_rows(dir: &PathBuf) -> Vec<Vec<String>> {
        let mut files: Vec<_> = fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        files.sort();
        let mut rows = Vec::new();
        for f in files {
            let content = fs::read_to_string(f).unwrap();
            for line in content.lines().skip(1) {
                // пропускаем заголовок каждого файла
                if !line.is_empty() {
                    rows.push(line.split(',').map(str::to_string).collect());
                }
            }
        }
        rows
    }

    #[tokio::test]
    async fn delivered_packet_is_written_with_flight_time() {
        let dir = test_dir("delivered");
        let _ = fs::remove_dir_all(&dir);
        let (handle, task) = spawn(CollectorConfig { out_dir: dir.clone(), sweep_interval: Duration::from_millis(20), ..CollectorConfig::default() }).unwrap();

        handle.record_sent(sent(1, 1_000));
        handle.record_ack(RecvAck { pid: 1, recv_server_ms: 1_030, reorder_wait_ms: 0, out_of_order: false, report_arrival_unix_ms: 1_050 });
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();

        let rows = read_rows(&dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "1", "pid");
        assert_eq!(rows[0][2], "1", "delivered");
        assert_eq!(rows[0][3], "30", "flight_ms = 1030 - 1000");
        assert_eq!(rows[0][4], "50", "ack_rtt_ms = 1050 - 1000");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unacknowledged_packet_is_written_as_lost_after_timeout() {
        let dir = test_dir("lost");
        let _ = fs::remove_dir_all(&dir);
        let (handle, task) = spawn(CollectorConfig {
            out_dir: dir.clone(),
            loss_timeout: Duration::from_millis(50),
            sweep_interval: Duration::from_millis(10),
            ..CollectorConfig::default()
        })
        .unwrap();

        handle.record_sent(sent(7, 2_000));
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();

        let rows = read_rows(&dir);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "7");
        assert_eq!(rows[0][2], "0", "delivered=0");
        assert_eq!(rows[0][3], "", "flight_ms пуст");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn late_ack_after_timeout_is_ignored_not_double_written() {
        let dir = test_dir("late-ack");
        let _ = fs::remove_dir_all(&dir);
        let (handle, task) = spawn(CollectorConfig {
            out_dir: dir.clone(),
            loss_timeout: Duration::from_millis(30),
            sweep_interval: Duration::from_millis(10),
            ..CollectorConfig::default()
        })
        .unwrap();

        handle.record_sent(sent(3, 5_000));
        tokio::time::sleep(Duration::from_millis(80)).await; // списан по тайм-ауту
        handle.record_ack(RecvAck { pid: 3, recv_server_ms: 5_200, reorder_wait_ms: 0, out_of_order: false, report_arrival_unix_ms: 5_200 });
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();

        let rows = read_rows(&dir);
        assert_eq!(rows.len(), 1, "только одна строка — списание по тайм-ауту, поздний ack молча проигнорирован");
        assert_eq!(rows[0][2], "0");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn send_gap_and_recv_gap_reflect_arrival_pace() {
        let dir = test_dir("gaps");
        let _ = fs::remove_dir_all(&dir);
        let (handle, task) = spawn(CollectorConfig { out_dir: dir.clone(), sweep_interval: Duration::from_millis(20), ..CollectorConfig::default() }).unwrap();

        handle.record_sent(sent(1, 1_000));
        handle.record_sent(sent(2, 1_010));
        handle.record_ack(RecvAck { pid: 1, recv_server_ms: 1_030, reorder_wait_ms: 0, out_of_order: false, report_arrival_unix_ms: 1_030 });
        handle.record_ack(RecvAck { pid: 2, recv_server_ms: 1_045, reorder_wait_ms: 5, out_of_order: true, report_arrival_unix_ms: 1_045 });
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();

        let mut rows = read_rows(&dir);
        rows.sort_by_key(|r| r[0].clone());
        // Столбцы: ... [21] send_gap_ms (последний); [5] recv_gap_ms; [6] reorder_wait_ms; [7] out_of_order.
        assert_eq!(rows[0][21], "", "первый пакет — не с чем сравнивать темп отправки");
        assert_eq!(rows[1][21], "10", "10мс между отправками 1000 и 1010");
        assert_eq!(rows[0][5], "", "первое подтверждение — не с чем сравнивать темп приёма");
        assert_eq!(rows[1][5], "15", "15мс между приёмами 1030 и 1045");
        assert_eq!(rows[1][6], "5", "reorder_wait_ms");
        assert_eq!(rows[1][7], "1", "out_of_order");
        let _ = fs::remove_dir_all(&dir);
    }
}
