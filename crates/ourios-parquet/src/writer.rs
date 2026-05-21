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
//! - Dictionary encoding on every column **except** `body`
//!   (the [`CLAUDE.md`] §3.2 cardinality invariant — bodies are
//!   unbounded by design; dict on `body` is the failure mode).
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
    /// Flushes a row group when the in-progress buffer crosses
    /// [`ROW_GROUP_FLUSH_BYTES`].
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
        let batch = mined_records_to_batch(records).map_err(WriterError::Batch)?;
        self.inner.write(&batch).map_err(WriterError::Parquet)?;
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
        // Per-page statistics enabled. Distinct from the Parquet
        // "page index" feature (OffsetIndex / ColumnIndex)
        // controlled by `set_writer_version` + writer behaviour;
        // `EnabledStatistics::Page` only controls the granularity
        // of min/max stats inside each `DataPage` header. The
        // §3.6 "page index `yes`" column will be addressed
        // properly in the reader PR (PR-F) when the
        // page-index-read path lands and the writer-side toggle
        // is locked against a measurable read benefit; pinning a
        // shape here without that feedback loop is premature.
        .set_statistics_enabled(EnabledStatistics::Page);

    // §3.6: NO dictionary on `body`. CLAUDE.md §3.2's cardinality
    // invariant — bodies are unbounded by design; dict would
    // balloon. This is the load-bearing override.
    let body = ColumnPath::new(vec![crate::columns::BODY.to_string()]);
    builder = builder.set_column_dictionary_enabled(body, false);

    // §3.6: NO dictionary on the high-entropy attribute / id
    // columns (`attributes`, `trace_id`, `span_id`, `params`
    // values). Page index stays on for `trace_id` / `span_id`
    // per the §3.6 table; on `attributes` it's `no`/`no`/`no`.
    for high_entropy in [
        crate::columns::ATTRIBUTES,
        crate::columns::TRACE_ID,
        crate::columns::SPAN_ID,
    ] {
        builder = builder
            .set_column_dictionary_enabled(ColumnPath::new(vec![high_entropy.to_string()]), false);
    }

    // §3.6: bloom filter on `template_id` (B2 predicate-pushdown).
    let template_id = ColumnPath::new(vec![crate::columns::TEMPLATE_ID.to_string()]);
    builder = builder.set_column_bloom_filter_enabled(template_id, true);

    Ok(builder.build())
}
