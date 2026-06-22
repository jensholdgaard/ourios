//! Arrow `RecordBatch` → `Vec<MinedRecord>` decode for the
//! querier's row-returning path (RFC 0017 §3.3).
//!
//! This is the arrow-58 port of `ourios-parquet`'s
//! `batch_to_mined_records` (and its column-extractor helpers),
//! which targets arrow-55. The querier reads its batches through
//! `DataFusion`, which re-exports arrow-58, so every Arrow type here
//! comes from `datafusion::arrow::*` rather than the bare
//! `arrow_array` / `arrow_schema` crates the parquet reader uses.
//!
//! Behavioural differences from the reference are deliberate and
//! limited to two points:
//!
//! - Every failure maps onto [`crate::QueryError::Storage`] with an
//!   informative `detail` (column name + row index) rather than the
//!   reader's richly-typed `ReaderError`.
//! - No `validate_record_shape` step. The querier's downstream
//!   `render_log_body` handles every record shape safely, so the
//!   decoder builds the `MinedRecord`s without rejecting rows.
//!
//! The §3.9 forward/backward-compatibility rules are preserved:
//! missing OPTIONAL columns surface as `None` / empty `Vec`, and
//! the empty-list `"[]"` attribute short-circuit mirrors the
//! writer's `append_attributes`.

use datafusion::arrow::array::{Array, AsArray, PrimitiveArray};
use datafusion::arrow::datatypes::{
    ArrowPrimitiveType, Float32Type, Int32Type, TimestampNanosecondType, UInt8Type, UInt32Type,
    UInt64Type,
};
use datafusion::arrow::record_batch::RecordBatch;
use ourios_core::audit::ParamType;
use ourios_core::otlp::KeyValue;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::columns;

use crate::QueryError;

/// Decode every row of every batch into `MinedRecord`s, in batch
/// order. A running `row_offset` is threaded across batches so
/// per-row diagnostics report stable indices across a multi-batch
/// result set (a per-batch `enumerate()` would reset to 0 each
/// batch and produce ambiguous row numbers).
///
/// # Errors
/// [`QueryError::Storage`] when a column is absent / wrongly typed,
/// carries an unexpected null on a REQUIRED column, fails the
/// RFC 0005 §3.3 canonical-JSON attribute decode, carries an
/// unknown `body_kind` ordinal, or carries a negative timestamp
/// that can't be a `u64` nanos-since-epoch.
pub(crate) fn batches_to_mined_records(
    batches: &[RecordBatch],
) -> Result<Vec<MinedRecord>, QueryError> {
    let mut out = Vec::new();
    let mut row_offset: usize = 0;
    for batch in batches {
        let records = batch_to_mined_records(batch, row_offset)?;
        row_offset += records.len();
        out.extend(records);
    }
    Ok(out)
}

fn storage(detail: String) -> QueryError {
    QueryError::Storage { detail }
}

// One linear column-by-column unpack — the length is inherent to the wide
// RFC 0005 §3.2 row schema (one read per column), not extractable logic.
#[allow(clippy::too_many_lines)]
fn batch_to_mined_records(
    batch: &RecordBatch,
    row_offset: usize,
) -> Result<Vec<MinedRecord>, QueryError> {
    let n = batch.num_rows();
    let mut records: Vec<MinedRecord> = Vec::with_capacity(n);

    // Required columns.
    let tenant_id = required_string(batch, columns::TENANT_ID, row_offset)?;
    let template_id = required_u64(batch, columns::TEMPLATE_ID, row_offset)?;
    let template_version = required_u32(batch, columns::TEMPLATE_VERSION, row_offset)?;
    let time_unix_nano = required_timestamp(batch, columns::TIME_UNIX_NANO, row_offset)?;
    let severity_number = required_u8(batch, columns::SEVERITY_NUMBER, row_offset)?;
    let attributes = required_string(batch, columns::ATTRIBUTES, row_offset)?;
    let dropped_attributes_count =
        required_u32(batch, columns::DROPPED_ATTRIBUTES_COUNT, row_offset)?;
    let resource_attributes = required_string(batch, columns::RESOURCE_ATTRIBUTES, row_offset)?;
    let flags = required_u32(batch, columns::FLAGS, row_offset)?;
    let body_kind = required_u8(batch, columns::BODY_KIND, row_offset)?;
    let confidence = required_f32(batch, columns::CONFIDENCE, row_offset)?;
    let lossy_flag = required_bool(batch, columns::LOSSY_FLAG, row_offset)?;

    // OPTIONAL columns — §3.9 missing-column carve-out: an absent
    // column yields `None` for every row.
    let observed_time = optional_timestamp(batch, columns::OBSERVED_TIME_UNIX_NANO)?;
    let severity_text = optional_string(batch, columns::SEVERITY_TEXT)?;
    let scope_name = optional_string(batch, columns::SCOPE_NAME)?;
    let scope_version = optional_string(batch, columns::SCOPE_VERSION)?;
    let trace_id = optional_fixed_bytes16(batch, columns::TRACE_ID)?;
    let span_id = optional_fixed_bytes8(batch, columns::SPAN_ID)?;
    let event_name = optional_string(batch, columns::EVENT_NAME)?;
    let scope_attributes = optional_string(batch, columns::SCOPE_ATTRIBUTES)?;
    let resource_schema_url = optional_string(batch, columns::RESOURCE_SCHEMA_URL)?;
    let scope_schema_url = optional_string(batch, columns::SCOPE_SCHEMA_URL)?;
    let body = optional_binary(batch, columns::BODY)?;

    let params_lists = decode_params_column(batch, row_offset)?;
    let separators_lists = decode_separators_column(batch, row_offset)?;

    for i in 0..n {
        let attrs_str = attributes[i].as_str();
        let decoded_attrs = if attrs_str == "[]" {
            Vec::new()
        } else {
            decode_attrs(attrs_str, columns::ATTRIBUTES, row_offset + i)?
        };
        let res_str = resource_attributes[i].as_str();
        let decoded_resource = if res_str == "[]" {
            Vec::new()
        } else {
            decode_attrs(res_str, columns::RESOURCE_ATTRIBUTES, row_offset + i)?
        };

        let decoded_scope_attrs =
            decode_optional_scope_attributes(scope_attributes.as_ref(), i, row_offset)?;

        let t_ns = u64::try_from(time_unix_nano[i]).map_err(|_| {
            storage(format!(
                "column `{}` row {}: negative i64 timestamp can't be a u64 nanos-since-epoch",
                columns::TIME_UNIX_NANO,
                row_offset + i,
            ))
        })?;
        let observed_t = match &observed_time {
            None => None,
            Some(col) => col[i]
                .map(|v| {
                    u64::try_from(v).map_err(|_| {
                        storage(format!(
                            "column `{}` row {}: negative i64 timestamp can't be a u64 \
                             nanos-since-epoch",
                            columns::OBSERVED_TIME_UNIX_NANO,
                            row_offset + i,
                        ))
                    })
                })
                .transpose()?,
        };

        let bk = decode_body_kind(body_kind[i], row_offset + i)?;
        let body_string = match &body {
            None => None,
            // RFC 0005 §3.2: body is raw bytes; the in-memory
            // `MinedRecord.body` is `Option<String>` today. Lossy
            // UTF-8 decode keeps the round-trip working for the
            // UTF-8 writes that exist; non-UTF-8 bytes surface as
            // U+FFFD replacements.
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
            scope_attributes: decoded_scope_attrs,
            resource_schema_url: resource_schema_url.as_ref().and_then(|c| c[i].clone()),
            scope_schema_url: scope_schema_url.as_ref().and_then(|c| c[i].clone()),
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
        records.push(record);
    }

    Ok(records)
}

fn decode_attrs(
    s: &str,
    column: &'static str,
    row_index: usize,
) -> Result<Vec<KeyValue>, QueryError> {
    ourios_core::otlp::canonical::decode_attributes(s.as_bytes()).map_err(|source| {
        storage(format!(
            "column `{column}` row {row_index}: RFC 0005 §3.3 canonical-JSON decode failed: \
             {source}",
        ))
    })
}

fn decode_body_kind(ord: u8, row_index: usize) -> Result<BodyKind, QueryError> {
    match ord {
        0 => Ok(BodyKind::String),
        1 => Ok(BodyKind::Structured),
        other => Err(storage(format!(
            "column `{}` row {row_index}: unknown body_kind ordinal {other} (RFC 0005 §3.2 pins \
             0=String, 1=Structured)",
            columns::BODY_KIND,
        ))),
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
        // RFC 0005 §3.9: unknown ordinals surface as `Unknown(N)`
        // so a file written by a future writer reads through.
        other => ParamType::Unknown(other),
    }
}

fn decode_params_column(
    batch: &RecordBatch,
    row_offset: usize,
) -> Result<Vec<Vec<Param>>, QueryError> {
    let idx = batch
        .schema()
        .index_of(columns::PARAMS)
        .map_err(|_| storage(format!("missing REQUIRED column `{}`", columns::PARAMS)))?;
    let list = batch.column(idx).as_list_opt::<i32>().ok_or_else(|| {
        storage(format!(
            "column `{}` is not a 3-level LIST as declared",
            columns::PARAMS,
        ))
    })?;

    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        let row = row_offset + row_idx;
        if list.is_null(row_idx) {
            return Err(storage(format!(
                "column `{}` row {row}: params list is NULL but the schema marks it REQUIRED",
                columns::PARAMS,
            )));
        }
        let elements = list.value(row_idx);
        let struct_arr = elements.as_struct_opt().ok_or_else(|| {
            storage(format!(
                "column `{}` row {row}: list element is not a STRUCT",
                columns::PARAMS,
            ))
        })?;
        let type_tag_col = struct_arr
            .column_by_name("type_tag")
            .ok_or_else(|| {
                storage(format!(
                    "column `{}` row {row}: struct missing `type_tag` field",
                    columns::PARAMS,
                ))
            })?
            .as_primitive_opt::<Int32Type>()
            .ok_or_else(|| {
                storage(format!(
                    "column `{}` row {row}: `type_tag` is not Int32",
                    columns::PARAMS,
                ))
            })?;
        let value_col = struct_arr.column_by_name("value").ok_or_else(|| {
            storage(format!(
                "column `{}` row {row}: struct missing `value` field",
                columns::PARAMS,
            ))
        })?;
        let value_bin = value_col.as_binary_opt::<i32>().ok_or_else(|| {
            storage(format!(
                "column `{}` row {row}: `value` is not BinaryArray",
                columns::PARAMS,
            ))
        })?;

        let mut row_params = Vec::with_capacity(struct_arr.len());
        for i in 0..struct_arr.len() {
            if struct_arr.is_null(i) {
                return Err(storage(format!(
                    "column `{}` row {row} param {i}: list-element struct is NULL but the \
                     schema marks the LIST element non-nullable",
                    columns::PARAMS,
                )));
            }
            if type_tag_col.is_null(i) {
                return Err(storage(format!(
                    "column `{}` row {row} param {i}: type_tag is NULL but the schema marks \
                     the struct field non-nullable",
                    columns::PARAMS,
                )));
            }
            let type_tag = decode_param_type(type_tag_col.value(i));
            if value_bin.is_null(i) {
                return Err(storage(format!(
                    "column `{}` row {row} param {i}: value is NULL — the writer never \
                     produces NULL value bytes, so this signals file corruption",
                    columns::PARAMS,
                )));
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

fn decode_separators_column(
    batch: &RecordBatch,
    row_offset: usize,
) -> Result<Vec<Vec<String>>, QueryError> {
    let idx = batch
        .schema()
        .index_of(columns::SEPARATORS)
        .map_err(|_| storage(format!("missing REQUIRED column `{}`", columns::SEPARATORS)))?;
    let list = batch.column(idx).as_list_opt::<i32>().ok_or_else(|| {
        storage(format!(
            "column `{}` is not a 3-level LIST as declared",
            columns::SEPARATORS,
        ))
    })?;

    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        let row = row_offset + row_idx;
        if list.is_null(row_idx) {
            return Err(storage(format!(
                "column `{}` row {row}: separators list is NULL but the schema marks it \
                 REQUIRED",
                columns::SEPARATORS,
            )));
        }
        let elements = list.value(row_idx);
        let bin = elements.as_binary_opt::<i32>().ok_or_else(|| {
            storage(format!(
                "column `{}` row {row}: list element is not BinaryArray",
                columns::SEPARATORS,
            ))
        })?;
        let mut row_seps = Vec::with_capacity(bin.len());
        for i in 0..bin.len() {
            if bin.is_null(i) {
                return Err(storage(format!(
                    "column `{}` row {row} separator {i}: list element is NULL but the schema \
                     marks it non-nullable",
                    columns::SEPARATORS,
                )));
            }
            row_seps.push(String::from_utf8_lossy(bin.value(i)).into_owned());
        }
        out.push(row_seps);
    }
    Ok(out)
}

// --- Column accessors ---
//
// Each helper returns a fully-materialised Vec for a REQUIRED column,
// or an `Option<Vec<...>>` for an OPTIONAL column (`None` when the
// file's schema doesn't carry the column, per §3.9).

fn required_string(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<String>, QueryError> {
    let col = required_column(batch, name)?;
    let arr = col.as_string_opt::<i32>().ok_or_else(|| {
        storage(format!(
            "column `{name}`: expected Utf8 string array, got {:?}",
            col.data_type(),
        ))
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(storage(format!(
                "column `{name}` row {}: null on a REQUIRED column",
                row_offset + i,
            )));
        }
        out.push(arr.value(i).to_string());
    }
    Ok(out)
}

fn required_u64(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<u64>, QueryError> {
    let col = required_column(batch, name)?;
    let arr = col.as_primitive_opt::<UInt64Type>().ok_or_else(|| {
        storage(format!(
            "column `{name}`: expected UInt64Array, got {:?}",
            col.data_type(),
        ))
    })?;
    materialize_required_primitive(arr, name, row_offset)
}

fn required_u32(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<u32>, QueryError> {
    let col = required_column(batch, name)?;
    let arr = col.as_primitive_opt::<UInt32Type>().ok_or_else(|| {
        storage(format!(
            "column `{name}`: expected UInt32Array, got {:?}",
            col.data_type(),
        ))
    })?;
    materialize_required_primitive(arr, name, row_offset)
}

fn required_u8(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<u8>, QueryError> {
    let col = required_column(batch, name)?;
    let arr = col.as_primitive_opt::<UInt8Type>().ok_or_else(|| {
        storage(format!(
            "column `{name}`: expected UInt8Array, got {:?}",
            col.data_type(),
        ))
    })?;
    materialize_required_primitive(arr, name, row_offset)
}

fn required_f32(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<f32>, QueryError> {
    let col = required_column(batch, name)?;
    let arr = col.as_primitive_opt::<Float32Type>().ok_or_else(|| {
        storage(format!(
            "column `{name}`: expected Float32Array, got {:?}",
            col.data_type(),
        ))
    })?;
    materialize_required_primitive(arr, name, row_offset)
}

/// Materialise a primitive Arrow array into `Vec<T::Native>`,
/// erroring on any NULL slot. Plain `arr.values().to_vec()` would
/// silently turn NULL slots into the buffer's zero fill, masking
/// corruption. Fast-paths the null-free case to a single buffer copy.
fn materialize_required_primitive<T: ArrowPrimitiveType>(
    arr: &PrimitiveArray<T>,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<T::Native>, QueryError> {
    if arr.null_count() == 0 {
        return Ok(arr.values().to_vec());
    }
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(storage(format!(
                "column `{name}` row {}: null on a REQUIRED column",
                row_offset + i,
            )));
        }
    }
    Ok(arr.values().to_vec())
}

fn required_bool(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<bool>, QueryError> {
    let col = required_column(batch, name)?;
    let arr = col.as_boolean_opt().ok_or_else(|| {
        storage(format!(
            "column `{name}`: expected BooleanArray, got {:?}",
            col.data_type(),
        ))
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(storage(format!(
                "column `{name}` row {}: null on a REQUIRED column",
                row_offset + i,
            )));
        }
        out.push(arr.value(i));
    }
    Ok(out)
}

fn required_timestamp(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<i64>, QueryError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<TimestampNanosecondType>()
        .ok_or_else(|| {
            storage(format!(
                "column `{name}`: expected TimestampNanosecondArray, got {:?}",
                col.data_type(),
            ))
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(storage(format!(
                "column `{name}` row {}: null on a REQUIRED column",
                row_offset + i,
            )));
        }
        out.push(arr.value(i));
    }
    Ok(out)
}

fn required_column<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a dyn Array, QueryError> {
    let idx = batch
        .schema()
        .index_of(name)
        .map_err(|_| storage(format!("missing REQUIRED column `{name}`")))?;
    Ok(batch.column(idx).as_ref())
}

/// RFC 0018 §3.1 — decode the per-row `scope_attributes` value: an
/// absent column, a NULL cell, or `"[]"` all mean "no scope
/// attributes" (empty vec); any other value is canonical JSON.
fn decode_optional_scope_attributes(
    scope_attributes: Option<&Vec<Option<String>>>,
    i: usize,
    row_offset: usize,
) -> Result<Vec<KeyValue>, QueryError> {
    let Some(col) = scope_attributes else {
        return Ok(Vec::new());
    };
    match col[i].as_deref() {
        None | Some("[]") => Ok(Vec::new()),
        Some(s) => decode_attrs(s, columns::SCOPE_ATTRIBUTES, row_offset + i),
    }
}

fn optional_string(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<String>>>, QueryError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col.as_string_opt::<i32>().ok_or_else(|| {
        storage(format!(
            "column `{name}`: expected Utf8 string array, got {:?}",
            col.data_type(),
        ))
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
) -> Result<Option<Vec<Option<Vec<u8>>>>, QueryError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col.as_binary_opt::<i32>().ok_or_else(|| {
        storage(format!(
            "column `{name}`: expected BinaryArray, got {:?}",
            col.data_type(),
        ))
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
) -> Result<Option<Vec<Option<i64>>>, QueryError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col
        .as_primitive_opt::<TimestampNanosecondType>()
        .ok_or_else(|| {
            storage(format!(
                "column `{name}`: expected TimestampNanosecondArray, got {:?}",
                col.data_type(),
            ))
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
) -> Result<Option<Vec<Option<[u8; 16]>>>, QueryError> {
    optional_fixed_bytes::<16>(batch, name)
}

fn optional_fixed_bytes8(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<[u8; 8]>>>, QueryError> {
    optional_fixed_bytes::<8>(batch, name)
}

fn optional_fixed_bytes<const N: usize>(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<[u8; N]>>>, QueryError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col.as_fixed_size_binary_opt().ok_or_else(|| {
        storage(format!(
            "column `{name}`: expected FixedSizeBinaryArray, got {:?}",
            col.data_type(),
        ))
    })?;
    if usize::try_from(arr.value_length()).ok() != Some(N) {
        return Err(storage(format!(
            "column `{name}`: expected FixedSizeBinary({N}), got FixedSizeBinary({})",
            arr.value_length(),
        )));
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
    use std::sync::Arc;

    use datafusion::arrow::array::builder::{BinaryBuilder, GenericListBuilder, StructBuilder};
    use datafusion::arrow::array::{
        ArrayRef, BooleanArray, Float32Array, Int32Builder, StringArray, TimestampNanosecondArray,
        UInt8Array, UInt32Array, UInt64Array,
    };
    use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    use datafusion::arrow::record_batch::RecordBatch;
    use ourios_core::audit::ParamType;
    use ourios_core::record::BodyKind;

    use super::*;

    /// One column the test wants in the batch: a `Field` plus the
    /// already-built array. The builder collects these in order and
    /// hands `RecordBatch::try_new` a matching `(schema, arrays)`.
    fn field(name: &'static str, dt: DataType, nullable: bool) -> Field {
        Field::new(name, dt, nullable)
    }

    fn ts_type() -> DataType {
        DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()))
    }

    /// Build a `params` list-of-struct array (struct fields
    /// `type_tag: Int32`, `value: Binary`) with one list per row;
    /// `rows[r]` is that row's params. Mirrors `ourios-parquet`'s
    /// `append_params` builder shape.
    fn params_array(rows: &[Vec<(i32, &[u8])>]) -> ArrayRef {
        let value_builder = StructBuilder::new(
            vec![
                Field::new("type_tag", DataType::Int32, false),
                Field::new("value", DataType::Binary, true),
            ],
            vec![
                Box::new(Int32Builder::new()),
                Box::new(BinaryBuilder::new()),
            ],
        );
        let mut builder: GenericListBuilder<i32, StructBuilder> =
            GenericListBuilder::new(value_builder).with_field(Field::new(
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
        for row in rows {
            for (tag, value) in row {
                let values = builder.values();
                values
                    .field_builder::<Int32Builder>(0)
                    .expect("type_tag field 0")
                    .append_value(*tag);
                values
                    .field_builder::<BinaryBuilder>(1)
                    .expect("value field 1")
                    .append_value(value);
                values.append(true);
            }
            builder.append(true);
        }
        Arc::new(builder.finish())
    }

    /// Build a `separators` list-of-binary array, one list per row.
    fn separators_array(rows: &[Vec<&[u8]>]) -> ArrayRef {
        let mut builder: GenericListBuilder<i32, BinaryBuilder> = GenericListBuilder::new(
            BinaryBuilder::new(),
        )
        .with_field(Field::new("element", DataType::Binary, false));
        for row in rows {
            for sep in row {
                builder.values().append_value(sep);
            }
            builder.append(true);
        }
        Arc::new(builder.finish())
    }

    /// The full REQUIRED-column set for a single-row batch, with
    /// every value supplied. Tests start from this and override /
    /// drop / add columns. Returns `(fields, arrays)` so a test can
    /// append OPTIONAL columns or swap a REQUIRED one for a NULL.
    fn required_columns_single_row() -> (Vec<Field>, Vec<ArrayRef>) {
        let fields = vec![
            field(columns::TENANT_ID, DataType::Utf8, false),
            field(columns::TEMPLATE_ID, DataType::UInt64, false),
            field(columns::TEMPLATE_VERSION, DataType::UInt32, false),
            field(columns::TIME_UNIX_NANO, ts_type(), false),
            field(columns::SEVERITY_NUMBER, DataType::UInt8, false),
            field(columns::ATTRIBUTES, DataType::Utf8, false),
            field(columns::DROPPED_ATTRIBUTES_COUNT, DataType::UInt32, false),
            field(columns::RESOURCE_ATTRIBUTES, DataType::Utf8, false),
            field(columns::FLAGS, DataType::UInt32, false),
            field(columns::BODY_KIND, DataType::UInt8, false),
            field(columns::CONFIDENCE, DataType::Float32, false),
            field(columns::LOSSY_FLAG, DataType::Boolean, false),
            field(
                columns::PARAMS,
                DataType::List(Arc::new(Field::new(
                    "element",
                    DataType::Struct(
                        vec![
                            Field::new("type_tag", DataType::Int32, false),
                            Field::new("value", DataType::Binary, true),
                        ]
                        .into(),
                    ),
                    false,
                ))),
                false,
            ),
            field(
                columns::SEPARATORS,
                DataType::List(Arc::new(Field::new("element", DataType::Binary, false))),
                false,
            ),
        ];
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(vec!["tenant-a"])),
            Arc::new(UInt64Array::from(vec![42u64])),
            Arc::new(UInt32Array::from(vec![7u32])),
            Arc::new(TimestampNanosecondArray::from(vec![1_000i64]).with_timezone("UTC")),
            Arc::new(UInt8Array::from(vec![9u8])),
            Arc::new(StringArray::from(vec!["[]"])),
            Arc::new(UInt32Array::from(vec![3u32])),
            Arc::new(StringArray::from(vec!["[]"])),
            Arc::new(UInt32Array::from(vec![1u32])),
            Arc::new(UInt8Array::from(vec![0u8])),
            Arc::new(Float32Array::from(vec![0.5f32])),
            Arc::new(BooleanArray::from(vec![false])),
            params_array(&[Vec::new()]),
            separators_array(&[vec![b"".as_slice()]]),
        ];
        (fields, arrays)
    }

    fn build_batch(fields: Vec<Field>, arrays: Vec<ArrayRef>) -> RecordBatch {
        let schema = Arc::new(Schema::new(fields));
        RecordBatch::try_new(schema, arrays).expect("test batch builds")
    }

    /// (1) A minimal all-REQUIRED-columns batch decodes to a
    /// `MinedRecord` with the expected field values, and pins the
    /// REQUIRED column set + their Arrow types.
    #[test]
    fn minimal_required_only_batch_decodes() {
        let (fields, arrays) = required_columns_single_row();
        let batch = build_batch(fields, arrays);
        let records = batches_to_mined_records(&[batch]).expect("decodes");
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(r.tenant_id.as_str(), "tenant-a");
        assert_eq!(r.template_id, 42);
        assert_eq!(r.template_version, 7);
        assert_eq!(r.time_unix_nano, 1_000);
        assert_eq!(r.severity_number, 9);
        assert_eq!(r.dropped_attributes_count, 3);
        assert_eq!(r.flags, 1);
        assert_eq!(r.body_kind, BodyKind::String);
        assert!((r.confidence - 0.5).abs() < f32::EPSILON);
        assert!(!r.lossy_flag);
        // OPTIONAL columns absent → None / empty.
        assert!(r.severity_text.is_none());
        assert!(r.scope_name.is_none());
        assert!(r.event_name.is_none());
        assert!(r.trace_id.is_none());
        assert!(r.span_id.is_none());
        assert!(r.observed_time_unix_nano.is_none());
        assert!(r.scope_attributes.is_empty());
        assert!(r.resource_schema_url.is_none());
        assert!(r.scope_schema_url.is_none());
        assert!(r.attributes.is_empty());
        assert!(r.resource_attributes.is_empty());
        assert!(r.params.is_empty());
        assert_eq!(r.separators, vec![String::new()]);
    }

    /// (2) §3.9: a missing OPTIONAL column surfaces as `None` /
    /// empty for that field. The minimal batch already omits every
    /// OPTIONAL column, so this asserts each maps to its empty
    /// reading explicitly.
    #[test]
    fn missing_optional_columns_decode_to_none() {
        let (fields, arrays) = required_columns_single_row();
        let batch = build_batch(fields, arrays);
        let r = &batches_to_mined_records(&[batch]).expect("decodes")[0];
        assert!(r.severity_text.is_none());
        assert!(r.scope_name.is_none());
        assert!(r.scope_version.is_none());
        assert!(r.event_name.is_none());
        assert!(r.trace_id.is_none());
        assert!(r.span_id.is_none());
        assert!(r.body.is_none());
        assert!(r.observed_time_unix_nano.is_none());
        // RFC 0018 OPTIONAL columns.
        assert!(r.scope_attributes.is_empty());
        assert!(r.resource_schema_url.is_none());
        assert!(r.scope_schema_url.is_none());
    }

    /// (3) A NULL in a REQUIRED column (`tenant_id`) is rejected.
    #[test]
    fn null_in_required_column_errors() {
        let (mut fields, mut arrays) = required_columns_single_row();
        // Declare the field nullable so `RecordBatch::try_new` admits
        // the NULL; the decoder's own REQUIRED-column NULL check is
        // what must reject it (a malformed file the schema can't catch).
        fields[0] = field(columns::TENANT_ID, DataType::Utf8, true);
        arrays[0] = Arc::new(StringArray::from(vec![None::<&str>]));
        let batch = build_batch(fields, arrays);
        let err = batches_to_mined_records(&[batch]).expect_err("NULL tenant_id must error");
        assert!(
            matches!(err, QueryError::Storage { .. }),
            "expected Storage, got {err:?}",
        );
    }

    /// (4) An unknown `body_kind` ordinal (99) is rejected — the
    /// §3.2 column pins 0=String, 1=Structured.
    #[test]
    fn unknown_body_kind_ordinal_errors() {
        let (fields, mut arrays) = required_columns_single_row();
        // body_kind is the 10th REQUIRED column (index 9).
        arrays[9] = Arc::new(UInt8Array::from(vec![99u8]));
        let batch = build_batch(fields, arrays);
        let err = batches_to_mined_records(&[batch]).expect_err("body_kind 99 must error");
        assert!(
            matches!(err, QueryError::Storage { .. }),
            "expected Storage, got {err:?}",
        );
    }

    /// (5) the `params` list-of-struct and `separators`
    /// list-of-binary columns round-trip to the right `Param` /
    /// `Vec<String>`.
    #[test]
    fn params_and_separators_round_trip() {
        let (fields, mut arrays) = required_columns_single_row();
        // Two params (NUM ordinal 2, IP ordinal 0) and two separators.
        arrays[12] = params_array(&[vec![(2, b"123"), (0, b"10.0.0.1")]]);
        arrays[13] = separators_array(&[vec![b"[".as_slice(), b"] ".as_slice(), b"".as_slice()]]);
        let batch = build_batch(fields, arrays);
        let r = &batches_to_mined_records(&[batch]).expect("decodes")[0];
        assert_eq!(
            r.params,
            vec![
                Param {
                    type_tag: ParamType::Num,
                    value: "123".to_string(),
                },
                Param {
                    type_tag: ParamType::Ip,
                    value: "10.0.0.1".to_string(),
                },
            ],
        );
        assert_eq!(
            r.separators,
            vec!["[".to_string(), "] ".to_string(), String::new()],
        );
    }

    /// (6) A negative `time_unix_nano` (i64) is rejected by the
    /// checked `i64`-to-`u64` conversion.
    #[test]
    fn negative_timestamp_errors() {
        let (fields, mut arrays) = required_columns_single_row();
        arrays[3] = Arc::new(TimestampNanosecondArray::from(vec![-1i64]).with_timezone("UTC"));
        let batch = build_batch(fields, arrays);
        let err = batches_to_mined_records(&[batch]).expect_err("negative ts must error");
        assert!(
            matches!(err, QueryError::Storage { .. }),
            "expected Storage, got {err:?}",
        );
    }
}
