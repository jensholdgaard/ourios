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
//! - Dictionary encoding **on** globally, **off** for the
//!   high-entropy columns the §3.6 table marks `no`: `body`
//!   (the [`CLAUDE.md`] §3.2 cardinality invariant — bodies are
//!   unbounded by design; dict on `body` is the failure mode),
//!   `attributes` (JSON `BYTE_ARRAY`, high entropy), `trace_id`
//!   and `span_id` (16- and 8-byte near-random opaque ids — dict
//!   and bloom both lose). The other columns in the §3.6 table
//!   that the table also says `no` for (`params` list values,
//!   `confidence`, `time_unix_nano` / `observed_time_unix_nano`,
//!   `lossy_flag`) get sensible defaults from arrow-rs without
//!   needing per-column overrides.
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

/// Streaming Parquet writer for one partition's data file.
///
/// One [`Writer`] writes one Parquet file. After [`Writer::close`]
/// returns, the file is complete on disk and ready for reader
/// access; before then the file's footer is not yet present and
/// readers MUST treat it as in-progress (§3.4's atomic-publish
/// convention — pinned in RFC 0005 §7 as an open question on the
/// concurrent-writer side, but the writer-side rule is "close
/// before announcing").
pub struct Writer {
    inner: ArrowWriter<File>,
    partition: PartitionKey,
    flush_uuid: Uuid,
    file_path: PathBuf,
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
        std::fs::create_dir_all(&dir).map_err(WriterError::Io)?;
        let flush_uuid = Uuid::now_v7();
        let file_path = dir.join(format!("{flush_uuid}.parquet"));
        let file = File::create(&file_path).map_err(WriterError::Io)?;

        let props = writer_properties()?;
        let inner =
            ArrowWriter::try_new(file, data_schema(), Some(props)).map_err(WriterError::Parquet)?;

        Ok(Self {
            inner,
            partition,
            flush_uuid,
            file_path,
        })
    }

    /// Append `records` to the writer. Validates each record's
    /// partition matches the writer's open partition (§3.4 / §3.9
    /// row-vs-path agreement: a writer scoped to one partition
    /// MUST NOT write rows from another), converts the slice to
    /// a `RecordBatch`, and forwards to `ArrowWriter::write`.
    ///
    /// **Row-group sizing.** Flushes a row group before *and*
    /// after each write when the in-progress buffer crosses
    /// [`ROW_GROUP_FLUSH_BYTES`] (128 MiB, RFC 0005 §3.5's lower
    /// bound). This bounds row-group overshoot to *one batch's
    /// worth of bytes*. The §3.5 upper bound ("never exceed
    /// 1 GiB") is enforced by caller discipline: callers handing
    /// in batches large enough to push a single write past 1 GiB
    /// of uncompressed Arrow bytes will produce a single
    /// oversized row group. Production callers should chunk
    /// `records` into batches well below that ceiling; corpus /
    /// bench inputs in PR-E2 are several orders of magnitude
    /// below it. A future PR may add internal chunking based on
    /// `ArrowWriter`'s row-count target if the caller-discipline
    /// rule proves fragile.
    ///
    /// # Errors
    ///
    /// - [`WriterError::PartitionMismatch`] when a record's derived
    ///   partition (per §3.4's time-fallback algorithm) disagrees
    ///   with the writer's `partition`. Fail-fast at write time
    ///   keeps the §3.9 reader contract enforceable — a file
    ///   written here will never produce a row-vs-path mismatch
    ///   on read.
    /// - [`WriterError::Batch`] when `RecordBatch` construction
    ///   fails (timestamp overflow per RFC 0005 §3.2, or Arrow
    ///   internal error).
    /// - [`WriterError::Parquet`] when the underlying Parquet
    ///   writer rejects the batch (codec or footer error).
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
        // Pre-write flush: if the in-progress row group already
        // crosses the §3.5 lower threshold from the *previous*
        // append, seal it now so this new batch starts a fresh
        // row group rather than piling on top. Bounds row-group
        // overshoot to a single batch's worth of bytes; callers
        // that hand in batches larger than (1 GiB − 128 MiB) can
        // still individually exceed the §3.5 upper bound, which
        // is a caller-side discipline the rustdoc documents.
        if self.inner.in_progress_size() >= ROW_GROUP_FLUSH_BYTES {
            self.inner.flush().map_err(WriterError::Parquet)?;
        }
        let batch = mined_records_to_batch(records).map_err(WriterError::Batch)?;
        self.inner.write(&batch).map_err(WriterError::Parquet)?;
        // Post-write flush: if this batch just tipped the buffer
        // past the threshold, seal it before returning.
        if self.inner.in_progress_size() >= ROW_GROUP_FLUSH_BYTES {
            self.inner.flush().map_err(WriterError::Parquet)?;
        }
        Ok(())
    }

    /// Close the writer, finalising the Parquet footer on disk.
    /// Must be called for the file to be readable; dropping
    /// without `close` leaves the file truncated/header-only.
    ///
    /// # Errors
    ///
    /// [`WriterError::Parquet`] when the footer write fails.
    pub fn close(self) -> Result<WrittenFile, WriterError> {
        let metadata = self.inner.close().map_err(WriterError::Parquet)?;
        Ok(WrittenFile {
            path: self.file_path,
            partition: self.partition,
            flush_uuid: self.flush_uuid,
            num_rows: metadata.num_rows,
        })
    }

    /// Inspector for the absolute path the writer is writing
    /// into. Useful for tests; production callers usually take
    /// [`WrittenFile::path`] from [`Writer::close`]'s return value.
    #[must_use]
    pub fn file_path(&self) -> &Path {
        &self.file_path
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
    /// Filesystem I/O failure (directory create, file open).
    Io(io::Error),
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
}

impl fmt::Display for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "filesystem I/O: {e}"),
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
        }
    }
}

impl std::error::Error for WriterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Parquet(e) => Some(e),
            Self::Batch(e) => Some(e),
            Self::PartitionMismatch { .. } => None,
        }
    }
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

    // §3.6 also marks the `params` list-value leaf as `Dictionary
    // = no` ("Per-row entropy too high"). Parquet's 3-level LIST
    // encoding for `LIST<STRUCT<type_tag: INT32, value:
    // BINARY>>` exposes the value leaf at the dotted path
    // `params.list.element.value`; this override disables dict
    // on that exact leaf, leaving the small-cardinality
    // `params.list.element.type_tag` leaf to inherit the global
    // dict-on default (the §3.6 entry's "list values"
    // parenthetical scopes the no-dict rule to the byte payload
    // only). The integration test `tests/no_body_dict.rs` /
    // related metadata walks pin both expectations.
    let params_value_leaf = ColumnPath::new(vec![
        crate::columns::PARAMS.to_string(),
        "list".to_string(),
        "element".to_string(),
        "value".to_string(),
    ]);
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
    // 3-level LIST encoding path.
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
