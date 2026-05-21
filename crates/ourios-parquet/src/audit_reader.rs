//! Parquet audit-stream reader per RFC 0005 §3.7 / §3.9.
//!
//! Two entry points:
//!
//! - [`AuditReader::open_partition`] — production query path.
//!   Opens a single `<uuid>.parquet` file under a known
//!   [`PartitionKey`] and enforces row-vs-path validation on the
//!   audit axes (tenant + year/month/day; the audit partition
//!   path has no hour segment, so the hour field is ignored).
//!
//! - [`AuditReader::open_file`] — diagnostic single-file path.
//!   Skips row-vs-path validation; surfaces events as stored.
//!
//! Forward / backward compatibility per §3.9: unknown columns
//! silently ignored; missing OPTIONAL columns (`triggering_line_
//! sample`, `reason`) surface as `None`; missing baseline REQUIRED
//! columns are a hard read error.
//!
//! **Unknown `event_kind` ordinals** are currently surfaced as a
//! [`AuditReaderError::UnknownEventKind`] hard error. The audit
//! event enum [`AuditEventKind`] has no catch-all variant; a
//! future RFC 0005 §3.8 amendment that adds a new ordinal will
//! either extend the enum (and this match) or introduce an
//! `Unknown(u8)` variant analogous to [`ParamType::Unknown`]. The
//! data side handles the analogous case (`params.type_tag = 99`)
//! via `ParamType::Unknown`; the audit side defers the choice
//! until a real new variant lands.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use arrow_array::cast::AsArray;
use arrow_array::types::{Int32Type, TimestampNanosecondType, UInt8Type, UInt32Type, UInt64Type};
use arrow_array::{Array, RecordBatch, StructArray};
use ourios_core::audit::{AuditEvent, AuditEventKind, ParamType, SlotExpansion};
use ourios_core::tenant::TenantId;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::errors::ParquetError;

use crate::audit_columns;
use crate::audit_record_batch::{
    EVENT_KIND_TEMPLATE_TYPE_EXPANDED, EVENT_KIND_TEMPLATE_WIDENED,
    EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE,
};
use crate::audit_writer::{audit_partition_matches, derive_audit_partition};
use crate::partition::PartitionKey;

/// Streaming Parquet reader for one audit file.
pub struct AuditReader {
    inner: ParquetRecordBatchReader,
    partition: Option<PartitionKey>,
    file_path: PathBuf,
}

impl AuditReader {
    /// Open an audit file under a known [`PartitionKey`].
    ///
    /// # Errors
    ///
    /// - [`AuditReaderError::Io`] on file-open / Parquet-footer
    ///   parsing failures.
    /// - [`AuditReaderError::Parquet`] on Parquet schema / reader
    ///   construction failures.
    /// - [`AuditReaderError::MissingRequiredColumn`] if the
    ///   file's schema lacks one of the §3.7 baseline REQUIRED
    ///   columns.
    pub fn open_partition(path: &Path, partition: PartitionKey) -> Result<Self, AuditReaderError> {
        let mut reader = Self::open_file(path)?;
        reader.partition = Some(partition);
        Ok(reader)
    }

    /// Diagnostic single-file open. Skips §3.9 row-vs-path
    /// validation.
    ///
    /// # Errors
    ///
    /// Same set as [`Self::open_partition`].
    pub fn open_file(path: &Path) -> Result<Self, AuditReaderError> {
        let file = File::open(path).map_err(|source| AuditReaderError::Io {
            op: "open",
            path: path.to_path_buf(),
            source,
        })?;

        let builder =
            ParquetRecordBatchReaderBuilder::try_new(file).map_err(AuditReaderError::Parquet)?;

        let file_schema = builder.schema();
        for expected_field in crate::audit_schema().fields() {
            if !expected_field.is_nullable()
                && file_schema
                    .column_with_name(expected_field.name())
                    .is_none()
            {
                return Err(AuditReaderError::MissingRequiredColumn {
                    name: expected_field.name().clone(),
                });
            }
        }

        let inner = builder.build().map_err(AuditReaderError::Parquet)?;

        Ok(Self {
            inner,
            partition: None,
            file_path: path.to_path_buf(),
        })
    }

    /// Read every event in the file. Applies row-vs-path
    /// validation when the reader was opened via
    /// [`Self::open_partition`].
    ///
    /// # Errors
    ///
    /// See per-variant docs on [`AuditReaderError`].
    pub fn read_all(self) -> Result<Vec<AuditEvent>, AuditReaderError> {
        let mut out = Vec::new();
        let partition = self.partition;
        let file_path = self.file_path;
        let mut row_offset: usize = 0;
        for batch in self.inner {
            let batch = batch.map_err(|e| AuditReaderError::Parquet(e.into()))?;
            let events = batch_to_audit_events(&batch, row_offset)?;
            if let Some(p) = &partition {
                for (idx_in_batch, e) in events.iter().enumerate() {
                    validate_event_vs_partition(e, p, row_offset + idx_in_batch, &file_path)?;
                }
            }
            row_offset += events.len();
            out.extend(events);
        }
        Ok(out)
    }
}

/// Errors produced by [`AuditReader`].
#[derive(Debug)]
pub enum AuditReaderError {
    Io {
        op: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Parquet(ParquetError),
    /// File schema lacks one of the §3.7 baseline REQUIRED
    /// columns. Treat as file corruption.
    MissingRequiredColumn {
        name: String,
    },
    /// Column-data shape mismatch.
    Conversion {
        column: &'static str,
        detail: String,
    },
    /// `event_kind` ordinal isn't one of the §3.7 mapping
    /// table's values. Until a future amendment adds an
    /// `AuditEventKind::Unknown` variant, unknown ordinals are
    /// a hard error.
    UnknownEventKind {
        row_index: usize,
        ordinal: u8,
    },
    /// `timestamp` nanos couldn't be converted to `SystemTime`
    /// (negative — pre-epoch — or out of `Duration` range).
    TimestampDecode {
        row_index: usize,
        nanos: i64,
    },
    /// Row's derived audit partition disagrees with the
    /// partition supplied to [`AuditReader::open_partition`].
    PartitionMismatch {
        row_index: usize,
        file_path: PathBuf,
        expected: PartitionKey,
        actual: PartitionKey,
    },
}

impl fmt::Display for AuditReaderError {
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
                "audit file is missing baseline REQUIRED column `{name}` (RFC 0005 §3.9: \
                 missing baseline columns are a hard read error)",
            ),
            Self::Conversion { column, detail } => {
                write!(f, "column `{column}` conversion failed: {detail}")
            }
            Self::UnknownEventKind { row_index, ordinal } => write!(
                f,
                "row {row_index}: unknown event_kind ordinal {ordinal} — the §3.7 mapping \
                 table pins 0 / 1 / 2; reading an unknown ordinal needs an \
                 AuditEventKind::Unknown variant which is deferred until a real new variant \
                 lands via a §3.8 amendment",
            ),
            Self::TimestampDecode { row_index, nanos } => write!(
                f,
                "row {row_index}: timestamp = {nanos} ns can't be converted to SystemTime \
                 (negative or out of Duration range)",
            ),
            Self::PartitionMismatch {
                row_index,
                file_path,
                expected,
                actual,
            } => write!(
                f,
                "row {row_index} in {} derives audit partition (tenant_id={}, year={:04}, \
                 month={:02}, day={:02}) which does not match the file's open partition \
                 (tenant_id={}, year={:04}, month={:02}, day={:02}) — RFC 0005 §3.9 \
                 row-vs-path contract (audit axis: tenant + year/month/day)",
                file_path.display(),
                actual.tenant_id,
                actual.year,
                actual.month,
                actual.day,
                expected.tenant_id,
                expected.year,
                expected.month,
                expected.day,
            ),
        }
    }
}

impl std::error::Error for AuditReaderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parquet(e) => Some(e),
            Self::MissingRequiredColumn { .. }
            | Self::Conversion { .. }
            | Self::UnknownEventKind { .. }
            | Self::TimestampDecode { .. }
            | Self::PartitionMismatch { .. } => None,
        }
    }
}

fn validate_event_vs_partition(
    event: &AuditEvent,
    expected: &PartitionKey,
    row_index: usize,
    file_path: &Path,
) -> Result<(), AuditReaderError> {
    // Reuse the writer's derivation so writer and reader agree
    // bit-for-bit on the partition tuple. Map the writer-side
    // error variants to the reader-side equivalents.
    let actual = derive_audit_partition(event).map_err(|e| match e {
        crate::audit_writer::AuditWriterError::Batch(_) => AuditReaderError::TimestampDecode {
            row_index,
            // The writer-side error doesn't carry the raw nanos
            // back out; surface the row index and a placeholder
            // — the same row's `Self::Conversion` on the
            // timestamp column would have fired first in practice.
            nanos: 0,
        },
        // No other variant is reachable from
        // `derive_audit_partition`.
        _ => AuditReaderError::Conversion {
            column: audit_columns::TIMESTAMP,
            detail: format!("row {row_index}: audit partition derivation failed unexpectedly"),
        },
    })?;
    if !audit_partition_matches(&actual, expected) {
        return Err(AuditReaderError::PartitionMismatch {
            row_index,
            file_path: file_path.to_path_buf(),
            expected: expected.clone(),
            actual,
        });
    }
    Ok(())
}

/// Decode one batch's rows into [`AuditEvent`]s. `row_offset` is
/// the file-level index of this batch's first row (cumulative
/// row count across all prior batches in the same file), so every
/// row-index field on returned errors is file-global —
/// consistent with [`AuditReaderError::PartitionMismatch`] which
/// is also computed file-globally in [`AuditReader::read_all`].
fn batch_to_audit_events(
    batch: &RecordBatch,
    row_offset: usize,
) -> Result<Vec<AuditEvent>, AuditReaderError> {
    let n = batch.num_rows();
    let mut events: Vec<AuditEvent> = Vec::with_capacity(n);

    let tenant_id = required_string(batch, audit_columns::TENANT_ID)?;
    let timestamp = required_timestamp(batch, audit_columns::TIMESTAMP)?;
    let event_kind = required_u8(batch, audit_columns::EVENT_KIND)?;
    // `event_type` is required-and-redundant (kept in sync with
    // `event_kind` by the writer). Surface it for sanity-check
    // diagnostics but use `event_kind` as the source of truth
    // for variant dispatch.
    let _event_type = required_string(batch, audit_columns::EVENT_TYPE)?;
    let template_id = required_u64(batch, audit_columns::TEMPLATE_ID)?;
    let old_version = required_u32(batch, audit_columns::OLD_VERSION)?;
    let new_version = required_u32(batch, audit_columns::NEW_VERSION)?;
    let old_template = required_string(batch, audit_columns::OLD_TEMPLATE)?;
    let new_template = required_string(batch, audit_columns::NEW_TEMPLATE)?;
    let positions_widened_lists = decode_positions_column(batch, row_offset)?;
    let slots_expanded_lists = decode_slots_column(batch, row_offset)?;
    let triggering_line_hash = required_fixed_bytes16(batch, audit_columns::TRIGGERING_LINE_HASH)?;
    let triggering_line_sample = optional_string(batch, audit_columns::TRIGGERING_LINE_SAMPLE)?;
    let reason = optional_string(batch, audit_columns::REASON)?;

    for i in 0..n {
        let file_row = row_offset + i;
        let ts = decode_timestamp(timestamp[i], file_row)?;
        let kind = match event_kind[i] {
            EVENT_KIND_TEMPLATE_WIDENED => AuditEventKind::TemplateWidened {
                old_version: old_version[i],
                new_version: new_version[i],
                old_template: old_template[i].clone(),
                new_template: new_template[i].clone(),
                positions_widened: positions_widened_lists[i].clone(),
            },
            EVENT_KIND_TEMPLATE_TYPE_EXPANDED => AuditEventKind::TemplateTypeExpanded {
                old_version: old_version[i],
                new_version: new_version[i],
                old_template: old_template[i].clone(),
                new_template: new_template[i].clone(),
                slots_expanded: slots_expanded_lists[i].clone(),
            },
            EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE => {
                // Recover `would_be_template` / `would_be_positions`
                // from the JSON-encoded `reason` column. The
                // writer always emits this payload for the
                // rejection variant; a foreign writer that put a
                // free-form string in `reason` falls back to the
                // empty values rather than erroring (per the
                // module-level note in `audit_record_batch.rs`).
                let reason_str = reason.as_ref().and_then(|c| c[i].as_deref());
                let (would_be_template, would_be_positions) = reason_str
                    .and_then(decode_rejection_reason)
                    .unwrap_or_default();
                AuditEventKind::TemplateWideningRejectedDegenerate {
                    version: old_version[i],
                    current_template: old_template[i].clone(),
                    would_be_template,
                    would_be_positions,
                }
            }
            other => {
                return Err(AuditReaderError::UnknownEventKind {
                    row_index: file_row,
                    ordinal: other,
                });
            }
        };

        let event = AuditEvent {
            kind,
            tenant_id: TenantId::new(tenant_id[i].clone()),
            template_id: template_id[i],
            triggering_line_hash: triggering_line_hash[i],
            triggering_line_sample: triggering_line_sample.as_ref().and_then(|c| c[i].clone()),
            timestamp: ts,
        };
        events.push(event);
    }

    Ok(events)
}

/// Convert a non-negative i64 nanos-since-epoch to [`SystemTime`].
/// Uses [`SystemTime::checked_add`] so an out-of-`Duration`-range
/// nanos value (corrupt or foreign-writer file) returns a
/// structured error instead of panicking on the arithmetic.
fn decode_timestamp(nanos: i64, row_index: usize) -> Result<SystemTime, AuditReaderError> {
    let ns_u64 =
        u64::try_from(nanos).map_err(|_| AuditReaderError::TimestampDecode { row_index, nanos })?;
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_nanos(ns_u64))
        .ok_or(AuditReaderError::TimestampDecode { row_index, nanos })
}

/// Decode the rejection variant's `reason` column payload. The
/// writer encodes a JSON object with `would_be_template` (string)
/// and `would_be_positions` (array of integers). Returns `None`
/// if the payload doesn't parse — letting the caller fall back to
/// empty defaults rather than fail the read.
fn decode_rejection_reason(s: &str) -> Option<(String, Vec<u16>)> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let obj = v.as_object()?;
    let template = obj.get("would_be_template")?.as_str()?.to_string();
    let positions = obj
        .get("would_be_positions")?
        .as_array()?
        .iter()
        .map(|p| p.as_u64().and_then(|n| u16::try_from(n).ok()))
        .collect::<Option<Vec<_>>>()?;
    Some((template, positions))
}

fn decode_positions_column(
    batch: &RecordBatch,
    row_offset: usize,
) -> Result<Vec<Vec<u16>>, AuditReaderError> {
    let idx = batch
        .schema()
        .index_of(audit_columns::POSITIONS_WIDENED)
        .map_err(|_| AuditReaderError::MissingRequiredColumn {
            name: audit_columns::POSITIONS_WIDENED.to_string(),
        })?;
    let list =
        batch
            .column(idx)
            .as_list_opt::<i32>()
            .ok_or_else(|| AuditReaderError::Conversion {
                column: audit_columns::POSITIONS_WIDENED,
                detail: "column is not a LIST<INT32> as declared".to_string(),
            })?;
    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        let file_row = row_offset + row_idx;
        if list.is_null(row_idx) {
            return Err(AuditReaderError::Conversion {
                column: audit_columns::POSITIONS_WIDENED,
                detail: format!(
                    "row {file_row}: positions_widened list is NULL but the schema marks it \
                     REQUIRED",
                ),
            });
        }
        let elements = list.value(row_idx);
        let i32_arr = elements.as_primitive_opt::<Int32Type>().ok_or_else(|| {
            AuditReaderError::Conversion {
                column: audit_columns::POSITIONS_WIDENED,
                detail: "list element is not Int32".to_string(),
            }
        })?;
        let mut row = Vec::with_capacity(i32_arr.len());
        for i in 0..i32_arr.len() {
            if i32_arr.is_null(i) {
                return Err(AuditReaderError::Conversion {
                    column: audit_columns::POSITIONS_WIDENED,
                    detail: format!(
                        "row {file_row} position {i}: element is NULL but the schema marks \
                         it non-nullable",
                    ),
                });
            }
            let v = i32_arr.value(i);
            let p = u16::try_from(v).map_err(|_| AuditReaderError::Conversion {
                column: audit_columns::POSITIONS_WIDENED,
                detail: format!(
                    "row {file_row} position {i}: value {v} doesn't fit in u16 (RFC 0001 \
                     §6.4's `positions_widened: Vec<u16>`)",
                ),
            })?;
            row.push(p);
        }
        out.push(row);
    }
    Ok(out)
}

fn decode_slots_column(
    batch: &RecordBatch,
    row_offset: usize,
) -> Result<Vec<Vec<SlotExpansion>>, AuditReaderError> {
    let idx = batch
        .schema()
        .index_of(audit_columns::SLOTS_EXPANDED)
        .map_err(|_| AuditReaderError::MissingRequiredColumn {
            name: audit_columns::SLOTS_EXPANDED.to_string(),
        })?;
    let list =
        batch
            .column(idx)
            .as_list_opt::<i32>()
            .ok_or_else(|| AuditReaderError::Conversion {
                column: audit_columns::SLOTS_EXPANDED,
                detail: "column is not a LIST<STRUCT> as declared".to_string(),
            })?;

    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        let file_row = row_offset + row_idx;
        if list.is_null(row_idx) {
            return Err(AuditReaderError::Conversion {
                column: audit_columns::SLOTS_EXPANDED,
                detail: format!(
                    "row {file_row}: slots_expanded list is NULL but the schema marks it \
                     REQUIRED",
                ),
            });
        }
        out.push(decode_slot_row(&list.value(row_idx), file_row)?);
    }
    Ok(out)
}

/// Decode one row's worth of `slots_expanded` — the inner STRUCT
/// list from one row of the outer LIST. Split out so
/// `decode_slots_column` stays under clippy's `too_many_lines`
/// threshold; logic is otherwise identical to the inlined version.
fn decode_slot_row(
    elements: &arrow_array::ArrayRef,
    row_idx: usize,
) -> Result<Vec<SlotExpansion>, AuditReaderError> {
    let struct_arr = elements
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: audit_columns::SLOTS_EXPANDED,
            detail: "list element is not a STRUCT".to_string(),
        })?;
    let slot_index_col = struct_arr
        .column_by_name("slot_index")
        .ok_or_else(|| AuditReaderError::Conversion {
            column: audit_columns::SLOTS_EXPANDED,
            detail: "struct missing `slot_index` field".to_string(),
        })?
        .as_primitive_opt::<Int32Type>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: audit_columns::SLOTS_EXPANDED,
            detail: "`slot_index` is not Int32".to_string(),
        })?;
    let types_added_col =
        struct_arr
            .column_by_name("types_added")
            .ok_or_else(|| AuditReaderError::Conversion {
                column: audit_columns::SLOTS_EXPANDED,
                detail: "struct missing `types_added` field".to_string(),
            })?;
    let types_added_list =
        types_added_col
            .as_list_opt::<i32>()
            .ok_or_else(|| AuditReaderError::Conversion {
                column: audit_columns::SLOTS_EXPANDED,
                detail: "`types_added` is not LIST<INT32>".to_string(),
            })?;

    let mut row_slots = Vec::with_capacity(struct_arr.len());
    for i in 0..struct_arr.len() {
        row_slots.push(decode_one_slot(
            struct_arr,
            slot_index_col,
            types_added_list,
            row_idx,
            i,
        )?);
    }
    Ok(row_slots)
}

/// Decode one [`SlotExpansion`] from a single struct-array slot.
fn decode_one_slot(
    struct_arr: &StructArray,
    slot_index_col: &arrow_array::PrimitiveArray<Int32Type>,
    types_added_list: &arrow_array::GenericListArray<i32>,
    row_idx: usize,
    i: usize,
) -> Result<SlotExpansion, AuditReaderError> {
    if struct_arr.is_null(i) {
        return Err(AuditReaderError::Conversion {
            column: audit_columns::SLOTS_EXPANDED,
            detail: format!(
                "row {row_idx} slot {i}: list-element struct is NULL but the schema \
                 marks the LIST element non-nullable",
            ),
        });
    }
    if slot_index_col.is_null(i) {
        return Err(AuditReaderError::Conversion {
            column: audit_columns::SLOTS_EXPANDED,
            detail: format!(
                "row {row_idx} slot {i}: slot_index is NULL but the schema marks the \
                 field non-nullable",
            ),
        });
    }
    let slot_index_i32 = slot_index_col.value(i);
    let slot_index = u16::try_from(slot_index_i32).map_err(|_| AuditReaderError::Conversion {
        column: audit_columns::SLOTS_EXPANDED,
        detail: format!(
            "row {row_idx} slot {i}: slot_index = {slot_index_i32} doesn't fit in u16 \
             (RFC 0001 §6.4's `SlotExpansion::slot_index: u16`)",
        ),
    })?;
    if types_added_list.is_null(i) {
        return Err(AuditReaderError::Conversion {
            column: audit_columns::SLOTS_EXPANDED,
            detail: format!(
                "row {row_idx} slot {i}: types_added list is NULL but the schema marks it \
                 non-nullable",
            ),
        });
    }
    let types_elements = types_added_list.value(i);
    let types_i32 = types_elements
        .as_primitive_opt::<Int32Type>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: audit_columns::SLOTS_EXPANDED,
            detail: "`types_added` element is not Int32".to_string(),
        })?;
    let mut added_types = Vec::with_capacity(types_i32.len());
    for j in 0..types_i32.len() {
        if types_i32.is_null(j) {
            return Err(AuditReaderError::Conversion {
                column: audit_columns::SLOTS_EXPANDED,
                detail: format!(
                    "row {row_idx} slot {i} type {j}: element is NULL but the schema marks \
                     it non-nullable",
                ),
            });
        }
        added_types.push(decode_param_type(types_i32.value(j)));
    }
    Ok(SlotExpansion {
        slot_index,
        added_types,
    })
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
        other => ParamType::Unknown(other),
    }
}

// --- Column accessors mirrored from `reader.rs` ---
//
// Kept here rather than re-exporting the data-reader's helpers
// because the data reader's signatures are tied to its own error
// enum; sharing the helpers would force a `ReaderError ↔
// AuditReaderError` conversion that adds friction without
// removing real duplication.

fn required_string(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<String>, AuditReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_string_opt::<i32>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!("expected Utf8 string array, got {:?}", col.data_type()),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(AuditReaderError::Conversion {
                column: name,
                detail: format!("row {i}: null on a REQUIRED column"),
            });
        }
        out.push(arr.value(i).to_string());
    }
    Ok(out)
}

fn required_u64(batch: &RecordBatch, name: &'static str) -> Result<Vec<u64>, AuditReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<UInt64Type>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!("expected UInt64Array, got {:?}", col.data_type()),
        })?;
    materialize_required_primitive(arr, name)
}

fn required_u32(batch: &RecordBatch, name: &'static str) -> Result<Vec<u32>, AuditReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<UInt32Type>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!("expected UInt32Array, got {:?}", col.data_type()),
        })?;
    materialize_required_primitive(arr, name)
}

fn required_u8(batch: &RecordBatch, name: &'static str) -> Result<Vec<u8>, AuditReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<UInt8Type>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!("expected UInt8Array, got {:?}", col.data_type()),
        })?;
    materialize_required_primitive(arr, name)
}

fn required_timestamp(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<i64>, AuditReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<TimestampNanosecondType>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!(
                "expected TimestampNanosecondArray, got {:?}",
                col.data_type()
            ),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(AuditReaderError::Conversion {
                column: name,
                detail: format!("row {i}: null on a REQUIRED column"),
            });
        }
        out.push(arr.value(i));
    }
    Ok(out)
}

fn required_fixed_bytes16(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<[u8; 16]>, AuditReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_fixed_size_binary_opt()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!("expected FixedSizeBinaryArray, got {:?}", col.data_type()),
        })?;
    if usize::try_from(arr.value_length()).ok() != Some(16) {
        return Err(AuditReaderError::Conversion {
            column: name,
            detail: format!(
                "expected FixedSizeBinary(16), got FixedSizeBinary({})",
                arr.value_length(),
            ),
        });
    }
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(AuditReaderError::Conversion {
                column: name,
                detail: format!("row {i}: null on a REQUIRED column"),
            });
        }
        let slice = arr.value(i);
        let mut buf = [0u8; 16];
        buf.copy_from_slice(slice);
        out.push(buf);
    }
    Ok(out)
}

fn materialize_required_primitive<T: arrow_array::types::ArrowPrimitiveType>(
    arr: &arrow_array::PrimitiveArray<T>,
    name: &'static str,
) -> Result<Vec<T::Native>, AuditReaderError> {
    if arr.null_count() == 0 {
        return Ok(arr.values().to_vec());
    }
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(AuditReaderError::Conversion {
                column: name,
                detail: format!("row {i}: null on a REQUIRED column"),
            });
        }
    }
    Ok(arr.values().to_vec())
}

fn required_column<'a>(
    batch: &'a RecordBatch,
    name: &'static str,
) -> Result<&'a dyn Array, AuditReaderError> {
    let idx =
        batch
            .schema()
            .index_of(name)
            .map_err(|_| AuditReaderError::MissingRequiredColumn {
                name: name.to_string(),
            })?;
    Ok(batch.column(idx).as_ref())
}

fn optional_string(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Option<Vec<Option<String>>>, AuditReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(None);
    };
    let arr = col
        .as_string_opt::<i32>()
        .ok_or_else(|| AuditReaderError::Conversion {
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

fn optional_column<'a>(batch: &'a RecordBatch, name: &'static str) -> Option<&'a dyn Array> {
    let idx = batch.schema().index_of(name).ok()?;
    Some(batch.column(idx).as_ref())
}
