// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless-mode support: rotating file log and atomic `status.json` writer.
//!
//! When the publisher runs with `--headless` (which implies `--no-ui`) all log
//! output is mirrored into `%LOCALAPPDATA%\SimRaceCenter\publisher\publisher.log`
//! (rotated at [`LOG_ROTATE_BYTES`]) and the live pipeline state is mirrored
//! into `status.json` in the same directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Rotate `publisher.log` once it would exceed this size.
pub const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// `%LOCALAPPDATA%\SimRaceCenter\publisher`, falling back to
/// `.\SimRaceCenter\publisher` when `LOCALAPPDATA` is unset.
/// The caller is responsible for `create_dir_all`.
pub fn data_dir() -> PathBuf {
    match std::env::var("LOCALAPPDATA") {
        Ok(base) if !base.is_empty() => Path::new(&base).join("SimRaceCenter").join("publisher"),
        _ => PathBuf::from(".").join("SimRaceCenter").join("publisher"),
    }
}

// ── File log ──────────────────────────────────────────────────────────────────

struct FileSink {
    file: File,
    path: PathBuf,
    current_len: u64,
    max_bytes: u64,
}

impl FileSink {
    fn append(&mut self, line: &str) -> io::Result<()> {
        let line_len = line.len() as u64;
        if self.current_len + line_len > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.current_len += line_len;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        let rotated = rotated_path(&self.path);
        // Windows refuses to rename a file with an open handle: swap the live
        // handle for a throwaway file, close it, rename, reopen.
        let swap = self.path.with_extension("log.swap");
        let temp = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&swap)?;
        drop(std::mem::replace(&mut self.file, temp));
        let _ = fs::remove_file(&rotated);
        fs::rename(&self.path, &rotated)?;
        self.file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.path)?;
        let _ = fs::remove_file(&swap);
        self.current_len = 0;
        Ok(())
    }
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

static SINK: OnceLock<Mutex<FileSink>> = OnceLock::new();

/// Install the global file sink. Subsequent [`log_line`] calls append to
/// `path` (rotating to `path.1` once `max_bytes` would be exceeded).
/// Only the first call has any effect.
pub fn init_file_log(path: &Path, max_bytes: u64) -> io::Result<()> {
    if SINK.get().is_some() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let current_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let sink = FileSink {
        file,
        path: path.to_path_buf(),
        current_len,
        max_bytes,
    };
    // Another thread may have raced us; first install wins.
    let _ = SINK.set(Mutex::new(sink));
    Ok(())
}

/// Emit `msg` to stderr always, and to the file sink when initialised.
pub fn log_line(level: &str, msg: &str) {
    eprintln!("{msg}");
    if let Some(sink) = SINK.get() {
        if let Ok(mut s) = sink.lock() {
            let line = format!("{} {:<5} {}\n", iso8601_utc(SystemTime::now()), level, msg);
            let _ = s.append(&line);
        }
    }
}

/// `log_info!("...")` — informational log line (stderr + file sink).
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::headless::log_line("INFO", &format!($($arg)*))
    };
}

/// `log_warn!("...")` — warning log line (stderr + file sink).
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::headless::log_line("WARN", &format!($($arg)*))
    };
}

// ── status.json ───────────────────────────────────────────────────────────────

/// Snapshot of publisher state written atomically to `status.json`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusFile {
    /// "starting" | "waiting_for_iracing" | "connected" | "stopped"
    pub state: String,
    pub destination: String,
    /// ISO-8601 UTC timestamp of this write.
    pub updated_at: String,
    pub last_post_at: Option<String>,
    pub last_error_kind: Option<String>,
    pub queued_events: usize,
    pub outbox_pending_batches: usize,
    pub sub_session_id: Option<i64>,
    pub events_enqueued_total: u64,
    pub events_delivered_total: u64,
    pub events_rejected_total: u64,
    pub events_duplicate_total: u64,
    pub events_lost_total: u64,
    pub calls_total: u64,
    pub calls_failed: u64,
    pub pid: u32,
    pub version: String,
}

/// Serialize `status` to a sibling `*.tmp` file, fsync, then rename over
/// `path` so readers never observe a partial file.
pub fn write_status_atomic(path: &Path, status: &StatusFile) -> io::Result<()> {
    let tmp = tmp_path(path);
    let json = serde_json::to_string_pretty(status).map_err(io::Error::other)?;
    {
        let mut f = File::create(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Format `t` as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC, no external date crate).
pub fn iso8601_utc(t: SystemTime) -> String {
    let duration = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();

    let days = (secs / 86_400) as i64;
    let secs_of = secs % 86_400;
    let (hh, mm, ss) = (secs_of / 3600, (secs_of % 3600) / 60, secs_of % 60);

    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y0 = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y0 + if m <= 2 { 1 } else { 0 };

    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_known_answer() {
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(iso8601_utc(t), "2023-11-14T22:13:20.000Z");
    }

    #[test]
    fn log_rotation() {
        // NOTE: the global OnceLock means this is the ONLY test allowed to
        // call init_file_log.
        let dir = std::env::temp_dir().join(format!(
            "dnc-logrot-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("publisher.log");
        init_file_log(&path, 200).unwrap();

        for i in 0..20 {
            log_line("INFO", &format!("line {i:02} padded to thirty chars"));
        }

        let rotated = dir.join("publisher.log.1");
        assert!(
            rotated.exists(),
            "publisher.log.1 should exist after rotation"
        );
        let current = fs::read_to_string(&path).unwrap();
        assert!(
            current.len() as u64 <= 200,
            "publisher.log should be at most max_bytes, got {}",
            current.len()
        );
        for line in current.lines() {
            assert!(
                line[..4].chars().all(|c| c.is_ascii_digit()),
                "line should start with a 4-digit year: {line:?}"
            );
            // Other tests share the global sink, so WARN lines are legal too.
            assert!(
                line.contains(" INFO ") || line.contains(" WARN "),
                "line should contain a level: {line:?}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_write_is_atomic_and_roundtrips() {
        let dir = std::env::temp_dir().join(format!(
            "dnc-status-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("status.json");

        let status = StatusFile {
            state: "connected".to_owned(),
            destination: "local".to_owned(),
            updated_at: iso8601_utc(SystemTime::now()),
            last_post_at: None,
            last_error_kind: None,
            queued_events: 3,
            outbox_pending_batches: 1,
            sub_session_id: Some(42),
            events_enqueued_total: 7,
            events_delivered_total: 4,
            events_rejected_total: 0,
            events_duplicate_total: 0,
            events_lost_total: 0,
            calls_total: 2,
            calls_failed: 0,
            pid: std::process::id(),
            version: "0.4.0".to_owned(),
        };

        write_status_atomic(&path, &status).unwrap();
        write_status_atomic(&path, &status).unwrap();

        assert!(!tmp_path(&path).exists(), "tmp file must not remain");
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["state"], "connected");
        assert_eq!(parsed["destination"], "local");
        assert_eq!(parsed["queuedEvents"], 3);
        assert_eq!(parsed["subSessionId"], 42);
        assert_eq!(parsed["pid"], status.pid);

        let _ = fs::remove_dir_all(&dir);
    }
}
