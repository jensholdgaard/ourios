//! Parquet data-file reader per RFC 0005 §3.9.
//!
//! Two entry points:
//!
//! - [`Reader::open_partition`] — production query path. Opens a
//!   single `<uuid>.parquet` file under a known
//!   [`PartitionKey`] and enforces the §3.9 row-vs-path
//!   validation (row-level `tenant_id` and the row's derived
//!   UTC year/month/day/hour, per the §3.4 fallback algorithm,
//!   must match the partition). Mismatch is a hard read error.
//!
//! - [`Reader::open_file`] — diagnostic single-file path. Skips
//!   row-vs-path validation. Surfaces records as stored. Not
//!   exposed through the production query path.
//!
//! Forward / backward compatibility (§3.9):
//!
//! - **Unknown columns are silently ignored.** A file with
//!   extra columns this reader doesn't know about reads through
//!   normally; the unknown columns are dropped.
//! - **Missing OPTIONAL columns surface as `None`.** Old files
//!   that pre-date an additive amendment read cleanly; the
//!   absent column defaults to `None` / empty `Vec` / sentinel
//!   per the column's declared type.
//! - **Missing baseline REQUIRED columns are a hard error.**
//!   A file lacking any of the columns RFC 0005 §3.2 declares
//!   REQUIRED is treated as corrupted; the reader refuses to
//!   surface records rather than making up defaults.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float32Type, Int32Type, TimestampNanosecondType, UInt8Type, UInt32Type, UInt64Type,
};
use arrow_array::{Array, RecordBatch, StructArray};
use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::errors::ParquetError;

use crate::columns;
use crate::partition::{PartitionKey, TimestampOverflowError};
use crate::store::{Store, StoreError};

/// Streaming Parquet reader for one data file.
///
/// One [`Reader`] reads one file. Use [`Reader::read_all`] to
/// pull every row into a `Vec<MinedRecord>` — sufficient for
/// MVP corpus replay and bench measurements. A future
/// streaming-iterator API can layer on once the bench has
/// measurable patterns.
pub struct Reader {
    inner: ParquetRecordBatchReader,
    partition: Option<PartitionKey>,
    file_path: PathBuf,
}

impl Reader {
    /// Open a file under a known [`PartitionKey`]. Every row
    /// surfaced via [`Self::read_all`] is validated against the
    /// partition: row-level `tenant_id` must equal
    /// `partition.tenant_id`, and the row's derived UTC year /
    /// month / day / hour (per §3.4's fallback algorithm —
    /// `time_unix_nano`, else `observed_time_unix_nano`, else
    /// the 1970 epoch) must match the partition's time-bucket
    /// tuple. Mismatch is a hard read error per §3.9.
    ///
    /// # Errors
    ///
    /// - [`ReaderError::Io`] on file-open / Parquet-footer
    ///   parsing failures.
    /// - [`ReaderError::Parquet`] on Parquet schema / reader
    ///   construction failures.
    /// - [`ReaderError::MissingRequiredColumn`] if the file's
    ///   schema lacks one of the §3.2 baseline REQUIRED columns.
    pub fn open_partition(path: &Path, partition: PartitionKey) -> Result<Self, ReaderError> {
        let mut reader = Self::open_file(path)?;
        reader.partition = Some(partition);
        Ok(reader)
    }

    /// Diagnostic single-file open. Skips §3.9 row-vs-path
    /// validation; surfaces records as stored. Not exposed
    /// through the production query path.
    ///
    /// # Errors
    ///
    /// Same set as [`Self::open_partition`].
    pub fn open_file(path: &Path) -> Result<Self, ReaderError> {
        let bytes = read_object(path)?;
        Self::from_bytes(bytes, path.to_path_buf())
    }

    /// Open a reader over in-memory Parquet `bytes` — the RFC 0013
    /// buffer-and-put read path (`Store.get` yields the object's bytes,
    /// decoded in memory). Applies the same RFC 0005 §3.9 baseline-column
    /// check as [`Self::open_file`]; no row-vs-path validation (there is no
    /// partition path on this path).
    ///
    /// # Errors
    /// [`ReaderError::Parquet`] if the bytes are not a valid Parquet file;
    /// [`ReaderError::MissingRequiredColumn`] if a baseline REQUIRED column
    /// is absent (§3.9).
    pub fn open_bytes(bytes: bytes::Bytes) -> Result<Self, ReaderError> {
        Self::from_bytes(bytes, PathBuf::from("<object-store>"))
    }

    /// Build a reader over in-memory Parquet `bytes`, recording `file_path`
    /// for diagnostics (the real path on the [`Self::open_file`] /
    /// [`Self::open_partition`] seam, a sentinel on [`Self::open_bytes`]).
    /// Applies the §3.9 baseline-column check; leaves `partition` unset.
    fn from_bytes(bytes: bytes::Bytes, file_path: PathBuf) -> Result<Self, ReaderError> {
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(bytes).map_err(ReaderError::Parquet)?;
        require_baseline_columns(builder.schema())?;
        let inner = builder.build().map_err(ReaderError::Parquet)?;

        Ok(Self {
            inner,
            partition: None,
            file_path,
        })
    }

    /// Read every row in the file as a `MinedRecord`. Applies
    /// row-vs-path validation when the reader was opened via
    /// [`Self::open_partition`].
    ///
    /// # Errors
    ///
    /// - [`ReaderError::Parquet`] on per-batch read failures.
    /// - [`ReaderError::Conversion`] when the file's column
    ///   data shape doesn't match what the reader expects
    ///   (logical-type mismatch, unexpected null on a REQUIRED
    ///   column, etc.).
    /// - [`ReaderError::AttributeDecode`] when an
    ///   `attributes` / `resource_attributes` column's bytes
    ///   fail the RFC 0005 §3.3 canonical-JSON decode (corrupt
    ///   file or foreign-producer bytes).
    /// - [`ReaderError::PartitionMismatch`] when a row's
    ///   derived partition disagrees with the writer-side
    ///   partition supplied to [`Self::open_partition`].
    pub fn read_all(self) -> Result<Vec<MinedRecord>, ReaderError> {
        let mut out = Vec::new();
        let partition = self.partition;
        let file_path = self.file_path;
        // Running file-level row offset so multi-batch files
        // report stable row indices across batches in
        // `PartitionMismatch` errors. Per-batch `enumerate()`
        // would reset to 0 every batch.
        let mut row_offset: usize = 0;
        for batch in self.inner {
            // `ParquetRecordBatchReader` yields `ArrowError`;
            // `From<ArrowError> for ParquetError` lets us
            // route everything through the same variant.
            let batch = batch.map_err(|e| ReaderError::Parquet(e.into()))?;
            let records = batch_to_mined_records(&batch, row_offset)?;
            if let Some(p) = &partition {
                for (idx_in_batch, r) in records.iter().enumerate() {
                    validate_row_vs_partition(r, p, row_offset + idx_in_batch, &file_path)?;
                }
            }
            row_offset += records.len();
            out.extend(records);
        }
        Ok(out)
    }
}

/// Errors produced by [`Reader`].
#[derive(Debug)]
pub enum ReaderError {
    /// Filesystem I/O failure (file open, footer read).
    Io {
        op: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// Parquet writer / reader failure (schema parse, codec
    /// failure, decode error).
    Parquet(ParquetError),
    /// The file's schema lacks one of the RFC 0005 §3.2 baseline
    /// REQUIRED columns. Treat as file corruption; the reader
    /// refuses to surface records rather than make up defaults.
    MissingRequiredColumn { name: String },
    /// The file's column data shape doesn't match what this
    /// reader expects — wrong logical type, unexpected null on
    /// a REQUIRED column, malformed list nesting, etc.
    Conversion {
        column: &'static str,
        detail: String,
    },
    /// An `attributes` / `resource_attributes` column carried
    /// bytes that the RFC 0005 §3.3 canonical-JSON decoder
    /// couldn't parse. Treat as file corruption — the writer
    /// only emits encoder-produced canonical bytes — or as a
    /// foreign producer that doesn't honour the §3.3 spec.
    /// Carries the row index for diagnostics.
    AttributeDecode {
        column: &'static str,
        row_index: usize,
        source: ourios_core::otlp::canonical::CanonicalJsonError,
    },
    /// A row's derived partition disagrees with the partition
    /// supplied to [`Reader::open_partition`]. RFC 0005 §3.9
    /// row-vs-path validation failed.
    PartitionMismatch {
        row_index: usize,
        file_path: PathBuf,
        expected: PartitionKey,
        actual: PartitionKey,
    },
    /// A row's `time_unix_nano` or `observed_time_unix_nano`
    /// can't fit in `i64` — same RFC 0005 §3.2 overflow contract
    /// the writer enforces.
    TimestampOverflow(TimestampOverflowError),
}

impl fmt::Display for ReaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, path, source } => write!(
                f,
                "filesystem I/O on `{op}` at {}: {source}",
                path.display(),
            ),
            Self::Parquet(e) => write!(f, "parquet reader: {e}"),
            Self::MissingRequiredColumn { name } => write!(
                f,
                "file is missing baseline REQUIRED column `{name}` (RFC 0005 §3.9: missing \
                 baseline columns are a hard read error)",
            ),
            Self::Conversion { column, detail } => {
                write!(f, "column `{column}` conversion failed: {detail}")
            }
            Self::AttributeDecode {
                column,
                row_index,
                source,
            } => write!(
                f,
                "column `{column}` row {row_index}: RFC 0005 §3.3 canonical-JSON decode \
                 failed: {source} (the writer only emits encoder-produced bytes; either the \
                 file is corrupt or a foreign producer wrote it)",
            ),
            Self::PartitionMismatch {
                row_index,
                file_path,
                expected,
                actual,
            } => write!(
                f,
                "row {row_index} in {} derives partition (tenant_id={}, year={:04}, \
                 month={:02}, day={:02}, hour={:02}) which does not match the file's open \
                 partition (tenant_id={}, year={:04}, month={:02}, day={:02}, hour={:02}) — \
                 RFC 0005 §3.9 row-vs-path contract",
                file_path.display(),
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
            Self::TimestampOverflow(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReaderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parquet(e) => Some(e),
            Self::TimestampOverflow(e) => Some(e),
            Self::MissingRequiredColumn { .. }
            | Self::Conversion { .. }
            | Self::PartitionMismatch { .. } => None,
            Self::AttributeDecode { source, .. } => Some(source),
        }
    }
}

fn validate_row_vs_partition(
    record: &MinedRecord,
    expected: &PartitionKey,
    row_index: usize,
    file_path: &Path,
) -> Result<(), ReaderError> {
    let actual = PartitionKey::derive(record).map_err(ReaderError::TimestampOverflow)?;
    if actual != *expected {
        return Err(ReaderError::PartitionMismatch {
            row_index,
            file_path: file_path.to_path_buf(),
            expected: expected.clone(),
            actual,
        });
    }
    Ok(())
}

/// RFC 0005 §3.9: every baseline REQUIRED (non-nullable) column must be
/// present in the file's schema, else a hard error. Shared by
/// [`Reader::open_file`] and [`Reader::open_bytes`].
/// Read a data file's bytes through the object-storage [`Store`] seam
/// (RFC 0013): a `LocalFileSystem`-backed store rooted at the file's parent
/// directory, keyed by the file name. The sync read path (compaction, tests)
/// thus goes through the same seam the S3 backend will, while keeping the
/// `&Path` API — `Store.get` is async, so this uses the blocking bridge.
fn read_object(path: &Path) -> Result<bytes::Bytes, ReaderError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ReaderError::Io {
            op: "object key",
            path: path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "file name is empty or not valid UTF-8",
            ),
        })?;
    let store = Store::local(parent).map_err(|e| store_io_err("open", path, &e))?;
    let bytes = store
        .get_blocking(name)
        .map_err(|e| store_io_err("get", path, &e))?;
    Ok(bytes::Bytes::from(bytes))
}

/// Map a [`StoreError`] from the read seam onto [`ReaderError::Io`], preserving
/// the file path and op for diagnostics. No consumer inspects the io kind of a
/// read failure (compaction wraps it opaquely; the querier reads via
/// `DataFusion`), so the backend cause rides in the message.
fn store_io_err(op: &'static str, path: &Path, err: &StoreError) -> ReaderError {
    ReaderError::Io {
        op,
        path: path.to_path_buf(),
        source: io::Error::other(err.to_string()),
    }
}

fn require_baseline_columns(file_schema: &arrow_schema::Schema) -> Result<(), ReaderError> {
    for expected_field in crate::data_schema().fields() {
        if !expected_field.is_nullable()
            && file_schema
                .column_with_name(expected_field.name())
                .is_none()
        {
            return Err(ReaderError::MissingRequiredColumn {
                name: expected_field.name().clone(),
            });
        }
    }
    Ok(())
}

/// Convert one Arrow `RecordBatch` to a `Vec<MinedRecord>` per
/// RFC 0005 §3.2. Handles the §3.9 "missing OPTIONAL column →
/// `None`" rule by checking column presence before unpacking.
fn batch_to_mined_records(
    batch: &RecordBatch,
    // File-global row offset of `batch`'s first row, threaded
    // from the caller so per-row diagnostics report stable
    // indices across multi-batch files. Per-batch
    // `enumerate()` would reset to 0 every batch and produce
    // ambiguous row numbers in `AttributeDecode` / similar.
    row_offset: usize,
) -> Result<Vec<MinedRecord>, ReaderError> {
    let n = batch.num_rows();
    let mut records: Vec<MinedRecord> = Vec::with_capacity(n);

    // Required columns — verified present at open time.
    let tenant_id = required_string(batch, columns::TENANT_ID)?;
    let template_id = required_u64(batch, columns::TEMPLATE_ID)?;
    let template_version = required_u32(batch, columns::TEMPLATE_VERSION)?;
    let time_unix_nano = required_timestamp(batch, columns::TIME_UNIX_NANO)?;
    let severity_number = required_u8(batch, columns::SEVERITY_NUMBER)?;
    let attributes = required_string(batch, columns::ATTRIBUTES)?;
    let dropped_attributes_count = required_u32(batch, columns::DROPPED_ATTRIBUTES_COUNT)?;
    let resource_attributes = required_string(batch, columns::RESOURCE_ATTRIBUTES)?;
    let flags = required_u32(batch, columns::FLAGS)?;
    let body_kind = required_u8(batch, columns::BODY_KIND)?;
    let confidence = required_f32(batch, columns::CONFIDENCE)?;
    let lossy_flag = required_bool(batch, columns::LOSSY_FLAG)?;

    // OPTIONAL columns — missing-column carve-out (§3.9): if
    // the file's schema doesn't carry the column, every row's
    // value defaults to `None`.
    let observed_time = optional_timestamp(batch, columns::OBSERVED_TIME_UNIX_NANO)?;
    let severity_text = optional_string(batch, columns::SEVERITY_TEXT)?;
    let scope_name = optional_string(batch, columns::SCOPE_NAME)?;
    let scope_version = optional_string(batch, columns::SCOPE_VERSION)?;
    let trace_id = optional_fixed_bytes16(batch, columns::TRACE_ID)?;
    let span_id = optional_fixed_bytes8(batch, columns::SPAN_ID)?;
    let event_name = optional_string(batch, columns::EVENT_NAME)?;
    let body = optional_binary(batch, columns::BODY)?;

    // List columns — `params` and `separators` are REQUIRED in
    // the schema but the list itself may be empty per RFC 0005
    // §3.2 (`Vec::new()` is a valid value).
    let params_lists = decode_params_column(batch)?;
    let separators_lists = decode_separators_column(batch)?;

    for i in 0..n {
        // Empty-list short-circuit mirrors the writer's
        // `append_attributes` — avoids the encoder round-trip
        // on every clean-attach record (the common case).
        let attrs_str = attributes[i].as_str();
        let decoded_attrs = if attrs_str == "[]" {
            Vec::new()
        } else {
            ourios_core::otlp::canonical::decode_attributes(attrs_str.as_bytes()).map_err(
                |source| ReaderError::AttributeDecode {
                    column: columns::ATTRIBUTES,
                    row_index: row_offset + i,
                    source,
                },
            )?
        };
        let res_str = resource_attributes[i].as_str();
        let decoded_resource = if res_str == "[]" {
            Vec::new()
        } else {
            ourios_core::otlp::canonical::decode_attributes(res_str.as_bytes()).map_err(
                |source| ReaderError::AttributeDecode {
                    column: columns::RESOURCE_ATTRIBUTES,
                    row_index: row_offset + i,
                    source,
                },
            )?
        };

        let t_ns = u64::try_from(time_unix_nano[i]).map_err(|_| ReaderError::Conversion {
            column: columns::TIME_UNIX_NANO,
            detail: "negative i64 timestamp can't be a u64 nanos-since-epoch".to_string(),
        })?;
        let observed_t = match &observed_time {
            None => None,
            Some(col) => col[i]
                .map(|v| {
                    u64::try_from(v).map_err(|_| ReaderError::Conversion {
                        column: columns::OBSERVED_TIME_UNIX_NANO,
                        detail: "negative i64 timestamp can't be a u64 nanos-since-epoch"
                            .to_string(),
                    })
                })
                .transpose()?,
        };

        let bk = decode_body_kind(body_kind[i])?;
        let body_string = match &body {
            None => None,
            // RFC 0005 §3.2: body is raw bytes; the in-memory
            // `MinedRecord.body` type is `Option<String>` today
            // (PR-E1 noted the future Bytes change). Lossy
            // UTF-8 decode keeps the round-trip working for
            // the UTF-8 writes that exist today; non-UTF-8 in
            // bytes-on-disk surface as U+FFFD replacements.
            Some(col) => col[i]
                .as_ref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
        };

        let record = MinedRecord {
            tenant_id: TenantId::new(tenant_id[i].clone()),
            template_id: template_id[i],
            template_version: template_version[i],
            severity_number: severity_number[i],
            severity_text: severity_text.as_ref().and_then(|c| c[i].clone()),
            scope_name: scope_name.as_ref().and_then(|c| c[i].clone()),
            scope_version: scope_version.as_ref().and_then(|c| c[i].clone()),
            time_unix_nano: t_ns,
            observed_time_unix_nano: observed_t,
            attributes: decoded_attrs,
            dropped_attributes_count: dropped_attributes_count[i],
            resource_attributes: decoded_resource,
            trace_id: trace_id.as_ref().and_then(|c| c[i]),
            span_id: span_id.as_ref().and_then(|c| c[i]),
            flags: flags[i],
            event_name: event_name.as_ref().and_then(|c| c[i].clone()),
            body_kind: bk,
            params: params_lists[i].clone(),
            separators: separators_lists[i].clone(),
            body: body_string,
            confidence: confidence[i],
            lossy_flag: lossy_flag[i],
        };
        // §3.2 reconstruction invariants — the writer rejects
        // these shapes too (`record_batch::Builders::append`),
        // so a file containing them indicates either corruption
        // or a foreign writer that doesn't honour the contract.
        validate_record_shape(&record, i)?;
        records.push(record);
    }

    Ok(records)
}

/// Enforce the RFC 0005 §3.2 / RFC 0001 §6.6 shape invariants
/// the writer guarantees on its inputs. The reader applies the
/// same checks against file contents so a foreign or corrupt
/// file can't yield a `MinedRecord` shape that downstream §6.6
/// reconstruction wouldn't be able to handle.
fn validate_record_shape(record: &MinedRecord, row_index: usize) -> Result<(), ReaderError> {
    if record.body_kind != BodyKind::String {
        return Ok(());
    }
    if record.lossy_flag {
        // Lossy String rows: §6.6 reconstruction returns the
        // retained body verbatim, so `body` must be present.
        if record.body.is_none() {
            return Err(ReaderError::Conversion {
                column: columns::BODY,
                detail: format!(
                    "row {row_index}: lossy_flag = true with body = None — RFC 0001 §6.6 \
                     reconstruction needs the retained body bytes",
                ),
            });
        }
    } else {
        // Clean attach: `separators.len() >= params.len() + 1`.
        // tokens.len() >= params.len() always, and separators
        // = tokens.len() + 1, so the lower bound is the
        // strictly-correct check the writer enforces too.
        let expected_at_least = record.params.len() + 1;
        if record.separators.len() < expected_at_least {
            return Err(ReaderError::Conversion {
                column: columns::SEPARATORS,
                detail: format!(
                    "row {row_index}: clean-attach String record has separators.len() = {} \
                     below the lower bound expected_at_least = {expected_at_least} \
                     (params.len() + 1) required by RFC 0005 §3.2",
                    record.separators.len(),
                ),
            });
        }
    }
    Ok(())
}

fn decode_body_kind(ord: u8) -> Result<BodyKind, ReaderError> {
    match ord {
        0 => Ok(BodyKind::String),
        1 => Ok(BodyKind::Structured),
        other => Err(ReaderError::Conversion {
            column: columns::BODY_KIND,
            detail: format!("unknown ordinal {other} (RFC 0005 §3.2 pins 0=String, 1=Structured)"),
        }),
    }
}

fn decode_param_type(ord: i32) -> ParamType {
    match ord {
        0 => ParamType::Ip,
        1 => ParamType::Uuid,
        2 => ParamType::Num,
        3 => ParamType::Hex,
        4 => ParamType::Ts,
        5 => ParamType::Path,
        6 => ParamType::Str,
        7 => ParamType::Overflow,
        // RFC 0005 §3.9: unknown `params.type_tag` ordinals
        // surface as `ParamType::Unknown(N)` so a file written
        // by a future writer with a new ParamType variant reads
        // through without error.
        other => ParamType::Unknown(other),
    }
}

fn decode_params_column(batch: &RecordBatch) -> Result<Vec<Vec<Param>>, ReaderError> {
    let idx = batch.schema().index_of(columns::PARAMS).map_err(|_| {
        ReaderError::MissingRequiredColumn {
            name: columns::PARAMS.to_string(),
        }
    })?;
    let list = batch
        .column(idx)
        .as_list_opt::<i32>()
        .ok_or_else(|| ReaderError::Conversion {
            column: columns::PARAMS,
            detail: "column is not a 3-level LIST as declared".to_string(),
        })?;

    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        if list.is_null(row_idx) {
            return Err(ReaderError::Conversion {
                column: columns::PARAMS,
                detail: format!(
                    "row {row_idx}: params list is NULL but the schema marks it REQUIRED",
                ),
            });
        }
        let elements = list.value(row_idx);
        let struct_arr = elements
            .as_any()
            .downcast_ref::<StructArray>()
            .ok_or_else(|| ReaderError::Conversion {
                column: columns::PARAMS,
                detail: "list element is not a STRUCT".to_string(),
            })?;
        let type_tag_col = struct_arr
            .column_by_name("type_tag")
            .ok_or_else(|| ReaderError::Conversion {
                column: columns::PARAMS,
                detail: "struct missing `type_tag` field".to_string(),
            })?
            .as_primitive_opt::<Int32Type>()
            .ok_or_else(|| ReaderError::Conversion {
                column: columns::PARAMS,
                detail: "`type_tag` is not Int32".to_string(),
            })?;
        let value_col =
            struct_arr
                .column_by_name("value")
                .ok_or_else(|| ReaderError::Conversion {
                    column: columns::PARAMS,
                    detail: "struct missing `value` field".to_string(),
                })?;
        let value_bin =
            value_col
                .as_binary_opt::<i32>()
                .ok_or_else(|| ReaderError::Conversion {
                    column: columns::PARAMS,
                    detail: "`value` is not BinaryArray".to_string(),
                })?;

        let mut row_params = Vec::with_capacity(struct_arr.len());
        for i in 0..struct_arr.len() {
            // The list-element struct itself is declared
            // non-nullable in `data_schema()` (the LIST's
            // `element` field carries `nullable: false`).
            // A NULL struct here is file corruption — surface
            // it before inspecting the inner type_tag / value
            // fields, since `is_null` on a NULL struct's
            // children would also fire and report a less
            // informative error.
            if struct_arr.is_null(i) {
                return Err(ReaderError::Conversion {
                    column: columns::PARAMS,
                    detail: format!(
                        "row {row_idx} param {i}: list-element struct is NULL but the schema \
                         marks the LIST element non-nullable",
                    ),
                });
            }
            // `type_tag` is declared non-nullable in §3.2.
            // NULL here is file corruption — surface as
            // Conversion rather than silently decoding to
            // `Ip` (ordinal 0).
            if type_tag_col.is_null(i) {
                return Err(ReaderError::Conversion {
                    column: columns::PARAMS,
                    detail: format!(
                        "row {row_idx} param {i}: type_tag is NULL but the schema marks the \
                         struct field non-nullable",
                    ),
                });
            }
            let type_tag = decode_param_type(type_tag_col.value(i));
            // The schema declares `value` as nullable, but the
            // writer never emits NULL (it always writes the
            // param's bytes). A NULL here therefore indicates
            // file corruption or a foreign writer; surface as
            // Conversion rather than collapsing NULL into the
            // empty-string sentinel where downstream consumers
            // can't tell them apart.
            if value_bin.is_null(i) {
                return Err(ReaderError::Conversion {
                    column: columns::PARAMS,
                    detail: format!(
                        "row {row_idx} param {i}: value is NULL — the writer never produces \
                         NULL value bytes, so this signals file corruption",
                    ),
                });
            }
            row_params.push(Param {
                type_tag,
                value: String::from_utf8_lossy(value_bin.value(i)).into_owned(),
            });
        }
        out.push(row_params);
    }
    Ok(out)
}

fn decode_separators_column(batch: &RecordBatch) -> Result<Vec<Vec<String>>, ReaderError> {
    let idx = batch.schema().index_of(columns::SEPARATORS).map_err(|_| {
        ReaderError::MissingRequiredColumn {
            name: columns::SEPARATORS.to_string(),
        }
    })?;
    let list = batch
        .column(idx)
        .as_list_opt::<i32>()
        .ok_or_else(|| ReaderError::Conversion {
            column: columns::SEPARATORS,
            detail: "column is not a 3-level LIST as declared".to_string(),
        })?;

    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        if list.is_null(row_idx) {
            return Err(ReaderError::Conversion {
                column: columns::SEPARATORS,
                detail: format!(
                    "row {row_idx}: separators list is NULL but the schema marks it REQUIRED",
                ),
            });
        }
        let elements = list.value(row_idx);
        let bin = elements
            .as_binary_opt::<i32>()
            .ok_or_else(|| ReaderError::Conversion {
                column: columns::SEPARATORS,
                detail: "list element is not BinaryArray".to_string(),
            })?;
        let mut row_seps = Vec::with_capacity(bin.len());
        for i in 0..bin.len() {
            // The schema declares the inner element non-nullable
            // (`Field::new("element", Binary, false)`). NULL on
            // this leaf is file corruption; surface as
            // Conversion rather than silently mapping to "".
            if bin.is_null(i) {
                return Err(ReaderError::Conversion {
                    column: columns::SEPARATORS,
                    detail: format!(
                        "row {row_idx} separator {i}: list element is NULL but the schema \
                         marks it non-nullable",
                    ),
                });
            }
            row_seps.push(String::from_utf8_lossy(bin.value(i)).into_owned());
        }
        out.push(row_seps);
    }
    Ok(out)
}

// --- Column accessors ---
//
// Each helper returns either a fully-materialised Vec of the
// column's values (REQUIRED columns) or an `Option<Vec<...>>`
// (OPTIONAL columns — `None` when the file's schema doesn't
// carry the column, per §3.9's missing-column carve-out).

fn required_string(batch: &RecordBatch, name: &'static str) -> Result<Vec<String>, ReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_string_opt::<i32>()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!("expected Utf8 string array, got {:?}", col.data_type()),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(ReaderError::Conversion {
                column: name,
                detail: format!("row {i}: null on a REQUIRED column"),
            });
        }
        out.push(arr.value(i).to_string());
    }
    Ok(out)
}

fn required_u64(batch: &RecordBatch, name: &'static str) -> Result<Vec<u64>, ReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<UInt64Type>()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!("expected UInt64Array, got {:?}", col.data_type()),
        })?;
    materialize_required_primitive(arr, name)
}

fn required_u32(batch: &RecordBatch, name: &'static str) -> Result<Vec<u32>, ReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<UInt32Type>()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!("expected UInt32Array, got {:?}", col.data_type()),
        })?;
    materialize_required_primitive(arr, name)
}

fn required_u8(batch: &RecordBatch, name: &'static str) -> Result<Vec<u8>, ReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<UInt8Type>()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!("expected UInt8Array, got {:?}", col.data_type()),
        })?;
    materialize_required_primitive(arr, name)
}

fn required_f32(batch: &RecordBatch, name: &'static str) -> Result<Vec<f32>, ReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<Float32Type>()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!("expected Float32Array, got {:?}", col.data_type()),
        })?;
    materialize_required_primitive(arr, name)
}

/// Materialise a primitive Arrow array into `Vec<T::Native>`,
/// erroring on any NULL slot. Plain `arr.values().to_vec()` would
/// silently turn NULL slots into zero (the underlying primitive
/// buffer's default fill), masking file corruption. Fast-paths
/// the null-free case so the common path is still a single
/// buffer copy.
fn materialize_required_primitive<T: arrow_array::types::ArrowPrimitiveType>(
    arr: &arrow_array::PrimitiveArray<T>,
    name: &'static str,
) -> Result<Vec<T::Native>, ReaderError> {
    if arr.null_count() == 0 {
        return Ok(arr.values().to_vec());
    }
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(ReaderError::Conversion {
                column: name,
                detail: format!("row {i}: null on a REQUIRED column"),
            });
        }
    }
    // Validity buffer reported nulls but no row matched —
    // unreachable in practice.
    Ok(arr.values().to_vec())
}

fn required_bool(batch: &RecordBatch, name: &'static str) -> Result<Vec<bool>, ReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_boolean_opt()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!("expected BooleanArray, got {:?}", col.data_type()),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(ReaderError::Conversion {
                column: name,
                detail: format!("row {i}: null on a REQUIRED column"),
            });
        }
        out.push(arr.value(i));
    }
    Ok(out)
}

fn required_timestamp(batch: &RecordBatch, name: &'static str) -> Result<Vec<i64>, ReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<TimestampNanosecondType>()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!(
                "expected TimestampNanosecondArray, got {:?}",
                col.data_type()
            ),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(ReaderError::Conversion {
                column: name,
                detail: format!("row {i}: null on a REQUIRED column"),
            });
        }
        out.push(arr.value(i));
    }
    Ok(out)
}

fn required_column<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a dyn Array, ReaderError> {
    let idx = batch
        .schema()
        .index_of(name)
        .map_err(|_| ReaderError::MissingRequiredColumn {
            name: name.to_string(),
        })?;
    Ok(batch.column(idx).as_ref())
}

fn optional_string(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<String>>>, ReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col
        .as_string_opt::<i32>()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!("expected Utf8 string array, got {:?}", col.data_type()),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        out.push(if arr.is_null(i) {
            None
        } else {
            Some(arr.value(i).to_string())
        });
    }
    Ok(Some(out))
}

fn optional_binary(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<Vec<u8>>>>, ReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col
        .as_binary_opt::<i32>()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!("expected BinaryArray, got {:?}", col.data_type()),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        out.push(if arr.is_null(i) {
            None
        } else {
            Some(arr.value(i).to_vec())
        });
    }
    Ok(Some(out))
}

fn optional_timestamp(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<i64>>>, ReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col
        .as_primitive_opt::<TimestampNanosecondType>()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!(
                "expected TimestampNanosecondArray, got {:?}",
                col.data_type()
            ),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        out.push(if arr.is_null(i) {
            None
        } else {
            Some(arr.value(i))
        });
    }
    Ok(Some(out))
}

fn optional_fixed_bytes16(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<[u8; 16]>>>, ReaderError> {
    optional_fixed_bytes::<16>(batch, name)
}

fn optional_fixed_bytes8(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<[u8; 8]>>>, ReaderError> {
    optional_fixed_bytes::<8>(batch, name)
}

fn optional_fixed_bytes<const N: usize>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<[u8; N]>>>, ReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col
        .as_fixed_size_binary_opt()
        .ok_or_else(|| ReaderError::Conversion {
            column: name,
            detail: format!("expected FixedSizeBinaryArray, got {:?}", col.data_type()),
        })?;
    if usize::try_from(arr.value_length()).ok() != Some(N) {
        return Err(ReaderError::Conversion {
            column: name,
            detail: format!(
                "expected FixedSizeBinary({N}), got FixedSizeBinary({})",
                arr.value_length(),
            ),
        });
    }
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        out.push(if arr.is_null(i) {
            None
        } else {
            let slice = arr.value(i);
            let mut buf = [0u8; N];
            buf.copy_from_slice(slice);
            Some(buf)
        });
    }
    Ok(Some(out))
}

fn optional_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Option<&'a dyn Array> {
    let idx = batch.schema().index_of(name).ok()?;
    Some(batch.column(idx).as_ref())
}

#[cfg(test)]
mod tests {
    //! Reader-side schema-evolution and shape-invariant scenarios
    //! per RFC 0005 §3.9. Colocated with the reader rather than
    //! living in `tests/` because every test here exercises the
    //! reader's behaviour against hand-rolled Parquet files — no
    //! writer involvement — and the shared helpers
    //! (`build_one_row_against`, `write_batch`) are reader-specific
    //! scaffolding that doesn't belong on the public surface.
    //!
    //! - **RFC0005.2** — Missing OPTIONAL column surfaces as `None`.
    //! - **RFC0005.3** — Unknown column silently ignored.
    //! - **RFC0005.4** — Missing baseline REQUIRED column → hard error.
    //! - **RFC0005.9** — Unknown `ParamType` ordinal → `Unknown(N)`.
    //! - Reader-side shape invariants (`lossy_flag` + body, separators
    //!   lower-bound) mirror the writer's `record_batch` rejections.

    use std::sync::Arc;

    use arrow_array::builder::{
        BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float32Builder, GenericListBuilder,
        Int32Builder, StringBuilder, StructBuilder, TimestampNanosecondBuilder, UInt8Builder,
        UInt32Builder, UInt64Builder,
    };
    use arrow_array::{ArrayRef, RecordBatch};
    use arrow_schema::{DataType, Field, Schema as ArrowSchema};
    use ourios_core::audit::ParamType;
    use ourios_core::record::{BodyKind, MinedRecord, Param};
    use ourios_core::tenant::TenantId;
    use parquet::arrow::ArrowWriter;
    use tempfile::TempDir;

    use super::{Reader, ReaderError};
    use crate::{PartitionKey, Writer, data_schema};

    /// Build a single-row `RecordBatch` against the test's
    /// chosen `ArrowSchema`. The schema may omit OPTIONAL columns
    /// or add extras; this builder fills sensible values for
    /// whatever columns the schema declares (mirroring the
    /// production schema shape for the matching names).
    //
    // `clippy::too_many_lines` — long flat match is the readable
    // shape here; splitting per-column constructors out hides
    // rather than helps.
    //
    // `clippy::match_same_arms` — `dropped_attributes_count` and
    // `flags` happen to share the "UInt32 zero" shape, but they're
    // semantically distinct columns and merging would hurt
    // readability when one of them later changes default.
    #[allow(clippy::too_many_lines, clippy::match_same_arms)]
    fn build_one_row_against(schema: Arc<ArrowSchema>) -> RecordBatch {
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
        for field in schema.fields() {
            let arr: ArrayRef = match field.name().as_str() {
                "tenant_id" => {
                    let mut b = StringBuilder::new();
                    b.append_value("tenant-a");
                    Arc::new(b.finish())
                }
                "template_id" => {
                    let mut b = UInt64Builder::new();
                    b.append_value(42);
                    Arc::new(b.finish())
                }
                "template_version" => {
                    let mut b = UInt32Builder::new();
                    b.append_value(1);
                    Arc::new(b.finish())
                }
                "time_unix_nano" => {
                    let mut b = TimestampNanosecondBuilder::new().with_timezone("UTC");
                    b.append_value(1_775_127_480_000_000_000);
                    Arc::new(b.finish())
                }
                "observed_time_unix_nano" => {
                    let mut b = TimestampNanosecondBuilder::new().with_timezone("UTC");
                    b.append_null();
                    Arc::new(b.finish())
                }
                "effective_time_unix_nano" => {
                    // Equals the row's non-zero `time_unix_nano`
                    // (the §3.2 derivation with observed = None).
                    let mut b = TimestampNanosecondBuilder::new().with_timezone("UTC");
                    b.append_value(1_775_127_480_000_000_000);
                    Arc::new(b.finish())
                }
                "severity_number" => {
                    let mut b = UInt8Builder::new();
                    b.append_value(9);
                    Arc::new(b.finish())
                }
                "severity_text" | "scope_name" | "scope_version" | "event_name" => {
                    let mut b = StringBuilder::new();
                    b.append_null();
                    Arc::new(b.finish())
                }
                "attributes" | "resource_attributes" => {
                    let mut b = StringBuilder::new();
                    b.append_value("[]");
                    Arc::new(b.finish())
                }
                "dropped_attributes_count" => {
                    let mut b = UInt32Builder::new();
                    b.append_value(0);
                    Arc::new(b.finish())
                }
                "trace_id" => {
                    let mut b = FixedSizeBinaryBuilder::new(16);
                    b.append_null();
                    Arc::new(b.finish())
                }
                "span_id" => {
                    let mut b = FixedSizeBinaryBuilder::new(8);
                    b.append_null();
                    Arc::new(b.finish())
                }
                "flags" => {
                    let mut b = UInt32Builder::new();
                    b.append_value(0);
                    Arc::new(b.finish())
                }
                "body_kind" => {
                    let mut b = UInt8Builder::new();
                    b.append_value(0); // String
                    Arc::new(b.finish())
                }
                "body" => {
                    let mut b = BinaryBuilder::new();
                    b.append_null();
                    Arc::new(b.finish())
                }
                "params" => {
                    let element_struct = StructBuilder::new(
                        vec![
                            Field::new("type_tag", DataType::Int32, false),
                            Field::new("value", DataType::Binary, true),
                        ],
                        vec![
                            Box::new(Int32Builder::new()),
                            Box::new(BinaryBuilder::new()),
                        ],
                    );
                    let mut b = GenericListBuilder::<i32, StructBuilder>::new(element_struct)
                        .with_field(Field::new(
                            "element",
                            DataType::Struct(
                                vec![
                                    Field::new("type_tag", DataType::Int32, false),
                                    Field::new("value", DataType::Binary, true),
                                ]
                                .into(),
                            ),
                            false,
                        ));
                    b.append(true); // empty list
                    Arc::new(b.finish())
                }
                "separators" => {
                    let mut b = GenericListBuilder::<i32, BinaryBuilder>::new(BinaryBuilder::new())
                        .with_field(Field::new("element", DataType::Binary, false));
                    // One element to satisfy the lower-bound check
                    // when read back, even though we're writing a
                    // single-row record.
                    b.values().append_value("");
                    b.append(true);
                    Arc::new(b.finish())
                }
                "confidence" => {
                    let mut b = Float32Builder::new();
                    b.append_value(0.0);
                    Arc::new(b.finish())
                }
                "lossy_flag" => {
                    let mut b = BooleanBuilder::new();
                    // Clean-attach shape: body is None (no
                    // retention needed), so the writer-side
                    // invariant (separators.len() >= params.len() + 1)
                    // is the relevant check. Our params=0 +
                    // separators=[""] satisfies it (1 >= 1).
                    b.append_value(false);
                    Arc::new(b.finish())
                }
                "future_column" => {
                    // Extra column the current reader doesn't know
                    // about — RFC0005.3 says it must be ignored.
                    let mut b = StringBuilder::new();
                    b.append_value("some-future-thing");
                    Arc::new(b.finish())
                }
                other => panic!("unhandled column in builder: {other}"),
            };
            arrays.push(arr);
        }
        RecordBatch::try_new(schema, arrays).expect("build batch")
    }

    fn write_batch(schema: Arc<ArrowSchema>, batch: &RecordBatch) -> tempfile::NamedTempFile {
        let file = tempfile::Builder::new()
            .suffix(".parquet")
            .tempfile()
            .expect("tempfile");
        let mut writer =
            ArrowWriter::try_new(file.reopen().unwrap(), schema, None).expect("writer");
        writer.write(batch).expect("write");
        writer.close().expect("close");
        file
    }

    /// Helper used by the shape-invariant tests to populate the
    /// "everything else" columns. Defers to
    /// `build_one_row_against` for the columns it overrides.
    fn single_field_array(field_name: &str) -> ArrayRef {
        let schema = data_schema();
        let target = schema
            .fields()
            .iter()
            .find(|f| f.name() == field_name)
            .unwrap_or_else(|| panic!("field {field_name} not in data_schema"));
        let single = Arc::new(ArrowSchema::new(vec![(**target).clone()]));
        let one_row = build_one_row_against(single);
        one_row.column(0).clone()
    }

    /// Scenario RFC0005.2 — Missing OPTIONAL column → `None`.
    #[test]
    fn rfc0005_2_missing_optional_column_surfaces_as_none() {
        // Schema omits `severity_text` (OPTIONAL in §3.2).
        let fields: Vec<Field> = data_schema()
            .fields()
            .iter()
            .filter(|f| f.name() != "severity_text")
            .map(|f| (**f).clone())
            .collect();
        let schema = Arc::new(ArrowSchema::new(fields));
        let batch = build_one_row_against(schema.clone());
        let f = write_batch(schema, &batch);

        let reader = Reader::open_file(f.path()).expect("open_file");
        let records = reader.read_all().expect("read_all");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].severity_text, None);
    }

    /// Scenario RFC0005.3 — Unknown column added → silently
    /// ignored on read.
    #[test]
    fn rfc0005_3_unknown_column_is_silently_ignored() {
        // Schema adds a `future_column` not in `data_schema()`.
        let mut fields: Vec<Field> = data_schema()
            .fields()
            .iter()
            .map(|f| (**f).clone())
            .collect();
        fields.push(Field::new("future_column", DataType::Utf8, true));
        let schema = Arc::new(ArrowSchema::new(fields));
        let batch = build_one_row_against(schema.clone());
        let f = write_batch(schema, &batch);

        let reader = Reader::open_file(f.path()).expect("open_file");
        let records = reader.read_all().expect("read_all");
        assert_eq!(records.len(), 1);
        // Reader doesn't surface the unknown column; the record
        // shape is unaffected.
        assert_eq!(records[0].tenant_id.as_str(), "tenant-a");
    }

    /// Scenario RFC0005.4 — Missing baseline REQUIRED column →
    /// hard error naming the column.
    #[test]
    fn rfc0005_4_missing_required_column_returns_hard_error() {
        // Schema omits `template_id` (REQUIRED in §3.2).
        let fields: Vec<Field> = data_schema()
            .fields()
            .iter()
            .filter(|f| f.name() != "template_id")
            .map(|f| (**f).clone())
            .collect();
        let schema = Arc::new(ArrowSchema::new(fields));
        let batch = build_one_row_against(schema.clone());
        let f = write_batch(schema, &batch);

        match Reader::open_file(f.path()) {
            Err(ReaderError::MissingRequiredColumn { name }) => assert_eq!(name, "template_id"),
            Err(other) => panic!("expected MissingRequiredColumn, got {other:?}"),
            Ok(_) => panic!("expected MissingRequiredColumn, got Ok"),
        }
    }

    /// Reader-side shape-invariant: a foreign-built file with
    /// `lossy_flag = true` and `body = NULL` is corruption. The
    /// reader mirrors the writer's `MissingBodyForLossyString`
    /// rejection — RFC 0001 §6.6 reconstruction has nothing to
    /// fall back to.
    #[test]
    fn reader_rejects_lossy_string_without_body() {
        let schema = data_schema();
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
        for field in schema.fields() {
            let arr: ArrayRef = match field.name().as_str() {
                "lossy_flag" => {
                    let mut b = BooleanBuilder::new();
                    b.append_value(true);
                    Arc::new(b.finish())
                }
                "body" => {
                    let mut b = BinaryBuilder::new();
                    b.append_null();
                    Arc::new(b.finish())
                }
                other => single_field_array(other),
            };
            arrays.push(arr);
        }
        let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
        let f = write_batch(schema, &batch);

        match Reader::open_file(f.path()).and_then(Reader::read_all) {
            Err(ReaderError::Conversion { column, .. }) => assert_eq!(column, "body"),
            Err(other) => panic!("expected Conversion on body, got {other:?}"),
            Ok(_) => panic!("expected Conversion error"),
        }
    }

    /// Reader-side shape-invariant: clean-attach String row with
    /// `separators` shorter than `params.len() + 1` is rejected,
    /// mirroring the writer's `InvalidSeparatorsForString`.
    #[test]
    fn reader_rejects_clean_string_with_too_few_separators() {
        let schema = data_schema();
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
        for field in schema.fields() {
            let arr: ArrayRef = match field.name().as_str() {
                "lossy_flag" => {
                    let mut b = BooleanBuilder::new();
                    b.append_value(false);
                    Arc::new(b.finish())
                }
                "params" => {
                    // Two params on disk; with the empty separators
                    // below, separators.len() = 0 <
                    // params.len() + 1 = 3.
                    let element_struct = StructBuilder::new(
                        vec![
                            Field::new("type_tag", DataType::Int32, false),
                            Field::new("value", DataType::Binary, true),
                        ],
                        vec![
                            Box::new(Int32Builder::new()),
                            Box::new(BinaryBuilder::new()),
                        ],
                    );
                    let mut b = GenericListBuilder::<i32, StructBuilder>::new(element_struct)
                        .with_field(Field::new(
                            "element",
                            DataType::Struct(
                                vec![
                                    Field::new("type_tag", DataType::Int32, false),
                                    Field::new("value", DataType::Binary, true),
                                ]
                                .into(),
                            ),
                            false,
                        ));
                    for _ in 0..2 {
                        b.values()
                            .field_builder::<Int32Builder>(0)
                            .unwrap()
                            .append_value(2);
                        b.values()
                            .field_builder::<BinaryBuilder>(1)
                            .unwrap()
                            .append_value(b"x");
                        b.values().append(true);
                    }
                    b.append(true);
                    Arc::new(b.finish())
                }
                "separators" => {
                    // Empty separators on a clean-attach String row.
                    let mut b = GenericListBuilder::<i32, BinaryBuilder>::new(BinaryBuilder::new())
                        .with_field(Field::new("element", DataType::Binary, false));
                    b.append(true);
                    Arc::new(b.finish())
                }
                other => single_field_array(other),
            };
            arrays.push(arr);
        }
        let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
        let f = write_batch(schema, &batch);

        match Reader::open_file(f.path()).and_then(Reader::read_all) {
            Err(ReaderError::Conversion { column, .. }) => assert_eq!(column, "separators"),
            Err(other) => panic!("expected Conversion on separators, got {other:?}"),
            Ok(_) => panic!("expected Conversion error"),
        }
    }

    /// Scenario RFC0005.9 — Unknown `ParamType` ordinal → reader
    /// returns `ParamType::Unknown(N)` rather than erroring.
    /// Round-trip path: the writer accepts `ParamType::Unknown(99)`
    /// (preserves the ordinal on disk) and the reader recovers it.
    #[test]
    fn rfc0005_9_unknown_param_type_round_trips_as_unknown_variant() {
        let bucket = TempDir::new().unwrap();

        let rec = MinedRecord {
            tenant_id: TenantId::new("tenant-x"),
            template_id: 1,
            template_version: 1,
            severity_number: 9,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            time_unix_nano: 1_775_127_480_000_000_000,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::String,
            params: vec![Param {
                type_tag: ParamType::Unknown(99),
                value: "unknown-variant-payload".to_string(),
            }],
            // params.len() + 1 = 2 ≤ separators.len() = 2.
            separators: vec![String::new(), String::new()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        };
        let partition = PartitionKey::derive(&rec).unwrap();

        let mut writer = Writer::open(bucket.path(), partition.clone()).unwrap();
        writer.append_records(&[rec]).unwrap();
        let written = writer.close().unwrap();

        let reader = Reader::open_partition(&written.path, partition).unwrap();
        let records = reader
            .read_all()
            .expect("unknown variant must read cleanly");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].params.len(), 1);
        match records[0].params[0].type_tag {
            ParamType::Unknown(99) => {}
            other => panic!("expected Unknown(99), got {other:?}"),
        }
        assert_eq!(records[0].params[0].value, "unknown-variant-payload");
    }
}
