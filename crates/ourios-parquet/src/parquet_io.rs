//! Shared buffer-and-put core for the data ([`crate::Writer`]) and audit
//! ([`crate::AuditWriter`]) Parquet writers — the write-side counterpart
//! to [`crate::decode`]'s shared column accessors.
//!
//! Both writers follow the same lifecycle: encode rows into an in-memory
//! `ArrowWriter<Vec<u8>>` in sub-batches with a §3.5 row-group flush
//! threshold, then `put` the finished bytes to the object store on close
//! (the atomic commit point). [`BufferedParquetWriter`] owns that
//! lifecycle — buffer, key, poisoning, publish — while each outer writer
//! keeps what actually differs: its schema and writer properties, its
//! row-to-batch encoder, its partition pre-check, and its result type.
//!
//! The generic functions take any error implementing [`WriteError`], the
//! two-sided seam that lets one core serve both writers without unifying
//! their public error enums.

use std::io;
use std::path::{Path, PathBuf};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use parquet::arrow::ArrowWriter;
use parquet::errors::ParquetError;
use parquet::file::properties::WriterProperties;
use uuid::Uuid;

use crate::store::Store;

/// Constructors for the error cases the shared write core produces.
/// Mirrors [`crate::decode::DecodeError`] on the read side: the core
/// stays generic and each writer's public error enum stays intact.
/// Each error enum implements this next to its own definition, so the
/// core depends on no concrete writer and a new writer plugs in
/// without editing this module.
pub(crate) trait WriteError: Sized {
    /// The underlying Parquet writer failed (write, flush, or footer).
    fn parquet(e: ParquetError) -> Self;
    /// A filesystem / object-store operation failed.
    fn io(op: &'static str, path: PathBuf, source: io::Error) -> Self;
    /// A prior append failed with a Parquet error; the buffer is
    /// undefined and the writer refuses further work.
    fn poisoned() -> Self;
}

/// The `/`-delimited object key for a partition's file: the partition's
/// Hive path rendered against an empty root, plus `<uuid>.parquet`.
/// Object keys are `/`-delimited regardless of the host OS, hence the
/// separator replacement.
pub(crate) fn object_key(partition_path: &Path, flush_uuid: Uuid) -> String {
    format!(
        "{}/{}.parquet",
        partition_path
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"),
        flush_uuid
    )
}

/// Shared scaffolding of the local-filesystem constructors: create the
/// partition directory and open a local [`Store`] rooted at
/// `bucket_root` (`Store::local` canonicalises the root, which must
/// therefore exist; the object-store `put` on close creates any
/// remaining parents).
pub(crate) fn open_local_store<E: WriteError>(bucket_root: &Path, dir: &Path) -> Result<Store, E> {
    std::fs::create_dir_all(dir)
        .map_err(|source| E::io("create_dir_all", dir.to_path_buf(), source))?;
    Store::local(bucket_root)
        .map_err(|e| E::io("open store", bucket_root.to_path_buf(), io::Error::other(e)))
}

/// Failure of [`write_chunked`], split by origin so the poison decision
/// is structural: only [`ChunkError::Parquet`] (a failed
/// `ArrowWriter::write` / `flush`, buffer state undefined) poisons;
/// [`ChunkError::Encode`] comes from the row-to-batch encoder, which
/// never touches the buffer, so the writer remains usable.
pub(crate) enum ChunkError<E> {
    Parquet(ParquetError),
    Encode(E),
}

impl<E: WriteError> ChunkError<E> {
    pub(crate) fn into_write_error(self) -> E {
        match self {
            Self::Parquet(e) => E::parquet(e),
            Self::Encode(e) => e,
        }
    }
}

/// Chunked encode-and-write loop shared by both writers (and the
/// one-shot [`crate::encode_records_to_parquet`]). Splits `rows` into
/// `sub_batch_rows`-sized sub-batches and runs a
/// flush-if-over-threshold check before every sub-batch, so the bound
/// on row-group size is `flush_bytes` + one sub-batch's worth — well
/// under §3.5's 1 GiB upper bound for any reasonable per-row size. The
/// size check happens *before* every sub-batch (not after), so a
/// sub-batch that pushes the buffer past the threshold seals the next
/// time around — bounded overshoot is intentional; unbounded overshoot
/// is what the RFC prohibits. A final post-write check keeps the next
/// call from inheriting an over-threshold buffer.
pub(crate) fn write_chunked<R, E>(
    inner: &mut ArrowWriter<Vec<u8>>,
    rows: &[R],
    sub_batch_rows: usize,
    flush_bytes: usize,
    num_rows: &mut i64,
    encode: impl Fn(&[R]) -> Result<RecordBatch, E>,
) -> Result<(), ChunkError<E>> {
    for chunk in rows.chunks(sub_batch_rows) {
        if inner.in_progress_size() >= flush_bytes {
            inner.flush().map_err(ChunkError::Parquet)?;
        }
        let batch = encode(chunk).map_err(ChunkError::Encode)?;
        inner.write(&batch).map_err(ChunkError::Parquet)?;
        // Count rows only once the sub-batch has been accepted, so a
        // mid-slice failure leaves `num_rows` reflecting exactly what
        // landed in the buffer. `chunk.len()` is bounded by
        // `sub_batch_rows` (at most 1024 across all callers), so the
        // cast to `i64` is lossless.
        #[allow(clippy::cast_possible_wrap)]
        let written = chunk.len() as i64;
        *num_rows += written;
    }
    if inner.in_progress_size() >= flush_bytes {
        inner.flush().map_err(ChunkError::Parquet)?;
    }
    Ok(())
}

/// Result of a successful [`BufferedParquetWriter::close`]: the raw
/// facts of the published object. Each outer writer folds these into
/// its own result type ([`crate::WrittenFile`] /
/// [`crate::audit_writer::AuditWrittenFile`]) together with the
/// partition identity it kept.
#[derive(Debug)]
pub(crate) struct ClosedFile {
    pub(crate) key: String,
    pub(crate) path: PathBuf,
    pub(crate) num_rows: i64,
    pub(crate) bytes_written: u64,
}

/// The shared buffer-and-put writer core. Rows accumulate in an
/// in-memory `ArrowWriter`; nothing is published until
/// [`Self::close`]'s store `put` (the atomic commit point). Dropping
/// without `close` discards the buffer — there is no temp artifact to
/// clean up.
pub(crate) struct BufferedParquetWriter {
    /// `Some` from [`Self::open`] until [`Self::close`] consumes it.
    inner: Option<ArrowWriter<Vec<u8>>>,
    /// Object store the finished file is `put` to on close.
    store: Store,
    /// `/`-delimited object key the file is published to, relative to
    /// the store root — the backend-agnostic address.
    key: String,
    /// The object key rendered as a path by default; the
    /// local-filesystem constructors override it with the absolute
    /// landing path via [`Self::set_final_path`]. For S3 this is not a
    /// filesystem path (readers address the file by `key`).
    final_path: PathBuf,
    /// Running count of rows written so far (incremented per
    /// sub-batch as each `write` succeeds); reported by
    /// [`Self::close`]. Tracked directly because `into_inner` returns
    /// the buffer, not file metadata.
    num_rows: i64,
    /// In-progress bytes at which a row group seals. Fixed at open
    /// time.
    flush_bytes: usize,
    /// Set to `true` once any `ArrowWriter::write` /
    /// `ArrowWriter::flush` call returns `Err`. The underlying
    /// `ArrowWriter`'s buffer state is undefined after such a failure
    /// (the row group may be partially written), so [`Self::close`]
    /// refuses to publish — putting a potentially corrupted buffer
    /// would land a bad file. The buffer is discarded (there is no
    /// on-disk artifact to inspect).
    poisoned: bool,
}

impl BufferedParquetWriter {
    /// Open a core for `key` on `store`. Buffer-and-put: encode into
    /// memory; nothing hits the store until [`Self::close`]. A
    /// construction failure leaves no artifact.
    pub(crate) fn open<E: WriteError>(
        store: &Store,
        key: String,
        schema: SchemaRef,
        props: WriterProperties,
        flush_bytes: usize,
    ) -> Result<Self, E> {
        let final_path = PathBuf::from(&key);
        let inner = ArrowWriter::try_new(Vec::new(), schema, Some(props)).map_err(E::parquet)?;
        Ok(Self {
            inner: Some(inner),
            store: store.clone(),
            key,
            final_path,
            num_rows: 0,
            flush_bytes,
            poisoned: false,
        })
    }

    /// Override the reported path with the absolute local landing path
    /// (the local-filesystem constructors; readers/tests join the store
    /// root to find the file).
    pub(crate) fn set_final_path(&mut self, path: PathBuf) {
        self.final_path = path;
    }

    /// The path reported through the written-file result: the absolute
    /// landing path for local writers, or the object key rendered as a
    /// path otherwise. The bytes only exist there after a successful
    /// `close` — while the writer is open they live in memory.
    pub(crate) fn final_path(&self) -> &Path {
        &self.final_path
    }

    /// Fail fast when a prior append poisoned the writer — touching
    /// `inner` after a Parquet error would call into an `ArrowWriter`
    /// whose buffer state is undefined. `close()` refuses to publish
    /// either way; surfacing the same error here lets callers stop
    /// driving the writer immediately instead of accumulating further
    /// (potentially doomed) Parquet operations.
    pub(crate) fn ensure_unpoisoned<E: WriteError>(&self) -> Result<(), E> {
        if self.poisoned {
            return Err(E::poisoned());
        }
        Ok(())
    }

    /// Append pre-validated rows through `encode`, chunked per
    /// [`write_chunked`]'s sizing rule. Poisons the writer on a
    /// Parquet failure (see [`ChunkError`] — encoder errors leave it
    /// usable). Callers run their partition pre-checks *before* this;
    /// the poisoned fail-fast is re-checked here so every path into
    /// the buffer is guarded.
    ///
    /// # Panics
    ///
    /// Structurally impossible. `inner` is `Some` from [`Self::open`]
    /// until [`Self::close`] takes ownership of `self`; `append`
    /// borrows `&mut self` and therefore cannot run after `close`.
    pub(crate) fn append<R, E: WriteError>(
        &mut self,
        rows: &[R],
        sub_batch_rows: usize,
        encode: impl Fn(&[R]) -> Result<RecordBatch, E>,
    ) -> Result<(), E> {
        self.ensure_unpoisoned::<E>()?;
        let inner = self
            .inner
            .as_mut()
            .expect("inner ArrowWriter is Some until close is called");
        match write_chunked(
            inner,
            rows,
            sub_batch_rows,
            self.flush_bytes,
            &mut self.num_rows,
            encode,
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                if matches!(e, ChunkError::Parquet(_)) {
                    self.poisoned = true;
                }
                Err(e.into_write_error())
            }
        }
    }

    /// Close the core, finalising the Parquet footer in the in-memory
    /// buffer and publishing the bytes to the object store under the
    /// key. Must be called for the file to be published; dropping
    /// without `close` discards the buffer and publishes nothing.
    ///
    /// **Atomic publish is logical, not crash-durable.** Once this
    /// method returns, the published object has a complete Parquet
    /// footer and any subsequent reader can open it. The store `put`
    /// is not `fsync`-ed, though — a host crash between the put and
    /// the OS's next page-cache flush could lose the file.
    /// Crash-survival durability is the WAL's domain (`CLAUDE.md`
    /// §3.4 "WAL-before-ack"); the Parquet writer is the storage tier
    /// and assumes its records are recoverable via WAL replay after a
    /// crash.
    ///
    /// **Poisoning check.** If a prior append failed with a Parquet
    /// error, the writer is poisoned and this method refuses to
    /// publish — returns the poisoned error and discards the buffer.
    ///
    /// # Errors
    ///
    /// - `E::poisoned()` when a prior append failed with a Parquet
    ///   error.
    /// - `E::parquet(..)` when the footer write fails.
    /// - `E::io(..)` when the store `put` fails. Nothing is published
    ///   in that case (object-store puts are atomic).
    ///
    /// # Panics
    ///
    /// Structurally impossible. `inner` is populated by [`Self::open`]
    /// and only consumed here; `close` takes `self` by value so it
    /// can't run twice.
    pub(crate) fn close<E: WriteError>(mut self) -> Result<ClosedFile, E> {
        if self.poisoned {
            // Refuse to publish a possibly-partial buffer.
            return Err(E::poisoned());
        }
        let inner = self
            .inner
            .take()
            .expect("close consumes self; inner is Some on entry");
        // `into_inner` writes the footer and returns the finished
        // bytes; the `put` is the atomic commit point.
        let bytes = inner.into_inner().map_err(E::parquet)?;
        let bytes_written = bytes.len() as u64;
        self.store
            .put_blocking(&self.key, bytes)
            .map_err(|e| E::io("put", self.final_path.clone(), io::Error::other(e)))?;
        Ok(ClosedFile {
            key: self.key,
            path: self.final_path,
            num_rows: self.num_rows,
            bytes_written,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Int64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema, SchemaRef};
    use parquet::file::properties::WriterProperties;

    use super::*;
    use crate::writer::WriterError;

    fn test_schema() -> SchemaRef {
        Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]))
    }

    fn batch_of(values: &[i64]) -> RecordBatch {
        RecordBatch::try_new(
            test_schema(),
            vec![Arc::new(Int64Array::from(values.to_vec()))],
        )
        .expect("test batch")
    }

    fn open_core(store: &Store) -> BufferedParquetWriter {
        BufferedParquetWriter::open::<WriterError>(
            store,
            "data/t/part/file.parquet".to_string(),
            test_schema(),
            WriterProperties::builder().build(),
            crate::writer::ROW_GROUP_FLUSH_BYTES,
        )
        .expect("open core")
    }

    #[test]
    fn object_key_is_slash_delimited_with_parquet_suffix() {
        let uuid = Uuid::now_v7();
        let key = object_key(Path::new("data").join("tenant_id=t").as_path(), uuid);
        assert_eq!(key, format!("data/tenant_id=t/{uuid}.parquet"));
    }

    #[test]
    fn close_publishes_bytes_and_reports_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::local(dir.path()).expect("store");
        let mut core = open_core(&store);
        let rows: Vec<i64> = (0..10).collect();
        core.append(&rows, 4, |chunk| Ok::<_, WriterError>(batch_of(chunk)))
            .expect("append");
        let closed = core.close::<WriterError>().expect("close");
        assert_eq!(closed.num_rows, 10);
        let bytes = store.get_blocking(&closed.key).expect("get");
        assert_eq!(closed.bytes_written, bytes.len() as u64);
    }

    #[test]
    fn encode_error_does_not_poison() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::local(dir.path()).expect("store");
        let mut core = open_core(&store);
        let rows = [1i64, 2, 3];
        let err = core
            .append(&rows, 8, |_| {
                Err::<RecordBatch, _>(WriterError::Batch(
                    crate::record_batch::BatchError::TimestampOverflow {
                        field: "time_unix_nano",
                        value: u64::MAX,
                    },
                ))
            })
            .expect_err("encode error surfaces");
        assert!(matches!(err, WriterError::Batch(_)));
        // The encoder never touched the buffer, so the writer stays
        // usable and close publishes normally.
        core.append(&rows, 8, |chunk| Ok::<_, WriterError>(batch_of(chunk)))
            .expect("append after encode error");
        let closed = core
            .close::<WriterError>()
            .expect("close after encode error");
        assert_eq!(closed.num_rows, 3);
    }

    #[test]
    fn parquet_error_poisons_appends_and_close() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::local(dir.path()).expect("store");
        let mut core = open_core(&store);
        // A batch whose schema disagrees with the writer's makes the
        // underlying `ArrowWriter::write` fail — the Parquet-origin
        // failure class that must poison.
        let wrong_schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, false)]));
        let wrong = RecordBatch::try_new(
            wrong_schema,
            vec![Arc::new(arrow_array::StringArray::from(vec!["x"]))],
        )
        .expect("wrong-schema batch");
        let rows = [1i64];
        let err = core
            .append(&rows, 8, |_| Ok::<_, WriterError>(wrong.clone()))
            .expect_err("schema-mismatched write fails");
        assert!(matches!(err, WriterError::Parquet(_)));
        let err = core
            .append(&rows, 8, |chunk| Ok::<_, WriterError>(batch_of(chunk)))
            .expect_err("poisoned append fails fast");
        assert!(matches!(err, WriterError::Poisoned));
        let err = core
            .close::<WriterError>()
            .expect_err("poisoned close refuses");
        assert!(matches!(err, WriterError::Poisoned));
    }
}
