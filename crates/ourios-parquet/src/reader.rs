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
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float32Type, Int32Type, TimestampNanosecondType, UInt8Type, UInt32Type, UInt64Type,
};
use arrow_array::{Array, RecordBatch, StructArray};
use ourios_core::audit::ParamType;
use ourios_core::otlp::KeyValue;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::errors::ParquetError;

use crate::columns;
use crate::partition::{PartitionKey, TimestampOverflowError};

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
        let file = File::open(path).map_err(|source| ReaderError::Io {
            op: "open",
            path: path.to_path_buf(),
            source,
        })?;

        let builder =
            ParquetRecordBatchReaderBuilder::try_new(file).map_err(ReaderError::Parquet)?;

        // RFC 0005 §3.9: missing baseline REQUIRED columns →
        // hard error. Walk the expected schema and check each
        // non-nullable field is present in the file.
        let file_schema = builder.schema();
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

        let inner = builder.build().map_err(ReaderError::Parquet)?;

        Ok(Self {
            inner,
            partition: None,
            file_path: path.to_path_buf(),
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
    /// - [`ReaderError::AttributesNotYetDecoded`] when the
    ///   file contains a non-empty `attributes` /
    ///   `resource_attributes` JSON string. The canonical-JSON
    ///   decoder is symmetric to the writer's encoder — both
    ///   are deferred to the RFC 0005 §3.3 canonicalisation PR.
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
            let records = batch_to_mined_records(&batch)?;
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
    /// Non-empty `attributes` or `resource_attributes` column.
    /// The canonical-JSON decoder is deferred to the RFC 0005
    /// §3.3 canonicalisation PR (symmetric to the writer's
    /// `AttributesNotYetEncoded`).
    AttributesNotYetDecoded {
        column: &'static str,
        encoded: String,
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
            Self::AttributesNotYetDecoded { column, encoded } => write!(
                f,
                "column `{column}` carries non-empty canonical JSON ({encoded:?}) but the \
                 RFC 0005 §3.3 decoder is deferred to the canonicalisation PR (symmetric to \
                 the writer's `AttributesNotYetEncoded`)",
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
            | Self::AttributesNotYetDecoded { .. }
            | Self::PartitionMismatch { .. } => None,
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

/// Convert one Arrow `RecordBatch` to a `Vec<MinedRecord>` per
/// RFC 0005 §3.2. Handles the §3.9 "missing OPTIONAL column →
/// `None`" rule by checking column presence before unpacking.
fn batch_to_mined_records(batch: &RecordBatch) -> Result<Vec<MinedRecord>, ReaderError> {
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
        let attrs_str = attributes[i].as_str();
        if attrs_str != "[]" {
            return Err(ReaderError::AttributesNotYetDecoded {
                column: columns::ATTRIBUTES,
                encoded: attrs_str.to_string(),
            });
        }
        let res_str = resource_attributes[i].as_str();
        if res_str != "[]" {
            return Err(ReaderError::AttributesNotYetDecoded {
                column: columns::RESOURCE_ATTRIBUTES,
                encoded: res_str.to_string(),
            });
        }

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

        records.push(MinedRecord {
            tenant_id: TenantId::new(tenant_id[i].clone()),
            template_id: template_id[i],
            template_version: template_version[i],
            severity_number: severity_number[i],
            severity_text: severity_text.as_ref().and_then(|c| c[i].clone()),
            scope_name: scope_name.as_ref().and_then(|c| c[i].clone()),
            scope_version: scope_version.as_ref().and_then(|c| c[i].clone()),
            time_unix_nano: t_ns,
            observed_time_unix_nano: observed_t,
            attributes: Vec::new(),
            dropped_attributes_count: dropped_attributes_count[i],
            resource_attributes: Vec::<KeyValue>::new(),
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
        });
    }

    Ok(records)
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
