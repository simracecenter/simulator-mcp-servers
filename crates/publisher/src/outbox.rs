// SPDX-License-Identifier: GPL-3.0-or-later

//! Durable bounded outbox for ingest batches.
//!
//! Every batch the delivery worker posts is first persisted as
//! `batch-<seq>.json` under the publisher data directory, and the file is
//! deleted only after the ingest endpoint acknowledges the POST. A publisher
//! killed between send and acknowledgement re-delivers the file on its next
//! start; the receiver deduplicates re-deliveries by event `id`.
//!
//! The outbox is bounded: once more than `max_files` batches are pending the
//! oldest files are discarded and the drop is recorded in
//! [`Outbox::dropped_batches_total`] / [`Outbox::dropped_events_total`] so the
//! loss is visible in `status.json` rather than silent.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const BATCH_PREFIX: &str = "batch-";
const BATCH_SUFFIX: &str = ".json";
const TMP_SUFFIX: &str = ".tmp";
const CORRUPT_SUFFIX: &str = ".corrupt";

/// File-backed FIFO of serialized ingest request bodies.
pub struct Outbox {
    dir: PathBuf,
    max_files: usize,
    /// Pending batch files, oldest first.
    pending: VecDeque<PathBuf>,
    next_seq: u64,
    dropped_batches_total: u64,
    dropped_events_total: u64,
}

impl Outbox {
    /// Open `dir` (creating it if needed) and recover every unacknowledged
    /// batch left by a previous run, oldest first. Half-written `.tmp` files
    /// are deleted — a batch is renamed into place only once fully synced.
    pub fn open(dir: &Path, max_files: usize) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let mut files: Vec<(u64, PathBuf)> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(TMP_SUFFIX) {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            if let Some(seq) = parse_batch_seq(&name) {
                files.push((seq, entry.path()));
            }
        }
        files.sort_by_key(|(seq, _)| *seq);
        let next_seq = files.last().map(|(seq, _)| seq + 1).unwrap_or(0);
        let mut outbox = Self {
            dir: dir.to_path_buf(),
            max_files,
            pending: files.into_iter().map(|(_, path)| path).collect(),
            next_seq,
            dropped_batches_total: 0,
            dropped_events_total: 0,
        };
        outbox.enforce_bound();
        Ok(outbox)
    }

    /// Persist `body` (a serialized ingest request) as the next pending batch.
    pub fn store(&mut self, body: &serde_json::Value) -> io::Result<()> {
        let seq = self.next_seq;
        let final_path = self.dir.join(batch_name(seq));
        let tmp_path = self.dir.join(format!("batch-{seq:020}{TMP_SUFFIX}"));
        let bytes = serde_json::to_vec(body).map_err(io::Error::other)?;
        {
            let mut file = File::create(&tmp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp_path, &final_path)?;
        self.next_seq = seq + 1;
        self.pending.push_back(final_path);
        self.enforce_bound();
        Ok(())
    }

    /// Number of batches still awaiting acknowledgement.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Path of the oldest pending batch, if any.
    pub fn front(&self) -> Option<&Path> {
        self.pending.front().map(PathBuf::as_path)
    }

    /// Total bytes held by pending batch files (best effort).
    pub fn pending_bytes(&self) -> u64 {
        self.pending
            .iter()
            .map(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
            .sum()
    }

    /// Batches dropped because the outbox was already at `max_files`.
    pub fn dropped_batches_total(&self) -> u64 {
        self.dropped_batches_total
    }

    /// Events inside dropped batches (best effort — an unreadable file counts
    /// as one lost batch and zero known events).
    pub fn dropped_events_total(&self) -> u64 {
        self.dropped_events_total
    }

    /// Read and parse the oldest pending batch body.
    pub fn read_front(&self) -> io::Result<serde_json::Value> {
        let path = self
            .front()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "outbox is empty"))?;
        let text = fs::read_to_string(path)?;
        serde_json::from_str(&text).map_err(io::Error::other)
    }

    /// Drop the front file after a successful POST. The file is removed from
    /// the pending list even if deletion fails — a leftover file is at most a
    /// duplicate re-delivery on the next start, while keeping it pending would
    /// re-post forever.
    pub fn ack_front(&mut self) {
        if let Some(path) = self.pending.pop_front() {
            if let Err(e) = fs::remove_file(&path) {
                crate::log_warn!("[outbox] could not delete {}: {e}", path.display());
            }
        }
    }

    /// Quarantine an undeliverable front file (unreadable or unparseable) so a
    /// single corrupt write cannot stall all later deliveries. Returns the
    /// number of events known to have been lost (0 when the file could not be
    /// parsed at all — the batch is still counted).
    pub fn quarantine_front(&mut self) -> u64 {
        let Some(path) = self.pending.pop_front() else {
            return 0;
        };
        let events = event_count(&path).unwrap_or(0);
        self.dropped_batches_total += 1;
        self.dropped_events_total += events;
        let mut quarantined = path.as_os_str().to_os_string();
        quarantined.push(CORRUPT_SUFFIX);
        let quarantined = PathBuf::from(quarantined);
        if let Err(e) = fs::rename(&path, &quarantined) {
            crate::log_warn!("[outbox] could not quarantine {}: {e}", path.display());
            let _ = fs::remove_file(&path);
        } else {
            crate::log_warn!(
                "[outbox] quarantined corrupt batch {}",
                quarantined.display()
            );
        }
        events
    }

    fn enforce_bound(&mut self) {
        while self.pending.len() > self.max_files {
            let Some(path) = self.pending.pop_front() else {
                break;
            };
            let events = event_count(&path).unwrap_or(0);
            self.dropped_batches_total += 1;
            self.dropped_events_total += events;
            crate::log_warn!(
                "[outbox] full ({}) — dropping oldest batch {} ({} events)",
                self.max_files,
                path.display(),
                events
            );
            if let Err(e) = fs::remove_file(&path) {
                crate::log_warn!("[outbox] could not drop {}: {e}", path.display());
            }
        }
    }
}

fn batch_name(seq: u64) -> String {
    format!("batch-{seq:020}{BATCH_SUFFIX}")
}

fn parse_batch_seq(name: &str) -> Option<u64> {
    name.strip_prefix(BATCH_PREFIX)?
        .strip_suffix(BATCH_SUFFIX)?
        .parse()
        .ok()
}

fn event_count(path: &Path) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    let body: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(body.get("events")?.as_array()?.len() as u64)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "src-outbox-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn body(n_events: usize) -> serde_json::Value {
        serde_json::json!({
            "subSessionId": 1,
            "sessionTime": 1.0,
            "sessionTick": 1,
            "events": (0..n_events).map(|i| serde_json::json!({"id": i})).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn store_ack_and_recovery_order() {
        let dir = temp_dir("recovery");
        {
            let mut outbox = Outbox::open(&dir, 10).unwrap();
            outbox.store(&body(2)).unwrap();
            outbox.store(&body(3)).unwrap();
            assert_eq!(outbox.pending_len(), 2);
            // Crash without ack — both files survive.
        }
        let mut outbox = Outbox::open(&dir, 10).unwrap();
        assert_eq!(outbox.pending_len(), 2);
        let front = outbox.read_front().unwrap();
        assert_eq!(front["events"].as_array().unwrap().len(), 2);
        outbox.ack_front();
        let front = outbox.read_front().unwrap();
        assert_eq!(front["events"].as_array().unwrap().len(), 3);
        outbox.ack_front();
        assert_eq!(outbox.pending_len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bound_drops_oldest_and_counts_events() {
        let dir = temp_dir("bound");
        let mut outbox = Outbox::open(&dir, 2).unwrap();
        outbox.store(&body(1)).unwrap();
        outbox.store(&body(2)).unwrap();
        outbox.store(&body(4)).unwrap();
        assert_eq!(outbox.pending_len(), 2);
        assert_eq!(outbox.dropped_batches_total(), 1);
        assert_eq!(outbox.dropped_events_total(), 1);
        // The surviving files are the two newest.
        let front = outbox.read_front().unwrap();
        assert_eq!(front["events"].as_array().unwrap().len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_tmp_files_are_discarded_on_open() {
        let dir = temp_dir("tmp");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("batch-00000000000000000003.tmp"),
            b"{\"events\":[]}",
        )
        .unwrap();
        let mut outbox = Outbox::open(&dir, 10).unwrap();
        assert_eq!(outbox.pending_len(), 0);
        outbox.store(&body(1)).unwrap();
        // next_seq starts at 0 — nothing recovered — and must not collide.
        assert_eq!(outbox.pending_len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn quarantine_moves_corrupt_front_file() {
        let dir = temp_dir("corrupt");
        {
            let mut outbox = Outbox::open(&dir, 10).unwrap();
            outbox.store(&body(2)).unwrap();
            outbox.store(&body(3)).unwrap();
        }
        // Corrupt the oldest file.
        let oldest = {
            let outbox = Outbox::open(&dir, 10).unwrap();
            outbox.front().unwrap().to_path_buf()
        };
        fs::write(&oldest, b"not json").unwrap();

        let mut outbox = Outbox::open(&dir, 10).unwrap();
        assert!(outbox.read_front().is_err());
        outbox.quarantine_front();
        assert_eq!(outbox.dropped_batches_total(), 1);
        assert_eq!(outbox.dropped_events_total(), 0); // unreadable → unknown
        let front = outbox.read_front().unwrap();
        assert_eq!(front["events"].as_array().unwrap().len(), 3);
        assert!(dir.join("batch-00000000000000000000.json.corrupt").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
