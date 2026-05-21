//! Parquet data-file writer per RFC 0005 §3.4 / §3.5 / §3.6.
//!
//! Opens a file at the Hive-style partition path computed by
//! [`PartitionKey::data_path`], names it `<UUIDv7>.parquet` per
//! §3.4, writes batches via [`mined_records_to_batch`], and
//! rotates row groups when the in-progress buffer crosses the
//! §3.5 threshold (128 MiB uncompressed).
//!
//! Encoding policy per §3.6:
//! - ZSTD level 3 compression on every column.
//! - Dictionary encoding **on** globally, **off** explicitly
//!   per-column for every §3.6 row marked `Dictionary = no`:
//!   `body` (the [`CLAUDE.md`] §3.2 cardinality invariant —
//!   bodies are unbounded by design; dict on `body` is the
//!   failure mode), `attributes` (JSON `BYTE_ARRAY`, high
//!   entropy), `trace_id` / `span_id` (16- and 8-byte
//!   near-random opaque ids), `time_unix_nano` /
//!   `observed_time_unix_nano` (delta-encoded inside ZSTD;
//!   dict would interfere), `confidence` (float, narrow range),
//!   and both leaves of the `params` list element
//!   (`params.list.element.type_tag` and
//!   `params.list.element.value` — §3.6 "(list values)" covers
//!   the entire `LIST<STRUCT<...>>` element). The §3.6
//!   `lossy_flag` row says `Dictionary = n/a` (boolean RLE
//!   handles it natively), so no override is needed for that one.
//! - Per-page statistics **on** globally so the Parquet page
//!   index (`ColumnIndex` + `OffsetIndex`) is emitted for the
//!   `Page index = yes` columns; downgraded to
//!   `EnabledStatistics::Chunk` for the `Page index = no`
//!   columns (`tenant_id`, `attributes`, `resource_attributes`,
//!   `body`, both `params` list-element leaves,
//!   `separators.list.element`).
//! - Bloom filter on `template_id` (B2 predicate-pushdown).
//!
//! [`CLAUDE.md`]: ../../../../CLAUDE.md

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use ourios_core::record::MinedRecord;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::errors::ParquetError;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::schema::types::ColumnPath;
use uuid::Uuid;

use crate::data_schema;
use crate::partition::PartitionKey;
use crate::record_batch::{BatchError, mined_records_to_batch};

/// RFC 0005 §3.5 — uncompressed bytes per row group, lower
/// threshold. The writer flushes a row group when
/// `ArrowWriter::in_progress_size` crosses this.
pub const ROW_GROUP_FLUSH_BYTES: usize = 128 * 1024 * 1024; // 128 MiB

/// Rows per internal sub-batch passed to `ArrowWriter::write`.
/// Chosen so that even with multi-KiB per-record payloads, a
/// single sub-batch's contribution after the [`ROW_GROUP_FLUSH_BYTES`]
/// threshold check stays well under RFC 0005 §3.5's hard 1 GiB
/// upper bound: 1024 rows × ≤ 768 KiB per record ≈ 768 MiB,
/// plus a 128 MiB pre-flushed buffer ≈ 896 MiB worst case, with
/// the 1 GiB ceiling still uncrossed.
const SUB_BATCH_ROWS: usize = 1024;

/// Streaming Parquet writer for one partition's data file.
///
/// One [`Writer`] writes one Parquet file. The writer publishes
/// **atomically**: bytes are written to a `<uuid>.parquet.tmp`
/// path while the writer is open; [`Writer::close`] renames the
/// temp file to the final `<uuid>.parquet` only after the footer
/// is written and the file is closed. Readers that enumerate the
/// partition can rely on "every `*.parquet` file has a logically
/// complete footer" — they filter the `.parquet.tmp` suffix out.
/// If the writer is dropped without [`Writer::close`] (panic,
/// early-return), [`Drop`] removes the `.parquet.tmp` file so an
/// aborted write doesn't pollute the partition directory with an
/// unreadable file. This satisfies RFC 0005 §7's "atomic-publish
/// convention (write to a temp path, rename on close)"
/// open-question item.
///
/// **The atomic publish is logical, not crash-durable.** Neither
/// the data pages nor the rename metadata are
/// [`File::sync_all`]-ed; a host crash or power loss between the
/// rename and the OS's next page-cache flush could leave the
/// renamed file with truncated or zero-padded contents on disk.
/// Crash-survival durability is the WAL's domain (`CLAUDE.md`
/// §3.4 "WAL-before-ack"); see [`Writer::close`]'s rustdoc for
/// the full reasoning.
pub struct Writer {
    inner: Option<ArrowWriter<File>>,
    partition: PartitionKey,
    flush_uuid: Uuid,
    /// Final `<uuid>.parquet` path the file moves to on close.
    final_path: PathBuf,
    /// `<uuid>.parquet.tmp` path the writer actually writes to.
    /// `None` once [`Self::close`] renames it away; the [`Drop`]
    /// impl uses `None` as the "already published; nothing to
    /// clean up" signal.
    temp_path: Option<PathBuf>,
    /// Set to `true` once any `ArrowWriter::write` /
    /// `ArrowWriter::flush` call returns `Err`. The underlying
    /// `ArrowWriter`'s buffer state is undefined after such a
    /// failure (the row group may be partially written), so
    /// [`Self::close`] refuses to publish — renaming the temp file
    /// into place would silently land a potentially-corrupted
    /// data file. The temp file is left on disk for diagnosis
    /// (the [`Drop`] impl skips its usual cleanup when poisoned).
    /// Mirrors [`crate::audit_writer::AuditWriter`]'s contract.
    poisoned: bool,
}

impl Writer {
    /// Open a writer for `partition` under `bucket_root`. Creates
    /// the partition directory and the `UUIDv7`-named Parquet file;
    /// the file is empty until [`Writer::append_records`] starts
    /// adding rows.
    ///
    /// # Errors
    ///
    /// - [`WriterError::Io`] when the partition directory or
    ///   target file cannot be created.
    /// - [`WriterError::Parquet`] when the ZSTD level is rejected
    ///   or `ArrowWriter` setup fails.
    pub fn open(bucket_root: &Path, partition: PartitionKey) -> Result<Self, WriterError> {
        let dir = partition.data_path(bucket_root);
        std::fs::create_dir_all(&dir).map_err(|source| WriterError::Io {
            op: "create_dir_all",
            path: dir.clone(),
            source_path: None,
            source,
        })?;
        let flush_uuid = Uuid::now_v7();
        let final_path = dir.join(format!("{flush_uuid}.parquet"));
        let temp_path = dir.join(format!("{flush_uuid}.parquet.tmp"));
        let file = File::create(&temp_path).map_err(|source| WriterError::Io {
            op: "create",
            path: temp_path.clone(),
            source_path: None,
            source,
        })?;

        // From this point on, the `.parquet.tmp` file exists on
        // disk. If anything below errors, no `Writer` is
        // constructed and `Drop` therefore never runs — we'd
        // leak the temp file unless we clean it up explicitly.
        let props = match writer_properties() {
            Ok(p) => p,
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&temp_path);
                return Err(e);
            }
        };
        let inner = match ArrowWriter::try_new(file, data_schema(), Some(props)) {
            Ok(w) => w,
            Err(e) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(WriterError::Parquet(e));
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

    /// Append `records` to the writer. Validates each record's
    /// partition matches the writer's open partition (§3.4 / §3.9
    /// row-vs-path agreement: a writer scoped to one partition
    /// MUST NOT write rows from another), converts the slice to
    /// a `RecordBatch`, and forwards to `ArrowWriter::write`.
    ///
    /// **Row-group sizing.** Internally chunks `records` into
    /// sub-batches of [`SUB_BATCH_ROWS`] (1024) rows and runs a
    /// flush-when-over-threshold check before each sub-batch
    /// write. RFC 0005 §3.5 pins the row-group target at 128 MiB
    /// – 1 GiB uncompressed; chunking + per-sub-batch flush
    /// keeps the maximum row-group size bounded to roughly
    /// `ROW_GROUP_FLUSH_BYTES + (per-record bytes × SUB_BATCH_ROWS)`,
    /// which stays comfortably under 1 GiB for the per-record
    /// sizes log ingest produces in practice.
    ///
    /// **Poisoning.** A failed `inner.write()` or `inner.flush()`
    /// leaves the underlying `ArrowWriter`'s buffer in an
    /// undefined state — the partial row group can't be safely
    /// recovered. When that happens, the writer marks itself
    /// poisoned and [`Self::close`] subsequently returns
    /// [`WriterError::Poisoned`] instead of renaming the temp
    /// file into place. The `.parquet.tmp` stays on disk for
    /// diagnosis. Per-record validation errors
    /// (`PartitionMismatch`, `Batch`) do **not** poison — the
    /// inner writer hasn't been touched yet.
    ///
    /// # Errors
    ///
    /// - [`WriterError::PartitionMismatch`] when a record's derived
    ///   partition (per §3.4's time-fallback algorithm) disagrees
    ///   with the writer's `partition`. Fail-fast at write time
    ///   keeps the §3.9 reader contract enforceable — a file
    ///   written here will never produce a row-vs-path mismatch
    ///   on read. **Non-poisoning**.
    /// - [`WriterError::Batch`] when `RecordBatch` construction
    ///   fails (timestamp overflow per RFC 0005 §3.2, or Arrow
    ///   internal error). **Non-poisoning**.
    /// - [`WriterError::Parquet`] when the underlying Parquet
    ///   writer rejects the batch (codec or footer error).
    ///   **Poisons the writer**.
    ///
    /// # Panics
    ///
    /// Structurally impossible. The inner `ArrowWriter` is
    /// `Some` from [`Writer::open`] until [`Writer::close`]
    /// takes ownership of `self`; `append_records` borrows
    /// `&mut self` and therefore cannot run after `close`.
    pub fn append_records(&mut self, records: &[MinedRecord]) -> Result<(), WriterError> {
        if records.is_empty() {
            return Ok(());
        }
        for (idx, r) in records.iter().enumerate() {
            let derived = PartitionKey::derive(r).map_err(|e| WriterError::Batch(e.into()))?;
            if derived != self.partition {
                return Err(WriterError::PartitionMismatch {
                    row_index: idx,
                    expected: self.partition.clone(),
                    actual: derived,
                });
            }
        }
        let inner = self
            .inner
            .as_mut()
            .expect("inner ArrowWriter is Some until Writer::close is called");
        // Run the Parquet-touching loop in a helper that takes a
        // `&mut ArrowWriter<File>` so the outer `self.poisoned =
        // true` assignment can run after the borrow on `self.inner`
        // ends. Poison only on Parquet errors — `Batch` errors
        // come from `mined_records_to_batch` BEFORE any
        // `inner.write` touches the buffer, so the inner writer
        // is still in a clean state and a follow-up
        // `append_records` is safe.
        let result = append_chunks(inner, records);
        if matches!(result, Err(WriterError::Parquet(_))) {
            self.poisoned = true;
        }
        result
    }

    /// Close the writer, finalising the Parquet footer on the
    /// temp file and atomically renaming it to the final path.
    /// Must be called for the file to land at its final name;
    /// dropping without `close` leaves only a `.parquet.tmp`
    /// that the [`Drop`] impl deletes.
    ///
    /// **Atomic publish is logical, not crash-durable.** Once
    /// this method returns, the final-path file has a complete
    /// Parquet footer and any subsequent reader can open it.
    /// However, neither the data pages nor the rename metadata
    /// are [`File::sync_all`]-ed before this call returns — a
    /// host crash or power loss between rename and the OS's
    /// next page-cache flush could leave the renamed file with
    /// truncated or zero-padded contents on disk. Crash-survival
    /// durability is the WAL's domain (`CLAUDE.md` §3.4
    /// "WAL-before-ack"); the Parquet writer is the storage tier
    /// and assumes its records are recoverable via WAL replay
    /// after a crash.
    ///
    /// **Poisoning check.** If a prior `append_records` returned
    /// a [`WriterError::Parquet`] error, the writer is poisoned
    /// and this method refuses to publish — returns
    /// [`WriterError::Poisoned`] without touching `inner` /
    /// `temp_path`, so the [`Drop`] impl's poisoned branch leaves
    /// the temp file on disk for diagnosis.
    ///
    /// # Errors
    ///
    /// - [`WriterError::Poisoned`] when a prior `append_records`
    ///   failed with a Parquet error (temp file left on disk).
    /// - [`WriterError::Parquet`] when the footer write fails.
    /// - [`WriterError::Io`] when the atomic rename from the
    ///   temp filename to the final path fails (the temp file
    ///   is left in place for diagnosis in that case).
    ///
    /// # Panics
    ///
    /// Structurally impossible. `inner` / `temp_path` are
    /// populated by [`Writer::open`] and only consumed here;
    /// `close` takes `self` by value so it can't run twice.
    pub fn close(mut self) -> Result<WrittenFile, WriterError> {
        if self.poisoned {
            // Refuse to publish a possibly-partial file. Don't
            // take `inner` / `temp_path` — `Drop` sees
            // `poisoned = true` and leaves the .parquet.tmp on
            // disk for diagnosis.
            return Err(WriterError::Poisoned);
        }
        // Take both `inner` and `temp_path` BEFORE attempting
        // any fallible work. If `inner.close()` or
        // `fs::rename` then errors, `self.temp_path` is already
        // `None` so the [`Drop`] impl won't delete the
        // partially-written `.parquet.tmp` file on the way out
        // — the file stays on disk for diagnosis / recovery,
        // matching the # Errors clause above. This ordering is
        // load-bearing: a failed `close` that destroyed its own
        // artifact would be the worst-case failure mode.
        let inner = self
            .inner
            .take()
            .expect("Writer::close consumes self; inner is Some on entry");
        let temp_path = self
            .temp_path
            .take()
            .expect("temp_path is Some until close consumes it");
        let metadata = inner.close().map_err(WriterError::Parquet)?;
        std::fs::rename(&temp_path, &self.final_path).map_err(|source| WriterError::Io {
            op: "rename",
            path: self.final_path.clone(),
            source_path: Some(temp_path.clone()),
            source,
        })?;
        Ok(WrittenFile {
            path: self.final_path.clone(),
            partition: self.partition.clone(),
            flush_uuid: self.flush_uuid,
            num_rows: metadata.num_rows,
        })
    }

    /// Inspector for the absolute path the writer will publish
    /// to after [`Self::close`]. While the writer is open, the
    /// actual bytes live at `<this path>.tmp`; useful for tests
    /// that want to assert the final landing site without
    /// reading the file.
    #[must_use]
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }
}

impl Drop for Writer {
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
        // If `close` consumed the writer cleanly, `temp_path`
        // is `None` and the rename has already happened.
        // Otherwise the writer was dropped mid-stream (panic,
        // `?` early-return, etc.); remove the `.parquet.tmp`
        // file so the partition directory doesn't accrue
        // unreadable Parquet files an enumeration reader would
        // trip over.
        if let Some(temp) = self.temp_path.take() {
            // Drop the `ArrowWriter` *before* `remove_file`:
            // on Windows the underlying `File` holds an
            // exclusive handle and `remove_file` on the path
            // would fail while the writer is still alive.
            // (Custom `Drop::drop` runs first; struct fields
            // are only dropped after this function returns —
            // we have to release the file handle explicitly.)
            drop(self.inner.take());
            // Best-effort: ignore I/O errors here — there's no
            // recovery path from a destructor, and the worst-
            // case outcome is a stray `.parquet.tmp` that
            // operators clean up by hand. We deliberately do
            // not log; the WAL / ingest layer above is the
            // place for that.
            let _ = std::fs::remove_file(&temp);
        }
    }
}

/// Result of a successful [`Writer::close`].
#[derive(Debug)]
pub struct WrittenFile {
    /// Absolute path the file was written to.
    pub path: PathBuf,
    /// Partition key the file was opened under.
    pub partition: PartitionKey,
    /// `UUIDv7` flush identifier embedded in the filename.
    pub flush_uuid: Uuid,
    /// Total number of rows in the file (sum across row groups).
    pub num_rows: i64,
}

/// Errors produced by [`Writer`].
#[derive(Debug)]
pub enum WriterError {
    /// Filesystem I/O failure. Carries the operation name and
    /// the path(s) involved so logs and recovery scripts can
    /// pinpoint which step failed and which `.parquet.tmp` (if
    /// any) is left on disk for diagnosis.
    Io {
        /// Short operation name (e.g. `"create_dir_all"`,
        /// `"create"`, `"rename"`).
        op: &'static str,
        /// The primary path the operation was acting on. For
        /// `rename`, this is the *destination*; the source
        /// path lives in [`Self::source_path`] (when set).
        path: PathBuf,
        /// Secondary path for two-path operations (only
        /// populated for `rename`, where it carries the source
        /// `.parquet.tmp` path that's left on disk).
        source_path: Option<PathBuf>,
        /// Underlying `io::Error`.
        source: io::Error,
    },
    /// Parquet writer failure (footer write, codec failure).
    Parquet(ParquetError),
    /// `RecordBatch` construction failed — see [`BatchError`].
    Batch(BatchError),
    /// A record in the batch belongs to a different partition than
    /// the one the writer was opened against. Surfaces the
    /// row-vs-path contract from RFC 0005 §3.9 at write time
    /// rather than letting the reader catch the mismatch later.
    PartitionMismatch {
        /// Zero-based index into the batch slice.
        row_index: usize,
        /// The partition the writer was opened against.
        expected: PartitionKey,
        /// The partition derived from the offending record.
        actual: PartitionKey,
    },
    /// A prior [`Writer::append_records`] returned a `Parquet`
    /// error, leaving the underlying writer's buffer in an
    /// undefined state. [`Writer::close`] refuses to publish to
    /// protect against landing a partial / corrupted data file;
    /// the `.parquet.tmp` is preserved on disk for diagnosis.
    /// Mirrors [`crate::audit_writer::AuditWriterError::Poisoned`].
    Poisoned,
}

impl fmt::Display for WriterError {
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
            Self::Batch(e) => write!(f, "record batch: {e}"),
            Self::PartitionMismatch {
                row_index,
                expected,
                actual,
            } => write!(
                f,
                "record at index {row_index} derives partition (tenant_id={}, \
                 year={:04}, month={:02}, day={:02}, hour={:02}) which does not match the \
                 writer's open partition (tenant_id={}, year={:04}, month={:02}, day={:02}, \
                 hour={:02}) — RFC 0005 §3.9 row-vs-path contract",
                actual.tenant_id,
                actual.year,
                actual.month,
                actual.day,
                actual.hour,
                expected.tenant_id,
                expected.year,
                expected.month,
                expected.day,
                expected.hour,
            ),
            Self::Poisoned => write!(
                f,
                "Writer is poisoned — a prior append_records failed with a Parquet error, \
                 leaving the buffer in an undefined state; close() refuses to publish to \
                 avoid landing a partial / corrupted file (the .parquet.tmp is preserved \
                 on disk for diagnosis)",
            ),
        }
    }
}

impl std::error::Error for WriterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parquet(e) => Some(e),
            Self::Batch(e) => Some(e),
            Self::PartitionMismatch { .. } | Self::Poisoned => None,
        }
    }
}

/// Inner Parquet-touching loop of [`Writer::append_records`].
/// Borrows the `ArrowWriter` directly so the caller can set
/// `self.poisoned = true` after the borrow ends if this returns
/// an `Err(WriterError::Parquet(_))`. Per the §3.5 row-group
/// sizing rule, runs a `flush()` when the in-progress buffer
/// crosses [`ROW_GROUP_FLUSH_BYTES`] (128 MiB). Symmetric helper
/// to the audit writer's `append_chunks`.
fn append_chunks(
    inner: &mut ArrowWriter<File>,
    records: &[MinedRecord],
) -> Result<(), WriterError> {
    // Chunk into SUB_BATCH_ROWS-sized sub-batches and run a
    // flush-if-over-threshold check before every sub-batch.
    // The bound on row-group size is therefore:
    //   (§3.5 lower threshold) + (one sub-batch's worth) ≈
    //   well under §3.5's 1 GiB upper bound for any reasonable
    //   per-record size. The size check happens *before* every
    //   sub-batch (not after), so a sub-batch that pushes the
    //   buffer past the threshold seals the next time around —
    //   bounded overshoot is intentional; unbounded overshoot
    //   is what the RFC prohibits.
    for chunk in records.chunks(SUB_BATCH_ROWS) {
        if inner.in_progress_size() >= ROW_GROUP_FLUSH_BYTES {
            inner.flush().map_err(WriterError::Parquet)?;
        }
        let batch = mined_records_to_batch(chunk).map_err(WriterError::Batch)?;
        inner.write(&batch).map_err(WriterError::Parquet)?;
    }
    // Final post-write check so the next `append_records` call
    // doesn't inherit an over-threshold buffer.
    if inner.in_progress_size() >= ROW_GROUP_FLUSH_BYTES {
        inner.flush().map_err(WriterError::Parquet)?;
    }
    Ok(())
}

/// Build the [`WriterProperties`] that encode RFC 0005 §3.5
/// (compression codec) and §3.6 (per-column encoding policy).
fn writer_properties() -> Result<WriterProperties, WriterError> {
    let mut builder = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(3).map_err(WriterError::Parquet)?,
        ))
        // Dictionary on globally by default (most columns benefit
        // per §3.6); we opt out per-column below for the high-
        // entropy ones.
        .set_dictionary_enabled(true)
        // Per-page statistics enabled. In parquet-rs `≥ 53`,
        // setting `EnabledStatistics::Page` causes the writer to
        // emit the Parquet "page index" (`ColumnIndex` +
        // `OffsetIndex`) alongside the per-page min/max stats in
        // each `DataPage` header. This satisfies §3.6's "page
        // index = yes" entries (for `template_id`, `time_unix_nano`,
        // `severity_*`, `scope_*`, `trace_id`, `span_id`, etc.) —
        // a writer-side metadata check (`column_index_offset` is
        // `Some(_)`) pins the contract.
        .set_statistics_enabled(EnabledStatistics::Page);

    // §3.6 `Dictionary = no` overrides. The RFC's §3.6 table
    // names every column that opts out; this loop is the
    // exhaustive set. `body` carries `CLAUDE.md` §3.2's
    // cardinality invariant load-bearing rationale (bodies are
    // unbounded by design); the others are either high-entropy
    // (`attributes`, `trace_id`, `span_id`) or non-text numeric
    // columns where dict-encoding adds overhead without payoff
    // (`time_unix_nano`, `observed_time_unix_nano`,
    // `confidence`).
    for no_dict_col in [
        crate::columns::TIME_UNIX_NANO,
        crate::columns::OBSERVED_TIME_UNIX_NANO,
        crate::columns::ATTRIBUTES,
        crate::columns::TRACE_ID,
        crate::columns::SPAN_ID,
        crate::columns::BODY,
        crate::columns::CONFIDENCE,
    ] {
        builder = builder
            .set_column_dictionary_enabled(ColumnPath::new(vec![no_dict_col.to_string()]), false);
    }

    // §3.6 also marks the `params` "(list values)" row as
    // `Dictionary = no` / `Page index = no`. "List values" here
    // covers every leaf of the LIST<STRUCT<...>> element — both
    // the `type_tag` and `value` leaves — per a literal reading
    // of the RFC table. Parquet's 3-level LIST encoding exposes
    // the leaves at the dotted paths
    // `params.list.element.type_tag` (INT32) and
    // `params.list.element.value` (BINARY). These overrides
    // disable dict + page index on both leaves; the
    // `tests/no_body_dict.rs` metadata walks pin both.
    let params_type_tag_leaf = ColumnPath::new(vec![
        crate::columns::PARAMS.to_string(),
        "list".to_string(),
        "element".to_string(),
        "type_tag".to_string(),
    ]);
    let params_value_leaf = ColumnPath::new(vec![
        crate::columns::PARAMS.to_string(),
        "list".to_string(),
        "element".to_string(),
        "value".to_string(),
    ]);
    builder = builder.set_column_dictionary_enabled(params_type_tag_leaf.clone(), false);
    builder = builder.set_column_dictionary_enabled(params_value_leaf.clone(), false);

    // §3.6 `Page index = no` overrides. The global
    // `EnabledStatistics::Page` writes per-page stats AND the
    // Parquet `ColumnIndex` / `OffsetIndex`; downgrading these
    // columns to `EnabledStatistics::Chunk` keeps the chunk-
    // level min/max (still useful for row-group pruning) but
    // suppresses the per-page surface. The columns named here
    // are the §3.6 table's `Page index = no` rows: `tenant_id`
    // (one value per file — page index is moot), `attributes` /
    // `resource_attributes` / `body` (high-entropy JSON / opaque
    // bytes), and the `params` / `separators` list-value leaves
    // ("Per-row entropy too high" / "Almost always a single
    // space").
    for no_page_idx_col in [
        crate::columns::TENANT_ID,
        crate::columns::ATTRIBUTES,
        crate::columns::RESOURCE_ATTRIBUTES,
        crate::columns::BODY,
    ] {
        builder = builder.set_column_statistics_enabled(
            ColumnPath::new(vec![no_page_idx_col.to_string()]),
            EnabledStatistics::Chunk,
        );
    }
    // The `params` and `separators` list-value leaves at the
    // 3-level LIST encoding path. Both `params.list.element.type_tag`
    // and `params.list.element.value` are covered per the §3.6
    // "(list values)" literal reading.
    builder = builder.set_column_statistics_enabled(params_type_tag_leaf, EnabledStatistics::Chunk);
    builder = builder.set_column_statistics_enabled(params_value_leaf, EnabledStatistics::Chunk);
    builder = builder.set_column_statistics_enabled(
        ColumnPath::new(vec![
            crate::columns::SEPARATORS.to_string(),
            "list".to_string(),
            "element".to_string(),
        ]),
        EnabledStatistics::Chunk,
    );

    // §3.6: bloom filter on `template_id` (B2 predicate-pushdown).
    let template_id = ColumnPath::new(vec![crate::columns::TEMPLATE_ID.to_string()]);
    builder = builder.set_column_bloom_filter_enabled(template_id, true);

    Ok(builder.build())
}
