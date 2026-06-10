//! Parquet audit-stream writer per RFC 0005 §3.7.
//!
//! Mirrors [`crate::writer::Writer`] for the data stream but
//! writes to the **audit** Hive partition path computed by
//! [`PartitionKey::audit_path`] (which stops at `day=DD/` — the
//! audit partitioning is one axis coarser than the data
//! partitioning per §3.4's "audit volume is far lower"
//! rationale), uses [`audit_schema`] for the column shape, and
//! follows the §3.7 encoding-policy table for dictionary / page-
//! index overrides.
//!
//! Encoding policy per §3.7:
//! - ZSTD-3 compression on every column (§3.5 codec rule extends
//!   to the audit stream).
//! - Dictionary on globally; opt-out on `timestamp`,
//!   `old_template`, `new_template`, `triggering_line_hash`, and
//!   `triggering_line_sample` (the columns the §3.7 table marks
//!   `Dictionary = no`).
//! - Per-page statistics on globally so the Parquet page index is
//!   emitted for the §3.7 `Page index = yes` columns
//!   (`timestamp`, `event_kind`, `event_type`, `template_id`);
//!   downgraded to `EnabledStatistics::Chunk` for every other
//!   column.
//!
//! Row-vs-path agreement (§3.9) drops the `hour` axis on the
//! audit side: the audit partition path has no hour segment, so
//! the writer's per-event validation compares only `tenant_id`
//! plus `year` / `month` / `day` against the event's derived
//! partition. Two events emitted in the same day but different
//! hours land in the same audit file legitimately.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Datelike, Utc};
use ourios_core::audit::AuditEvent;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::errors::ParquetError;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::schema::types::ColumnPath;
use uuid::Uuid;

use crate::audit_record_batch::{AuditBatchError, audit_events_to_batch};
use crate::partition::PartitionKey;
use crate::writer::ROW_GROUP_FLUSH_BYTES;
use crate::{audit_columns, audit_schema};

/// Rows per internal sub-batch passed to `ArrowWriter::write`.
/// Audit volume is far lower than data volume; a smaller chunk
/// size keeps per-batch memory bounded for the common case. The
/// §3.5 row-group sizing invariant is still enforced via the
/// `in_progress_size()` flush below — defensive against
/// pathological-volume audit streams that could otherwise grow
/// a row group past the §3.5 1 GiB ceiling.
const SUB_BATCH_ROWS: usize = 256;

/// Streaming audit Parquet writer.
///
/// One [`AuditWriter`] writes one audit Parquet file. Atomic
/// publish mirrors [`crate::Writer`]: bytes are written to
/// `<uuid>.parquet.tmp` while the writer is open; [`Self::close`]
/// renames the temp file to the final `<uuid>.parquet` only after
/// the footer is written. Dropping without `close` removes the
/// temp file (the [`Drop`] impl is identical to the data side's).
///
/// The atomic publish is logical, not crash-durable — the same
/// caveat the data writer documents applies here.
pub struct AuditWriter {
    inner: Option<ArrowWriter<File>>,
    partition: PartitionKey,
    flush_uuid: Uuid,
    final_path: PathBuf,
    temp_path: Option<PathBuf>,
    /// Set to `true` once any `ArrowWriter::write` /
    /// `ArrowWriter::flush` call returns `Err`. The underlying
    /// `ArrowWriter`'s buffer state is undefined after such a
    /// failure (the row group may be partially written), so
    /// [`Self::close`] refuses to publish — renaming the temp file
    /// into place would silently land a potentially-corrupted
    /// audit file. The temp file is left on disk for diagnosis
    /// (the [`Drop`] impl skips its usual cleanup when poisoned).
    poisoned: bool,
}

impl AuditWriter {
    /// Open an audit writer for `partition` under `bucket_root`.
    /// Creates the audit partition directory (`audit/tenant_id=…/
    /// year=YYYY/month=MM/day=DD/`) and the UUIDv7-named file.
    ///
    /// # Errors
    ///
    /// - [`AuditWriterError::Io`] when the partition directory or
    ///   target file cannot be created.
    /// - [`AuditWriterError::Parquet`] when the ZSTD level is
    ///   rejected or `ArrowWriter` setup fails.
    pub fn open(bucket_root: &Path, partition: PartitionKey) -> Result<Self, AuditWriterError> {
        let dir = partition.audit_path(bucket_root);
        std::fs::create_dir_all(&dir).map_err(|source| AuditWriterError::Io {
            op: "create_dir_all",
            path: dir.clone(),
            source_path: None,
            source,
        })?;
        let flush_uuid = Uuid::now_v7();
        let final_path = dir.join(format!("{flush_uuid}.parquet"));
        let temp_path = dir.join(format!("{flush_uuid}.parquet.tmp"));
        let file = File::create(&temp_path).map_err(|source| AuditWriterError::Io {
            op: "create",
            path: temp_path.clone(),
            source_path: None,
            source,
        })?;

        let props = match audit_writer_properties() {
            Ok(p) => p,
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&temp_path);
                return Err(e);
            }
        };
        let inner = match ArrowWriter::try_new(file, audit_schema(), Some(props)) {
            Ok(w) => w,
            Err(e) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(AuditWriterError::Parquet(e));
            }
        };

        Ok(Self {
            inner: Some(inner),
            partition,
            flush_uuid,
            final_path,
            temp_path: Some(temp_path),
            poisoned: false,
        })
    }

    /// Append `events` to the writer. Validates each event's
    /// derived partition matches the writer's open partition on
    /// the tenant + year/month/day axes (audit partitioning has
    /// no hour segment per §3.4), converts the slice to a
    /// `RecordBatch`, and forwards to `ArrowWriter::write`.
    ///
    /// **Row-group sizing.** Mirrors the data writer's defensive
    /// flush: a check on `inner.in_progress_size()` before and
    /// after each sub-batch keeps row groups bounded by
    /// [`ROW_GROUP_FLUSH_BYTES`] + one sub-batch's worth of bytes,
    /// well under the §3.5 1 GiB ceiling. Audit volume is far
    /// lower than data volume in practice (the §3.7 "low-volume
    /// stream" framing), so the threshold rarely fires — but a
    /// pathological-volume audit run can't blow past §3.5 either.
    ///
    /// **Poisoning.** A failed `inner.write()` or `inner.flush()`
    /// leaves the underlying `ArrowWriter`'s buffer in an
    /// undefined state — the partial row group can't be safely
    /// recovered. When that happens, the writer marks itself
    /// poisoned and [`Self::close`] subsequently returns
    /// [`AuditWriterError::Poisoned`] instead of renaming the
    /// temp file into place. The `.parquet.tmp` stays on disk
    /// for diagnosis. `PartitionMismatch` and `Batch` errors do
    /// **not** poison: the writer remains usable for a follow-up
    /// `append_events` call.
    ///
    /// `append_events` is **not all-or-nothing** across the
    /// sub-batches it issues internally. The slice is chunked
    /// into `SUB_BATCH_ROWS`-sized pieces; if chunk *N* writes
    /// successfully and chunk *N+1*'s `audit_events_to_batch`
    /// then errors with `Batch`, the events from chunks `0..N`
    /// have already landed in the in-progress row group. Callers
    /// that want atomicity must pre-validate inputs (timestamps,
    /// rejection-variant template equality) before the first
    /// `append_events` call. `PartitionMismatch`, by contrast,
    /// *is* pre-checked across the whole slice before any writes
    /// happen, so it fires before chunk 0.
    ///
    /// # Errors
    ///
    /// - [`AuditWriterError::Poisoned`] when a prior
    ///   `append_events` already returned `Parquet`; fails fast
    ///   without touching `inner`.
    /// - [`AuditWriterError::PartitionMismatch`] when an event's
    ///   derived audit partition disagrees with the writer's
    ///   open partition. Pre-checked across the whole slice
    ///   before any `inner.write`. **Non-poisoning**.
    /// - [`AuditWriterError::Batch`] when `RecordBatch`
    ///   construction fails. **Non-poisoning**, but earlier
    ///   chunks in the same call may have written successfully
    ///   — see the atomicity note above.
    /// - [`AuditWriterError::Parquet`] when the underlying
    ///   Parquet writer rejects the batch or a row-group flush
    ///   fails. **Poisons the writer**; subsequent
    ///   `append_events` / `close` calls return `Poisoned`.
    ///
    /// # Panics
    ///
    /// Structurally impossible. The inner `ArrowWriter` is
    /// `Some` from [`Self::open`] until [`Self::close`] takes
    /// ownership of `self`; `append_events` borrows `&mut self`.
    pub fn append_events(&mut self, events: &[AuditEvent]) -> Result<(), AuditWriterError> {
        if self.poisoned {
            // Fail fast — touching `inner` after a prior Parquet
            // error would call into an `ArrowWriter` whose buffer
            // state is undefined. `close()` will refuse to publish
            // either way; surface the same `Poisoned` error here
            // so callers can stop driving the writer immediately
            // instead of accumulating further (potentially
            // doomed) Parquet operations.
            return Err(AuditWriterError::Poisoned);
        }
        if events.is_empty() {
            return Ok(());
        }
        for (idx, e) in events.iter().enumerate() {
            let derived = derive_audit_partition(e)?;
            if !audit_partition_matches(&derived, &self.partition) {
                return Err(AuditWriterError::PartitionMismatch {
                    row_index: idx,
                    expected: self.partition.clone(),
                    actual: derived,
                });
            }
        }
        let inner = self
            .inner
            .as_mut()
            .expect("inner ArrowWriter is Some until AuditWriter::close is called");
        // Run the Parquet-touching loop in a helper that takes a
        // `&mut ArrowWriter<File>` so the outer `self.poisoned =
        // true` assignment can run after the borrow on `self.inner`
        // ends. Poison only on Parquet errors — `Batch` errors
        // come from `audit_events_to_batch`, which runs on a
        // single chunk and doesn't touch `inner` itself; the
        // buffer's state at the moment a `Batch` error fires is
        // whatever earlier chunks left it (clean, or holding
        // already-written events from this same call). Either
        // way a follow-up `append_events` is safe — the contract
        // is "writer remains usable", not "no events persisted".
        let result = append_chunks(inner, events);
        if matches!(result, Err(AuditWriterError::Parquet(_))) {
            self.poisoned = true;
        }
        result
    }

    /// Close the writer, finalising the Parquet footer on the
    /// temp file and atomically renaming it to the final path.
    ///
    /// **Both fallible steps preserve the temp file on disk for
    /// diagnosis.** `inner` and `temp_path` are taken out of
    /// `self` *before* any fallible work; if either `inner.close()`
    /// (footer write) or `fs::rename` (atomic publish) then errors,
    /// `self.temp_path` is already `None` so the [`Drop`] impl
    /// won't delete the partially-written `.parquet.tmp`. This
    /// ordering is load-bearing — a failed `close` that destroyed
    /// its own artifact would be the worst-case failure mode,
    /// matching the data writer's [`crate::writer::Writer::close`]
    /// contract.
    ///
    /// **Poisoning check.** If a prior `append_events` returned a
    /// [`AuditWriterError::Parquet`] error, the writer is
    /// poisoned and this method refuses to publish — returns
    /// [`AuditWriterError::Poisoned`] without touching `inner` /
    /// `temp_path`, so the [`Drop`] impl's poisoned branch leaves
    /// the temp file on disk for diagnosis.
    ///
    /// # Errors
    ///
    /// - [`AuditWriterError::Poisoned`] when a prior
    ///   `append_events` failed with a Parquet error (temp file
    ///   left on disk).
    /// - [`AuditWriterError::Parquet`] when the footer write
    ///   fails (temp file left on disk).
    /// - [`AuditWriterError::Io`] when the atomic rename fails
    ///   (temp file left on disk).
    ///
    /// # Panics
    ///
    /// Structurally impossible. `inner` / `temp_path` are
    /// populated by [`Self::open`] and only consumed here;
    /// `close` takes `self` by value so it can't run twice.
    pub fn close(mut self) -> Result<AuditWrittenFile, AuditWriterError> {
        if self.poisoned {
            // Refuse to publish a possibly-partial file. Don't
            // take `inner` / `temp_path` — `Drop` sees `poisoned
            // = true` and leaves the .parquet.tmp on disk for
            // diagnosis. The Parquet handle is released as part
            // of the destructor cascade.
            return Err(AuditWriterError::Poisoned);
        }
        // Take both `inner` and `temp_path` BEFORE any fallible
        // work so that a failed `inner.close()` / `fs::rename`
        // leaves the `.parquet.tmp` on disk for diagnosis (the
        // [`Drop`] impl only removes the file when `temp_path`
        // is still `Some`). Matches the data writer's contract;
        // see the doc comment above.
        let inner = self
            .inner
            .take()
            .expect("AuditWriter::close consumes self; inner is Some on entry");
        let temp_path = self
            .temp_path
            .take()
            .expect("temp_path is Some until close consumes it");
        let metadata = inner.close().map_err(AuditWriterError::Parquet)?;
        std::fs::rename(&temp_path, &self.final_path).map_err(|source| AuditWriterError::Io {
            op: "rename",
            path: self.final_path.clone(),
            source_path: Some(temp_path.clone()),
            source,
        })?;
        Ok(AuditWrittenFile {
            path: self.final_path.clone(),
            partition: self.partition.clone(),
            flush_uuid: self.flush_uuid,
            num_rows: metadata.num_rows,
        })
    }

    /// Inspector for the absolute path the writer will publish
    /// to on close. While the writer is open the actual bytes
    /// live at `<this path>.tmp`.
    #[must_use]
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

impl Drop for AuditWriter {
    fn drop(&mut self) {
        if self.poisoned {
            // A poisoned writer preserves its `.parquet.tmp` for
            // diagnosis. Release the file handle (drop inner) but
            // leave the temp file on disk — `close()` already
            // returned `Poisoned` without consuming `temp_path`,
            // so it's still `Some(...)` here. We deliberately do
            // not `remove_file` it.
            drop(self.inner.take());
            return;
        }
        if let Some(temp) = self.temp_path.take() {
            drop(self.inner.take());
            let _ = std::fs::remove_file(&temp);
        }
    }
}

/// Result of a successful [`AuditWriter::close`].
#[derive(Debug)]
pub struct AuditWrittenFile {
    pub path: PathBuf,
    pub partition: PartitionKey,
    pub flush_uuid: Uuid,
    pub num_rows: i64,
}

/// Errors produced by [`AuditWriter`].
#[derive(Debug)]
pub enum AuditWriterError {
    Io {
        op: &'static str,
        path: PathBuf,
        source_path: Option<PathBuf>,
        source: io::Error,
    },
    Parquet(ParquetError),
    Batch(AuditBatchError),
    PartitionMismatch {
        row_index: usize,
        expected: PartitionKey,
        actual: PartitionKey,
    },
    /// A prior [`AuditWriter::append_events`] returned a
    /// `Parquet` error, leaving the underlying writer's buffer in
    /// an undefined state. [`AuditWriter::close`] refuses to
    /// publish to protect against landing a partial / corrupted
    /// audit file; the `.parquet.tmp` is preserved on disk for
    /// diagnosis.
    Poisoned,
}

impl fmt::Display for AuditWriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                op,
                path,
                source_path,
                source,
            } => match source_path {
                Some(src) => write!(
                    f,
                    "filesystem I/O on `{op}` from {} to {}: {source}",
                    src.display(),
                    path.display(),
                ),
                None => write!(
                    f,
                    "filesystem I/O on `{op}` at {}: {source}",
                    path.display(),
                ),
            },
            Self::Parquet(e) => write!(f, "parquet writer: {e}"),
            Self::Batch(e) => write!(f, "audit batch: {e}"),
            Self::PartitionMismatch {
                row_index,
                expected,
                actual,
            } => write!(
                f,
                "audit event at index {row_index} derives partition (tenant_id={}, \
                 year={:04}, month={:02}, day={:02}) which does not match the writer's open \
                 partition (tenant_id={}, year={:04}, month={:02}, day={:02}) — RFC 0005 §3.9 \
                 row-vs-path contract (audit axis: tenant + year/month/day; hour is ignored)",
                actual.tenant_id,
                actual.year,
                actual.month,
                actual.day,
                expected.tenant_id,
                expected.year,
                expected.month,
                expected.day,
            ),
            Self::Poisoned => write!(
                f,
                "AuditWriter is poisoned — a prior append_events failed with a Parquet \
                 error, leaving the buffer in an undefined state; close() refuses to \
                 publish to avoid landing a partial / corrupted file (the .parquet.tmp is \
                 preserved on disk for diagnosis)",
            ),
        }
    }
}

impl std::error::Error for AuditWriterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parquet(e) => Some(e),
            Self::Batch(e) => Some(e),
            Self::PartitionMismatch { .. } | Self::Poisoned => None,
        }
    }
}

/// Inner Parquet-touching loop of [`AuditWriter::append_events`].
/// Borrows the `ArrowWriter` directly so the caller can set
/// `self.poisoned = true` after the borrow ends if this returns
/// an `Err(AuditWriterError::Parquet(_))`. Per the §3.5 row-group
/// sizing rule, runs a `flush()` when the in-progress buffer
/// crosses [`ROW_GROUP_FLUSH_BYTES`] (128 MiB).
fn append_chunks(
    inner: &mut ArrowWriter<File>,
    events: &[AuditEvent],
) -> Result<(), AuditWriterError> {
    for chunk in events.chunks(SUB_BATCH_ROWS) {
        if inner.in_progress_size() >= ROW_GROUP_FLUSH_BYTES {
            inner.flush().map_err(AuditWriterError::Parquet)?;
        }
        let batch = audit_events_to_batch(chunk).map_err(AuditWriterError::Batch)?;
        inner.write(&batch).map_err(AuditWriterError::Parquet)?;
    }
    if inner.in_progress_size() >= ROW_GROUP_FLUSH_BYTES {
        inner.flush().map_err(AuditWriterError::Parquet)?;
    }
    Ok(())
}

/// Derive the [`PartitionKey`] for an audit event. Reuses the
/// data-side `PartitionKey` shape (which carries an `hour`
/// field); the audit path stops at `day`, so the `hour` field is
/// populated but ignored by [`audit_partition_matches`].
pub(crate) fn derive_audit_partition(event: &AuditEvent) -> Result<PartitionKey, AuditWriterError> {
    let nanos = event
        .timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| AuditWriterError::Batch(AuditBatchError::PreEpochTimestamp))?
        .as_nanos();
    let ns_i64 = i64::try_from(nanos)
        .map_err(|_| AuditWriterError::Batch(AuditBatchError::TimestampOverflow { nanos }))?;
    let dt = DateTime::<Utc>::from_timestamp_nanos(ns_i64);
    Ok(PartitionKey {
        tenant_id: event.tenant_id.as_str().to_owned(),
        year: dt.year(),
        month: dt.month(),
        day: dt.day(),
        hour: 0,
    })
}

/// Audit row-vs-path comparison: equal on tenant, year, month,
/// and day, ignoring `hour`. The audit partition path has no
/// hour segment, so events from any hour of the same day
/// legitimately land in the same file.
pub(crate) fn audit_partition_matches(a: &PartitionKey, b: &PartitionKey) -> bool {
    a.tenant_id == b.tenant_id && a.year == b.year && a.month == b.month && a.day == b.day
}

/// Build the [`WriterProperties`] that encode RFC 0005 §3.7's
/// per-column dictionary / page-index policy. ZSTD-3 codec on
/// every column matches the §3.5 rule.
fn audit_writer_properties() -> Result<WriterProperties, AuditWriterError> {
    let mut builder = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(3).map_err(AuditWriterError::Parquet)?,
        ))
        .set_dictionary_enabled(true)
        .set_statistics_enabled(EnabledStatistics::Page);

    // §3.7 `Dictionary = no` columns.
    for no_dict_col in [
        audit_columns::TIMESTAMP,
        audit_columns::OLD_TEMPLATE,
        audit_columns::NEW_TEMPLATE,
        audit_columns::TRIGGERING_LINE_HASH,
        audit_columns::TRIGGERING_LINE_SAMPLE,
    ] {
        builder = builder
            .set_column_dictionary_enabled(ColumnPath::new(vec![no_dict_col.to_string()]), false);
    }

    // §3.7 `Page index = no` columns — downgrade stats to chunk
    // so the per-page surface isn't emitted, but the per-chunk
    // min/max stays available for row-group pruning.
    for no_page_idx_col in [
        audit_columns::TENANT_ID,
        audit_columns::OLD_VERSION,
        audit_columns::NEW_VERSION,
        audit_columns::OLD_TEMPLATE,
        audit_columns::NEW_TEMPLATE,
        audit_columns::TRIGGERING_LINE_HASH,
        audit_columns::TRIGGERING_LINE_SAMPLE,
        audit_columns::REASON,
    ] {
        builder = builder.set_column_statistics_enabled(
            ColumnPath::new(vec![no_page_idx_col.to_string()]),
            EnabledStatistics::Chunk,
        );
    }
    // The `positions_widened` / `slots_expanded` "(list values)"
    // rows in the §3.7 table get `Page index = no` on the list
    // leaves. `positions_widened` is `LIST<INT32>` (one leaf);
    // `slots_expanded` is `LIST<STRUCT<INT32, LIST<INT32>>>` —
    // two integer leaves at slot_index and types_added.list.
    // element.
    builder = builder.set_column_statistics_enabled(
        ColumnPath::new(vec![
            audit_columns::POSITIONS_WIDENED.to_string(),
            "list".to_string(),
            "element".to_string(),
        ]),
        EnabledStatistics::Chunk,
    );
    builder = builder.set_column_statistics_enabled(
        ColumnPath::new(vec![
            audit_columns::SLOTS_EXPANDED.to_string(),
            "list".to_string(),
            "element".to_string(),
            "slot_index".to_string(),
        ]),
        EnabledStatistics::Chunk,
    );
    builder = builder.set_column_statistics_enabled(
        ColumnPath::new(vec![
            audit_columns::SLOTS_EXPANDED.to_string(),
            "list".to_string(),
            "element".to_string(),
            "types_added".to_string(),
            "list".to_string(),
            "element".to_string(),
        ]),
        EnabledStatistics::Chunk,
    );

    Ok(builder.build())
}
