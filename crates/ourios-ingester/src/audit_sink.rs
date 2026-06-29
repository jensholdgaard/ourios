//! Buffering audit sink for the ingest path (issue #302).
//!
//! The Drain miner emits a template audit event (`template_created` /
//! `template_widened` / `template_type_expanded`, RFC 0001 §6.4 / RFC 0017
//! §3.1) synchronously from `MinerCluster::ingest`, which runs on the
//! receiver's async request path. The querier's read-time template registry
//! (RFC 0017 `derive_template_registry`) folds those events to reconstruct a
//! clean row's body bit-for-bit (`CLAUDE.md` §3.3); without them a clean,
//! high-confidence row — whose `body` column is intentionally empty — renders
//! to nothing.
//!
//! [`ourios_parquet::ParquetAuditSink`] persists one Parquet file per event
//! with a blocking store write inside `emit`, so wiring it into the receiver
//! would both stall the request path on store latency and spray one tiny
//! object per template event (hazard `CLAUDE.md` §4 #4, the small-file
//! problem). This sink instead mirrors the RFC 0014 record sink
//! ([`crate::record_sink::ParquetRecordSink`]): [`AuditSink::emit`] only
//! buffers (a cheap, request-path-safe push), and the blocking store I/O
//! happens on [`SharedParquetAuditSink::flush`], which the receiver drives off
//! the async runtime at the same cadence + rotation + shutdown points it
//! flushes the record sink.
//!
//! **Flush ordering (no later than the records).** A row stamped
//! `template_version = N` needs its template event durable for the registry to
//! render it, so the receiver flushes this sink *before* the record sink at
//! each cadence point, and gates the miner snapshot on this sink draining too:
//! a flush failure retains the events (the WAL is the durability of record, so
//! recovery re-mines and re-emits them) rather than dropping them. The inline
//! size trigger on the record sink (RFC 0014 §3.1, fired from `emit`) is *not*
//! paired with an audit flush — a clean row flushed by that trigger may render
//! empty in the narrow window before the next audit flush; it reconstructs once
//! the audit cadence catches up, and a crash in that window re-mines from the
//! WAL, so durability is unaffected.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use ourios_core::audit::{AuditEvent, AuditSink};
use ourios_parquet::{AuditWriter, AuditWriterError, PartitionKey, Store, derive_audit_partition};

/// A buffering [`AuditSink`] persisting the RFC 0005 §3.7 audit Parquet stream
/// through the [`Store`] (local or S3, RFC 0019).
///
/// [`AuditSink::emit`] buffers in memory; [`Self::flush`] drains the buffer,
/// groups the events by audit partition (tenant + UTC day, RFC 0005 §3.4), and
/// writes each partition's batch with a single [`AuditWriter`] — so one flush
/// produces at most one file per partition, not one per event.
pub struct BufferingAuditSink {
    store: Store,
    buffer: Vec<AuditEvent>,
    flushes: u64,
    events_flushed: u64,
    flush_errors: u64,
    derive_errors: u64,
}

impl BufferingAuditSink {
    /// A sink buffering events and flushing the audit stream through `store`
    /// (local or S3).
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self {
            store,
            buffer: Vec::new(),
            flushes: 0,
            events_flushed: 0,
            flush_errors: 0,
            derive_errors: 0,
        }
    }

    /// Events currently buffered (not yet flushed).
    #[must_use]
    pub fn buffered_events(&self) -> usize {
        self.buffer.len()
    }

    /// Count of successful partition flushes (one per partition per [`Self::flush`]).
    #[must_use]
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// Total events written out across all successful flushes.
    #[must_use]
    pub fn events_flushed(&self) -> u64 {
        self.events_flushed
    }

    /// Drain the buffer and persist it, one [`AuditWriter`] per audit partition.
    ///
    /// A partition whose write fails has its events retained for the next flush
    /// (the WAL is the durability of record); an event whose timestamp can't
    /// derive a partition (pre-epoch / overflow) is counted and dropped, since
    /// retrying it would loop forever. An empty buffer is a no-op — no writer is
    /// opened and no file is published.
    pub fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let mut groups: HashMap<PartitionKey, Vec<AuditEvent>> = HashMap::new();
        for event in std::mem::take(&mut self.buffer) {
            match derive_audit_partition(&event) {
                Ok(partition) => groups.entry(partition).or_default().push(event),
                Err(_) => self.derive_errors = self.derive_errors.saturating_add(1),
            }
        }
        for (partition, batch) in groups {
            if self.write_partition(&partition, &batch).is_err() {
                self.flush_errors = self.flush_errors.saturating_add(1);
                self.buffer.extend(batch);
            }
        }
    }

    /// Write one partition's batch through a single [`AuditWriter`]. On success
    /// the counters advance; on error the caller retains the batch.
    fn write_partition(
        &mut self,
        partition: &PartitionKey,
        batch: &[AuditEvent],
    ) -> Result<(), AuditWriterError> {
        let mut writer = AuditWriter::open_in(&self.store, partition.clone())?;
        writer.append_events(batch)?;
        writer.close()?;
        self.flushes = self.flushes.saturating_add(1);
        self.events_flushed = self.events_flushed.saturating_add(batch.len() as u64);
        Ok(())
    }
}

/// A cloneable handle to one shared [`BufferingAuditSink`].
///
/// Mirrors [`crate::record_sink::SharedParquetSink`]: the ingest path has two
/// holders of the same sink — the miner `emit`s template events through its
/// `Box<dyn AuditSink>`, while the receiver drives [`Self::flush`] off the async
/// runtime at the cadence / rotation / shutdown points. `Clone` yields another
/// handle to the *same* sink.
///
/// All access serializes on one mutex. `emit` is a short critical section;
/// `flush` is **not** — it holds the lock across `AuditWriter` encode +
/// `Store` put (an S3 PUT on the s3 backend), so the receiver runs it via
/// `block_in_place` / `spawn_blocking`, the same posture the record sink takes.
#[derive(Clone)]
pub struct SharedParquetAuditSink {
    inner: Arc<Mutex<BufferingAuditSink>>,
}

impl SharedParquetAuditSink {
    /// Wrap `sink` in a shared, cloneable handle.
    #[must_use]
    pub fn new(sink: BufferingAuditSink) -> Self {
        Self {
            inner: Arc::new(Mutex::new(sink)),
        }
    }

    /// Lock the sink, recovering a poisoned mutex. A poison means a past panic
    /// while a flush was in flight; the buffer + counters remain structurally
    /// consistent (the WAL is the durability of record), so recovering the
    /// inner sink is safer than panicking the ingest path (the same posture
    /// [`crate::record_sink::SharedParquetSink`] takes).
    fn lock(&self) -> std::sync::MutexGuard<'_, BufferingAuditSink> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drain the buffer to the audit stream — the cadence / rotation / shutdown
    /// trigger, run off the async runtime by the receiver.
    pub fn flush(&self) {
        self.lock().flush();
    }

    /// Events currently buffered (not yet flushed) — observability + tests.
    #[must_use]
    pub fn buffered_events(&self) -> usize {
        self.lock().buffered_events()
    }

    /// Successful partition flushes so far — observability + tests.
    #[must_use]
    pub fn flushes(&self) -> u64 {
        self.lock().flushes()
    }
}

impl AuditSink for SharedParquetAuditSink {
    fn emit(&mut self, event: AuditEvent) {
        self.lock().buffer.push(event);
    }
}

impl AuditSink for BufferingAuditSink {
    fn emit(&mut self, event: AuditEvent) {
        self.buffer.push(event);
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, UNIX_EPOCH};

    use ourios_core::audit::{
        AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
    };
    use ourios_core::tenant::TenantId;
    use ourios_parquet::{AuditReader, Store};

    use super::{BufferingAuditSink, SharedParquetAuditSink};

    fn created_event(tenant: &str, template_id: u64, template: &str) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new(tenant),
            // 2026-04-02T10:58:00Z — a fixed, in-window day.
            timestamp: UNIX_EPOCH + Duration::from_secs(1_775_127_480),
            payload: AuditPayload::Template {
                template_id,
                triggering_line_hash: hash_triggering_line(template.as_bytes()),
                triggering_line_sample: Some(template.to_owned()),
                change: TemplateChange::Created {
                    new_template: template.to_owned(),
                },
            },
        }
    }

    /// Every `*.parquet` audit file under `root`, recursively.
    fn parquet_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "parquet") {
                    out.push(path);
                }
            }
        }
        out
    }

    /// Every audit event published under `root`, read back through the reader,
    /// paired with the file it came from.
    fn read_back(root: &Path) -> Vec<(PathBuf, AuditEvent)> {
        let mut out = Vec::new();
        for file in parquet_files(root) {
            let events = AuditReader::open_file(&file)
                .expect("open_file")
                .read_all()
                .expect("read_all");
            for event in events {
                out.push((file.clone(), event));
            }
        }
        out
    }

    #[test]
    fn flush_batches_per_partition_and_round_trips() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let handle = SharedParquetAuditSink::new(BufferingAuditSink::new(store));

        // Three events across two tenants → two audit partitions.
        let events = [
            created_event("alpha", 1, "user <*> logged in"),
            created_event("alpha", 2, "GET <*>"),
            created_event("bravo", 1, "order <*> shipped"),
        ];
        let mut producer = handle.clone();
        for event in &events {
            producer.emit(event.clone());
        }
        assert_eq!(handle.buffered_events(), 3, "clones share one buffer");
        assert_eq!(handle.flushes(), 0, "no trigger fired yet");

        handle.flush();
        assert_eq!(handle.buffered_events(), 0, "flush drained the buffer");
        // One file per partition, not one per event (the small-file guard).
        assert_eq!(handle.flushes(), 2, "two partitions → two files");

        let read = read_back(dir.path());
        let distinct_files: std::collections::HashSet<&PathBuf> =
            read.iter().map(|(k, _)| k).collect();
        assert_eq!(distinct_files.len(), 2, "exactly two audit files written");
        let mut got: Vec<AuditEvent> = read.into_iter().map(|(_, e)| e).collect();
        let mut want: Vec<AuditEvent> = events.to_vec();
        let sort = |v: &mut Vec<AuditEvent>| {
            v.sort_by_key(|e| match &e.payload {
                AuditPayload::Template { template_id, .. } => {
                    (e.tenant_id.as_str().to_owned(), *template_id)
                }
                _ => (e.tenant_id.as_str().to_owned(), 0),
            });
        };
        sort(&mut got);
        sort(&mut want);
        assert_eq!(
            got, want,
            "every emitted event round-trips out of the stream"
        );
    }

    #[test]
    fn empty_buffer_flush_is_a_no_op() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let handle = SharedParquetAuditSink::new(BufferingAuditSink::new(store.clone()));

        handle.flush();

        assert_eq!(handle.flushes(), 0, "no flush happened");
        assert!(
            store.list_blocking(None).expect("list").is_empty(),
            "no object is published on an empty-buffer flush",
        );
    }
}
