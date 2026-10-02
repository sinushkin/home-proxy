//! Запись строк в CSV с ротацией по размеру. Формат — свой (не внешний csv-крейт): столбцы
//! фиксированы и известны заранее, поля не содержат запятых/переводов строк (числа, короткие
//! слова), так что простой `join(",")` безопасен и быстрее универсального форматтера.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::inner::InnerInfo;
use crate::record::{MsgKind, SentRecord};

/// Строка CSV: один отправленный вниз пакет, после подтверждения или списания по тайм-ауту.
/// Схема и смысл столбцов — PLAN-ML.md, раздел «Схема CSV» (упрощённая до миллисекундной
/// точности времени — доступное разрешение часов в этой реализации, см. журнал плана).
pub struct Row<'a> {
    pub pid: u32,
    pub t_send_unix_ms: u64,
    pub delivered: bool,
    /// `None`, если не доставлен.
    pub flight_ms: Option<i64>,
    pub ack_rtt_ms: Option<u64>,
    pub recv_gap_ms: Option<u64>,
    pub reorder_wait_ms: Option<u16>,
    pub out_of_order: Option<bool>,
    pub slot: u32,
    pub via_relay: bool,
    pub local_port: u16,
    pub dst_port: u16,
    pub wire_len: u16,
    pub payload_len: u16,
    pub inner: &'a InnerInfo,
    pub kind: MsgKind,
    pub client_id: Option<u8>,
    pub flow: Option<u32>,
    pub hole_age_ms: u32,
    pub send_gap_ms: Option<u64>,
}

pub const HEADER: &str = "pid,t_send_unix_ms,delivered,flight_ms,ack_rtt_ms,recv_gap_ms,reorder_wait_ms,out_of_order,slot,via_relay,local_port,dst_port,wire_len,payload_len,inner_proto,inner_sport,inner_dport,kind,client_id,flow,hole_age_ms,send_gap_ms";

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

fn bool01(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

impl Row<'_> {
    /// Строка целиком, без завершающего перевода строки.
    pub fn to_line(&self) -> String {
        let (sport, dport) = self.inner.ports.map_or((None, None), |(s, d)| (Some(s), Some(d)));
        [
            self.pid.to_string(),
            self.t_send_unix_ms.to_string(),
            bool01(self.delivered).to_string(),
            opt(self.flight_ms),
            opt(self.ack_rtt_ms),
            opt(self.recv_gap_ms),
            opt(self.reorder_wait_ms),
            opt(self.out_of_order.map(bool01)),
            self.slot.to_string(),
            bool01(self.via_relay).to_string(),
            self.local_port.to_string(),
            self.dst_port.to_string(),
            self.wire_len.to_string(),
            self.payload_len.to_string(),
            self.inner.proto.as_str().to_string(),
            opt(sport),
            opt(dport),
            self.kind.as_str().to_string(),
            opt(self.client_id),
            opt(self.flow),
            self.hole_age_ms.to_string(),
            opt(self.send_gap_ms),
        ]
        .join(",")
    }

    /// Строка доставленного пакета.
    pub fn delivered(
        record: &SentRecord,
        flight_ms: i64,
        ack_rtt_ms: u64,
        recv_gap_ms: Option<u64>,
        reorder_wait_ms: u16,
        out_of_order: bool,
        send_gap_ms: Option<u64>,
    ) -> Row<'_> {
        Row {
            pid: record.pid,
            t_send_unix_ms: record.t_send_unix_ms,
            delivered: true,
            flight_ms: Some(flight_ms),
            ack_rtt_ms: Some(ack_rtt_ms),
            recv_gap_ms,
            reorder_wait_ms: Some(reorder_wait_ms),
            out_of_order: Some(out_of_order),
            slot: record.slot,
            via_relay: record.via_relay,
            local_port: record.local_port,
            dst_port: record.dst_port,
            wire_len: record.wire_len,
            payload_len: record.payload_len,
            inner: &record.inner,
            kind: record.kind,
            client_id: record.client_id,
            flow: record.flow,
            hole_age_ms: record.hole_age_ms,
            send_gap_ms,
        }
    }

    /// Строка пакета, списанного по тайм-ауту (подтверждение не пришло).
    pub fn lost(record: &SentRecord, send_gap_ms: Option<u64>) -> Row<'_> {
        Row {
            pid: record.pid,
            t_send_unix_ms: record.t_send_unix_ms,
            delivered: false,
            flight_ms: None,
            ack_rtt_ms: None,
            recv_gap_ms: None,
            reorder_wait_ms: None,
            out_of_order: None,
            slot: record.slot,
            via_relay: record.via_relay,
            local_port: record.local_port,
            dst_port: record.dst_port,
            wire_len: record.wire_len,
            payload_len: record.payload_len,
            inner: &record.inner,
            kind: record.kind,
            client_id: record.client_id,
            flow: record.flow,
            hole_age_ms: record.hole_age_ms,
            send_gap_ms,
        }
    }
}

/// Writer с ротацией: новый файл, когда текущий превышает `max_file_bytes`, с заголовком в
/// начале каждого файла. Имя — `down-<unix-мс начала файла>.csv`.
pub struct RotatingCsv {
    dir: PathBuf,
    max_file_bytes: u64,
    file: BufWriter<File>,
    written: u64,
}

impl RotatingCsv {
    pub fn create(dir: &Path, max_file_bytes: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let (file, written) = Self::open(dir)?;
        Ok(Self { dir: dir.to_path_buf(), max_file_bytes, file, written })
    }

    fn open(dir: &Path) -> io::Result<(BufWriter<File>, u64)> {
        // Счётчик вдобавок к времени: ротация может случиться дважды в одну и ту же миллисекунду
        // (маленький `max_file_bytes`, как в тесте) — без него файлы совпадали бы по имени.
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("down-{}-{n}.csv", now_unix_ms());
        let raw = OpenOptions::new().create(true).write(true).truncate(true).open(dir.join(name))?;
        let mut file = BufWriter::new(raw);
        file.write_all(HEADER.as_bytes())?;
        file.write_all(b"\n")?;
        Ok((file, HEADER.len() as u64 + 1))
    }

    fn open_new(&mut self) -> io::Result<()> {
        let (file, written) = Self::open(&self.dir)?;
        self.file = file;
        self.written = written;
        Ok(())
    }

    pub fn write_row(&mut self, row: &Row) {
        let line = row.to_line();
        if let Err(e) = self.file.write_all(line.as_bytes()).and_then(|()| self.file.write_all(b"\n")) {
            log::warn!("hp-stats: запись строки CSV не удалась: {e:#}");
            return;
        }
        self.written += line.len() as u64 + 1;
        if self.written >= self.max_file_bytes {
            let _ = self.file.flush();
            if let Err(e) = self.open_new() {
                log::warn!("hp-stats: не удалось открыть новый файл при ротации: {e:#}");
            }
        }
    }

    pub fn flush(&mut self) {
        let _ = self.file.flush();
    }
}

fn now_unix_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inner::Proto;

    fn sample() -> SentRecord {
        SentRecord {
            pid: 42,
            t_send_unix_ms: 1_000,
            slot: 3,
            via_relay: false,
            local_port: 40010,
            dst_port: 51000,
            wire_len: 120,
            payload_len: 100,
            inner: InnerInfo { proto: Proto::Tcp, ports: Some((443, 55000)) },
            kind: MsgKind::Ordered,
            client_id: None,
            flow: Some(7),
            hole_age_ms: 5_000,
        }
    }

    #[test]
    fn delivered_row_has_all_fields_filled() {
        let record = sample();
        let row = Row::delivered(&record, 25, 30, Some(10), 5, true, Some(2));
        let line = row.to_line();
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(fields.len(), HEADER.split(',').count());
        assert_eq!(fields[0], "42", "pid");
        assert_eq!(fields[2], "1", "delivered");
        assert_eq!(fields[3], "25", "flight_ms");
        assert_eq!(fields[14], "tcp", "inner_proto");
        assert_eq!(fields[15], "443", "inner_sport");
        assert_eq!(fields[16], "55000", "inner_dport");
        assert_eq!(fields[17], "ordered", "kind");
        assert_eq!(fields[19], "7", "flow");
    }

    #[test]
    fn lost_row_leaves_delivery_fields_empty() {
        let record = sample();
        let row = Row::lost(&record, None);
        let line = row.to_line();
        let fields: Vec<&str> = line.split(',').collect();
        assert_eq!(fields[2], "0", "delivered");
        assert_eq!(fields[3], "", "flight_ms пуст");
        assert_eq!(fields[4], "", "ack_rtt_ms пуст");
        assert_eq!(fields[6], "", "reorder_wait_ms пуст");
    }

    #[test]
    fn rotation_starts_a_new_file_with_header() {
        let tmp = std::env::temp_dir().join(format!("hp-stats-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let mut writer = RotatingCsv::create(&tmp, 64).unwrap();
        let record = sample();
        for _ in 0..5 {
            writer.write_row(&Row::lost(&record, None));
        }
        writer.flush();
        let mut files: Vec<_> = fs::read_dir(&tmp).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        files.retain(|f| f.to_string_lossy().starts_with("down-"));
        assert!(files.len() >= 2, "маленький лимит должен был заставить ротацию: файлов {}", files.len());
        let _ = fs::remove_dir_all(&tmp);
    }
}
