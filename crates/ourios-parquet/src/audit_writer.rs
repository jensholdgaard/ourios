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
use crate::store::Store;
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

/// Buffer-and-put audit Parquet writer (RFC 0013 object-storage
/// seam; RFC 0019 §3 the audit sink's S3 migration).
///
/// One [`AuditWriter`] writes one audit Parquet file. Atomic
/// publish mirrors [`crate::Writer`]: events are encoded into an
/// in-memory buffer as they arrive; [`Self::close`] writes the
/// finished bytes to the object store under the partition's key.
/// Nothing is published until that `put` — a writer dropped without
/// [`Self::close`] (panic, early-return) simply discards its buffer.
///
/// [`AuditWriter::open`] opens a local-filesystem [`Store`] rooted at a
/// `bucket_root` path; [`AuditWriter::open_in`] takes an already-built
/// [`Store`] so the same writer targets S3 (the compaction audit sink's
/// RFC 0019 path).
///
/// The atomic publish is logical, not crash-durable — the same
/// caveat the data writer documents applies here.
pub struct AuditWriter {
    inner: Option<ArrowWriter<Vec<u8>>>,
    partition: PartitionKey,
    flush_uuid: Uuid,
    /// Object store rooted at `bucket_root`; the finished file is
    /// `put` to [`Self::key`] on close.
    store: Store,
    /// `/`-delimited object key the file is published to, relative to
    /// the store root (`audit/tenant_id=…/year=…/…/<uuid>.parquet`).
    key: String,
    /// Absolute local landing path ([`AuditWriter::open`]); the object key
    /// rendered as a path for [`AuditWriter::open_in`] (which addresses by key
    /// regardless of the store's backend). Surfaced in [`AuditWrittenFile::path`].
    final_path: PathBuf,
    /// Running count of rows written so far (incremented per
    /// sub-batch as each `write` succeeds); reported by
    /// [`Self::close`]. Tracked directly because `into_inner` returns
    /// the buffer, not file metadata.
    num_rows: i64,
    /// Set to `true` once any `ArrowWriter::write` /
    /// `ArrowWriter::flush` call returns `Err`. The underlying
    /// `ArrowWriter`'s buffer state is undefined after such a
    /// failure (the row group may be partially written), so
    /// [`Self::close`] refuses to publish — putting a potentially
    /// corrupted buffer would land a bad audit file. The buffer is
    /// discarded (there is no on-disk artifact to inspect).
    poisoned: bool,
}

impl AuditWriter {
    /// Open an audit writer for `partition` under `bucket_root` — the
    /// **local-filesystem** constructor. Creates the audit partition
    /// directory (`audit/tenant_id=…/year=YYYY/month=MM/day=DD/`); the
    /// `<UUIDv7>.parquet` object itself is buffer-and-put — events accumulate in
    /// memory via [`AuditWriter::append_events`] and nothing is published until
    /// [`AuditWriter::close`].
    ///
    /// [`AuditWriter::open_in`] takes an already-built [`Store`] instead, so a
    /// writer can target S3 (the compaction audit sink's path under RFC 0019).
    ///
    /// # Errors
    ///
    /// - [`AuditWriterError::Io`] when the partition directory can't be
    ///   created or the object store can't be opened at `bucket_root`.
    /// - [`AuditWriterError::Parquet`] when the ZSTD level is
    ///   rejected or `ArrowWriter` setup fails.
    pub fn open(bucket_root: &Path, partition: PartitionKey) -> Result<Self, AuditWriterError> {
        // Ensure the store root (and the partition dir) exist:
        // `Store::local` canonicalises `bucket_root`, which must therefore
        // exist; the object-store `put` on close creates any remaining parents.
        let dir = partition.audit_path(bucket_root);
        std::fs::create_dir_all(&dir).map_err(|source| AuditWriterError::Io {
            op: "create_dir_all",
            path: dir.clone(),
            source_path: None,
            source,
        })?;
        let store = Store::local(bucket_root).map_err(|e| AuditWriterError::Io {
            op: "open store",
            path: bucket_root.to_path_buf(),
            source_path: None,
            source: io::Error::other(e),
        })?;
        let mut writer = Self::open_in(&store, partition)?;
        // Surface the absolute local landing path for the local backend
        // (readers/tests join the store root to find the file); the store
        // constructor leaves `final_path` as the object key rendered as a path.
        writer.final_path = dir.join(format!("{}.parquet", writer.flush_uuid));
        Ok(writer)
    }

    /// Open an audit writer for `partition` on an already-built [`Store`] (local
    /// or S3-compatible) — the S3-capable constructor (RFC 0019). Nothing is
    /// created up front (object stores have no directories, and the local
    /// backend's `put` creates parents); the file is `put` to its object key on
    /// [`AuditWriter::close`] (the buffer-and-put commit point).
    ///
    /// # Errors
    ///
    /// [`AuditWriterError::Parquet`] when the ZSTD level is rejected or
    /// `ArrowWriter` setup fails.
    pub fn open_in(store: &Store, partition: PartitionKey) -> Result<Self, AuditWriterError> {
        let flush_uuid = Uuid::now_v7();
        // The object key is the partition's audit Hive path (relative to the
        // store root) plus the file name, with `/` separators — object keys are
        // `/`-delimited regardless of the host OS.
        let key = format!(
            "{}/{}.parquet",
            partition
                .audit_path(Path::new(""))
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/"),
            flush_uuid
        );
        // No local root to join, so the object key rendered as a path is the
        // `final_path` here; the local constructor overrides it with the
        // absolute landing path.
        let final_path = PathBuf::from(&key);
        let props = audit_writer_properties()?;
        // Buffer-and-put: encode into memory; nothing hits the store until
        // `close`. A construction failure leaves no artifact.
        let inner = ArrowWriter::try_new(Vec::new(), audit_schema(), Some(props))
            .map_err(AuditWriterError::Parquet)?;

        Ok(Self {
            inner: Some(inner),
            partition,
            flush_uuid,
            store: store.clone(),
            key,
            final_path,
            num_rows: 0,
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
    /// [`AuditWriterError::Poisoned`] instead of publishing the
    /// buffer. The buffer is discarded (there is no on-disk
    /// artifact to inspect). `PartitionMismatch` and `Batch` errors
    /// do **not** poison: the writer remains usable for a follow-up
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
        // `&mut ArrowWriter<Vec<u8>>` so the outer `self.poisoned =
        // true` assignment can run after the borrow on `self.inner`
        // ends. `num_rows` is a disjoint field, so it can be borrowed
        // alongside `inner`; the helper bumps it per successfully
        // written sub-batch. Poison only on Parquet errors — `Batch`
        // errors come from `audit_events_to_batch`, which runs on a
        // single chunk and doesn't touch `inner` itself; the
        // buffer's state at the moment a `Batch` error fires is
        // whatever earlier chunks left it (clean, or holding
        // already-written events from this same call). Either
        // way a follow-up `append_events` is safe — the contract
        // is "writer remains usable", not "no events persisted".
        let result = append_chunks(inner, events, &mut self.num_rows);
        if matches!(result, Err(AuditWriterError::Parquet(_))) {
            self.poisoned = true;
        }
        result
    }

    /// Close the writer, finalising the Parquet footer in the
    /// in-memory buffer and publishing the bytes to the object store
    /// under the partition's key. Must be called for the file to be
    /// published; dropping without `close` discards the buffer and
    /// publishes nothing.
    ///
    /// **Poisoning check.** If a prior `append_events` returned a
    /// [`AuditWriterError::Parquet`] error, the writer is poisoned and
    /// this method refuses to publish — returns
    /// [`AuditWriterError::Poisoned`] and discards the buffer (there is
    /// no on-disk artifact to leave behind, unlike the former
    /// temp-file scheme).
    ///
    /// # Errors
    ///
    /// - [`AuditWriterError::Poisoned`] when a prior `append_events`
    ///   failed with a Parquet error.
    /// - [`AuditWriterError::Parquet`] when the footer write fails.
    /// - [`AuditWriterError::Io`] when the store `put` fails. Nothing
    ///   is published in that case (object-store puts are atomic).
    ///
    /// # Panics
    ///
    /// Structurally impossible. `inner` is populated by [`Self::open`]
    /// and only consumed here; `close` takes `self` by value so it
    /// can't run twice.
    pub fn close(mut self) -> Result<AuditWrittenFile, AuditWriterError> {
        if self.poisoned {
            // Refuse to publish a possibly-partial buffer.
            return Err(AuditWriterError::Poisoned);
        }
        let inner = self
            .inner
            .take()
            .expect("AuditWriter::close consumes self; inner is Some on entry");
        // `into_inner` writes the footer and returns the finished
        // bytes; the `put` is the atomic commit point.
        let bytes = inner.into_inner().map_err(AuditWriterError::Parquet)?;
        self.store
            .put_blocking(&self.key, bytes)
            .map_err(|e| AuditWriterError::Io {
                op: "put",
                path: self.final_path.clone(),
                source_path: None,
                source: io::Error::other(e),
            })?;
        Ok(AuditWrittenFile {
            path: self.final_path.clone(),
            partition: self.partition.clone(),
            flush_uuid: self.flush_uuid,
            num_rows: self.num_rows,
        })
    }

    /// Inspector for the path reported through [`AuditWrittenFile::path`]: the
    /// absolute landing path for [`Self::open`], or the object key rendered as a
    /// path for [`Self::open_in`] (no local root). The bytes only exist there
    /// after a successful `close` — while the writer is open they live in memory.
    #[must_use]
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

// No `Drop`: a writer abandoned without `close` just drops its
// in-memory buffer — nothing was ever written to the store, so there
// is no temp artifact to clean up (unlike the former temp-file scheme).

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
    /// audit file; the buffer is discarded.
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
                    "storage I/O on `{op}` from {} to {}: {source}",
                    src.display(),
                    path.display(),
                ),
                None => write!(f, "storage I/O on `{op}` at {}: {source}", path.display()),
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
                 publish to avoid landing a partial / corrupted file (the buffer is \
                 discarded)",
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
    inner: &mut ArrowWriter<Vec<u8>>,
    events: &[AuditEvent],
    num_rows: &mut i64,
) -> Result<(), AuditWriterError> {
    for chunk in events.chunks(SUB_BATCH_ROWS) {
        if inner.in_progress_size() >= ROW_GROUP_FLUSH_BYTES {
            inner.flush().map_err(AuditWriterError::Parquet)?;
        }
        let batch = audit_events_to_batch(chunk).map_err(AuditWriterError::Batch)?;
        inner.write(&batch).map_err(AuditWriterError::Parquet)?;
        // Count rows only once the sub-batch has been accepted, so a
        // mid-slice failure leaves `num_rows` reflecting exactly what
        // landed in the buffer. `chunk.len()` is bounded by
        // `SUB_BATCH_ROWS` (256), so the cast to `i64` is lossless.
        #[allow(clippy::cast_possible_wrap)]
        let written = chunk.len() as i64;
        *num_rows += written;
    }
    if inner.in_progress_size() >= ROW_GROUP_FLUSH_BYTES {
        inner.flush().map_err(AuditWriterError::Parquet)?;
    }
    Ok(())
}

/// Derive the [`PartitionKey`] for an audit event. Reuses the
/// data-side `PartitionKey` shape (which carries an `hour`
/// field); the audit path stops at `day`, so the `hour` field is
/// populated (always `0`) but ignored by `audit_partition_matches`.
///
/// Because `hour` is fixed at `0`, the returned key is canonical for the
/// audit partitioning axis (tenant + year/month/day): two events on the same
/// UTC day derive equal keys regardless of wall-clock hour. That makes it a
/// sound grouping key for a buffering sink batching one `AuditWriter` per
/// partition (issue #302).
///
/// # Errors
///
/// [`AuditWriterError::Batch`] when the event's timestamp is before the Unix
/// epoch or overflows `i64` nanoseconds.
pub fn derive_audit_partition(event: &AuditEvent) -> Result<PartitionKey, AuditWriterError> {
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
        audit_columns::ALIAS_ACTOR,
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
    // `alias_member_ids` "(list values)" gets `Page index = no` on
    // its list leaf per the §3.7 table (amendment 2026-06-12);
    // `alias_representative_id` keeps the page-index default
    // (`Page index = yes`, same shape as `template_id`).
    builder = builder.set_column_statistics_enabled(
        ColumnPath::new(vec![
            audit_columns::ALIAS_MEMBER_IDS.to_string(),
            "list".to_string(),
            "element".to_string(),
        ]),
        EnabledStatistics::Chunk,
    );

    Ok(builder.build())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use ourios_core::audit::{AuditEvent, AuditPayload};
    use ourios_core::tenant::TenantId;

    use super::{AuditWriter, derive_audit_partition};
    use crate::{AuditReader, Store};

    fn compaction_event(secs: u64) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new("acme"),
            timestamp: UNIX_EPOCH + Duration::from_secs(secs),
            payload: AuditPayload::Compaction {
                partition: "year=2026/month=04/day=02/hour=10".to_string(),
                input_files: vec!["a.parquet".to_string(), "b.parquet".to_string()],
                output_file: "c.parquet".to_string(),
                generation: 7,
                rows: 100,
            },
        }
    }

    /// `open_in` writes through an already-built `Store` and publishes on
    /// `close`: the events `put` to the partition's object key recover
    /// byte-for-byte, and `AuditWrittenFile` carries the row count plus the key
    /// (as `path`, rendered as a path for the store ctor).
    #[test]
    fn open_in_publishes_on_close_and_round_trips() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let events = vec![
            compaction_event(1_775_127_480),
            compaction_event(1_775_127_481),
        ];
        let partition = derive_audit_partition(&events[0]).expect("derive");

        let mut writer = AuditWriter::open_in(&store, partition).expect("open_in");
        writer.append_events(&events).expect("append");
        let written = writer.close().expect("close");

        // For `open_in`, `path` is the object key rendered as a path (no local
        // root); on this platform it round-trips to the store key.
        let key = written.path.to_string_lossy().into_owned();
        assert!(key.ends_with(".parquet"), "key: {key}");
        assert_eq!(written.num_rows, 2);
        let bytes = store.get_blocking(&key).expect("get");
        let read = AuditReader::open_bytes(bytes::Bytes::from(bytes))
            .expect("open_bytes")
            .read_all()
            .expect("read_all");
        assert_eq!(
            read, events,
            "events recover byte-for-byte through the store"
        );
    }

    /// A writer dropped without `close` publishes nothing — buffer-and-put means
    /// the object only exists after the `close` `put`, so an abandoned writer
    /// leaves no object behind (no temp file, unlike the old fs scheme).
    #[test]
    fn drop_without_close_publishes_nothing() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("store");
        let event = compaction_event(1_775_127_480);
        let partition = derive_audit_partition(&event).expect("derive");

        {
            let mut writer = AuditWriter::open_in(&store, partition).expect("open_in");
            writer
                .append_events(std::slice::from_ref(&event))
                .expect("append");
            // drop without close
        }
        assert!(
            store.list_blocking(None).expect("list").is_empty(),
            "no object is published until close",
        );
    }
}
