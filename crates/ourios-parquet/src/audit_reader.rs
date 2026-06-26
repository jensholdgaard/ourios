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
//! **Unknown `event_kind` ordinals** surface as
//! [`AuditPayload::Unknown`] — an opaque envelope-only event — never
//! as a file failure, per RFC 0005 §3.7's unknown-`event_kind`
//! tolerance rule (amendment 2026-06-12, the rule pinned when kinds
//! 4–5 landed). This is the [`ParamType::Unknown`] discipline applied
//! to the kind enum: every future §3.8 ordinal addition stays
//! non-breaking for readers, and folds defined over named kinds
//! ignore unknown rows by construction.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use arrow_array::cast::AsArray;
use arrow_array::types::{Int32Type, TimestampNanosecondType, UInt8Type, UInt32Type, UInt64Type};
use arrow_array::{Array, RecordBatch, StructArray};
use ourios_core::alias::ActorId;
use ourios_core::audit::{AuditEvent, AuditPayload, ParamType, SlotExpansion, TemplateChange};
use ourios_core::tenant::TenantId;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::errors::ParquetError;

use crate::audit_columns;
use crate::audit_record_batch::{
    EVENT_KIND_ALIAS_ASSERTED, EVENT_KIND_ALIAS_RETRACTED, EVENT_KIND_COMPACTION,
    EVENT_KIND_TEMPLATE_CREATED, EVENT_KIND_TEMPLATE_TYPE_EXPANDED, EVENT_KIND_TEMPLATE_WIDENED,
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
    /// - [`AuditReaderError::Io`] on file-open failures
    ///   (filesystem-level errors from [`File::open`]).
    /// - [`AuditReaderError::Parquet`] on Parquet-footer parsing,
    ///   schema-parse, or reader-construction failures (anything
    ///   surfaced by `ParquetRecordBatchReaderBuilder::try_new`
    ///   or `build`).
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
        Self::from_chunk_reader(file, path.to_path_buf())
    }

    /// Open an audit file from in-memory bytes — the RFC 0019 `Store` read path
    /// (`Store::get_blocking` → bytes → here), so the audit scan reads through
    /// the object-storage seam (local or S3) rather than `std::fs`. Skips
    /// row-vs-path validation like [`Self::open_file`]; there is no filesystem
    /// access, so no [`AuditReaderError::Io`] is produced.
    ///
    /// # Errors
    ///
    /// [`AuditReaderError::Parquet`] / [`AuditReaderError::MissingRequiredColumn`]
    /// as [`Self::open_file`].
    pub fn open_bytes(bytes: bytes::Bytes) -> Result<Self, AuditReaderError> {
        Self::from_chunk_reader(bytes, PathBuf::from("<object-store>"))
    }

    /// Build the reader from any Parquet [`ChunkReader`] (a `File` or in-memory
    /// `Bytes`), enforcing the §3.7 baseline-REQUIRED-column check. Shared by
    /// [`Self::open_file`] and [`Self::open_bytes`].
    fn from_chunk_reader<R: parquet::file::reader::ChunkReader + 'static>(
        reader: R,
        file_path: PathBuf,
    ) -> Result<Self, AuditReaderError> {
        let builder =
            ParquetRecordBatchReaderBuilder::try_new(reader).map_err(AuditReaderError::Parquet)?;

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
            file_path,
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

    let tenant_id = required_string(batch, audit_columns::TENANT_ID, row_offset)?;
    let timestamp = required_timestamp(batch, audit_columns::TIMESTAMP, row_offset)?;
    let event_kind = required_u8(batch, audit_columns::EVENT_KIND, row_offset)?;
    // `event_type` is required-and-redundant (kept in sync with
    // `event_kind` by the writer); `event_kind` is the source of
    // truth for variant dispatch. The string is preserved verbatim
    // on the unknown-kind tolerance path (§3.7) so a read-then-write
    // round-trips the envelope.
    let event_type = required_string(batch, audit_columns::EVENT_TYPE, row_offset)?;
    // Template-group columns — OPTIONAL since the §3.7 amendment
    // (NULL on `compaction` rows). Required-by-convention for the
    // template kinds; [`require_at`] errors if a template row finds
    // one NULL.
    let template_id = optional_u64(batch, audit_columns::TEMPLATE_ID)?;
    let old_version = optional_u32(batch, audit_columns::OLD_VERSION)?;
    let new_version = optional_u32(batch, audit_columns::NEW_VERSION)?;
    let old_template = optional_string(batch, audit_columns::OLD_TEMPLATE)?.unwrap_or_default();
    let new_template = optional_string(batch, audit_columns::NEW_TEMPLATE)?.unwrap_or_default();
    let positions_widened_lists = decode_positions_column(batch, row_offset)?;
    let slots_expanded_lists = decode_slots_column(batch, row_offset)?;
    let triggering_line_hash = optional_fixed_bytes16(batch, audit_columns::TRIGGERING_LINE_HASH)?;
    let triggering_line_sample =
        optional_string(batch, audit_columns::TRIGGERING_LINE_SAMPLE)?.unwrap_or_default();
    let reason = optional_string(batch, audit_columns::REASON)?.unwrap_or_default();
    // Compaction-group columns (RFC 0009 §3.6).
    let compaction_partition =
        optional_string(batch, audit_columns::COMPACTION_PARTITION)?.unwrap_or_default();
    let compaction_input_files =
        optional_string_list(batch, audit_columns::COMPACTION_INPUT_FILES)?;
    let compaction_output_file =
        optional_string(batch, audit_columns::COMPACTION_OUTPUT_FILE)?.unwrap_or_default();
    let compaction_generation = optional_u64(batch, audit_columns::COMPACTION_GENERATION)?;
    let compaction_rows = optional_u64(batch, audit_columns::COMPACTION_ROWS)?;
    // Alias-group columns (RFC 0001 §6.7 / §3.7 amendment 2026-06-12).
    let alias_representative_id = optional_u64(batch, audit_columns::ALIAS_REPRESENTATIVE_ID)?;
    let alias_member_ids = optional_u64_list(batch, audit_columns::ALIAS_MEMBER_IDS)?;
    let alias_actor = optional_string(batch, audit_columns::ALIAS_ACTOR)?.unwrap_or_default();

    for i in 0..n {
        let file_row = row_offset + i;
        let ts = decode_timestamp(timestamp[i], file_row)?;
        let payload = match event_kind[i] {
            EVENT_KIND_TEMPLATE_CREATED
            | EVENT_KIND_TEMPLATE_WIDENED
            | EVENT_KIND_TEMPLATE_TYPE_EXPANDED
            | EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE => {
                let cols = TemplateColumns {
                    event_kind: event_kind[i],
                    old_version: &old_version,
                    new_version: &new_version,
                    old_template: &old_template,
                    new_template: &new_template,
                    positions_widened: &positions_widened_lists,
                    slots_expanded: &slots_expanded_lists,
                    reason: &reason,
                };
                AuditPayload::Template {
                    template_id: require_at(&template_id, i, audit_columns::TEMPLATE_ID, file_row)?,
                    triggering_line_hash: require_at(
                        &triggering_line_hash,
                        i,
                        audit_columns::TRIGGERING_LINE_HASH,
                        file_row,
                    )?,
                    triggering_line_sample: triggering_line_sample.get(i).and_then(Clone::clone),
                    change: decode_template_change(&cols, i, file_row)?,
                }
            }
            EVENT_KIND_COMPACTION => {
                let cols = CompactionColumns {
                    partition: &compaction_partition,
                    input_files: &compaction_input_files,
                    output_file: &compaction_output_file,
                    generation: &compaction_generation,
                    rows: &compaction_rows,
                };
                decode_compaction_payload(&cols, i, file_row)?
            }
            kind @ (EVENT_KIND_ALIAS_ASSERTED | EVENT_KIND_ALIAS_RETRACTED) => {
                let cols = AliasColumns {
                    representative_id: &alias_representative_id,
                    member_ids: &alias_member_ids,
                    actor: &alias_actor,
                    reason: &reason,
                };
                decode_alias_payload(kind, &cols, i, file_row)?
            }
            // RFC 0005 §3.7 unknown-event_kind tolerance (amendment
            // 2026-06-12): an ordinal above the known range surfaces
            // as an opaque envelope-only event, never a file failure.
            other => AuditPayload::Unknown {
                event_kind: other,
                event_type: event_type[i].clone(),
            },
        };

        events.push(AuditEvent {
            tenant_id: TenantId::new(tenant_id[i].clone()),
            timestamp: ts,
            payload,
        });
    }

    Ok(events)
}

/// Borrowed per-column slices the template-change decoder reads.
struct TemplateColumns<'a> {
    event_kind: u8,
    old_version: &'a [Option<u32>],
    new_version: &'a [Option<u32>],
    old_template: &'a [Option<String>],
    new_template: &'a [Option<String>],
    positions_widened: &'a [Vec<u16>],
    slots_expanded: &'a [Vec<SlotExpansion>],
    reason: &'a [Option<String>],
}

/// Borrowed per-column slices the compaction-payload decoder reads
/// (RFC 0009 §3.6 / §3.7 amendment 2026-06-03). All five columns are
/// required-by-convention non-null for kind 3.
struct CompactionColumns<'a> {
    partition: &'a [Option<String>],
    input_files: &'a [Option<Vec<String>>],
    output_file: &'a [Option<String>],
    generation: &'a [Option<u64>],
    rows: &'a [Option<u64>],
}

/// Rebuild the compaction payload for row `i` from the
/// `compaction_*` columns.
fn decode_compaction_payload(
    cols: &CompactionColumns,
    i: usize,
    file_row: usize,
) -> Result<AuditPayload, AuditReaderError> {
    Ok(AuditPayload::Compaction {
        partition: require_at(
            cols.partition,
            i,
            audit_columns::COMPACTION_PARTITION,
            file_row,
        )?,
        input_files: require_at(
            cols.input_files,
            i,
            audit_columns::COMPACTION_INPUT_FILES,
            file_row,
        )?,
        output_file: require_at(
            cols.output_file,
            i,
            audit_columns::COMPACTION_OUTPUT_FILE,
            file_row,
        )?,
        generation: require_at(
            cols.generation,
            i,
            audit_columns::COMPACTION_GENERATION,
            file_row,
        )?,
        rows: require_at(cols.rows, i, audit_columns::COMPACTION_ROWS, file_row)?,
    })
}

/// Borrowed per-column slices the alias-payload decoder reads
/// (RFC 0001 §6.7 / §3.7 amendment 2026-06-12).
struct AliasColumns<'a> {
    representative_id: &'a [Option<u64>],
    member_ids: &'a [Option<Vec<u64>>],
    actor: &'a [Option<String>],
    reason: &'a [Option<String>],
}

/// Rebuild the alias payload for row `i` from the `alias_*` columns.
/// All three alias columns are required-by-convention non-null for
/// kinds 4–5; `member_ids` may be the valid empty list (distinct from
/// NULL), and an on-disk NULL `reason` decodes to the in-memory empty
/// string (the §3.7 `"" ↔ NULL` round-trip rule).
fn decode_alias_payload(
    kind: u8,
    cols: &AliasColumns,
    i: usize,
    file_row: usize,
) -> Result<AuditPayload, AuditReaderError> {
    let representative_id = require_at(
        cols.representative_id,
        i,
        audit_columns::ALIAS_REPRESENTATIVE_ID,
        file_row,
    )?;
    let member_ids = require_at(
        cols.member_ids,
        i,
        audit_columns::ALIAS_MEMBER_IDS,
        file_row,
    )?;
    let actor_str = require_at(cols.actor, i, audit_columns::ALIAS_ACTOR, file_row)?;
    // Aliasing is never anonymous (RFC 0001 §6.7); an empty actor is
    // a writer-invariant violation.
    let actor = ActorId::new(actor_str).map_err(|e| AuditReaderError::Conversion {
        column: audit_columns::ALIAS_ACTOR,
        detail: format!("row {file_row}: {e}"),
    })?;
    let reason = cols
        .reason
        .get(i)
        .and_then(Clone::clone)
        .unwrap_or_default();
    if kind == EVENT_KIND_ALIAS_ASSERTED {
        Ok(AuditPayload::AliasAsserted {
            representative_id,
            member_ids,
            actor,
            reason,
        })
    } else {
        Ok(AuditPayload::AliasRetracted {
            representative_id,
            member_ids,
            actor,
            reason,
        })
    }
}

/// Value at `col[i]`, or a `Conversion` error if it is absent / NULL —
/// the writer-invariant violation a corrupt or foreign-writer file
/// would produce (a template row missing a template column, or a
/// compaction row missing a compaction column).
fn require_at<T: Clone>(
    col: &[Option<T>],
    i: usize,
    column: &'static str,
    file_row: usize,
) -> Result<T, AuditReaderError> {
    col.get(i)
        .and_then(Clone::clone)
        .ok_or_else(|| AuditReaderError::Conversion {
            column,
            detail: format!(
                "row {file_row}: NULL on a column required for this event_kind \
                 (writer-invariant violation)",
            ),
        })
}

/// Rebuild the [`TemplateChange`] for row `i` from the template-group
/// columns, enforcing the §3.7 `old_template == new_template`
/// invariant for the non-widening kinds.
fn decode_template_change(
    cols: &TemplateColumns,
    i: usize,
    file_row: usize,
) -> Result<TemplateChange, AuditReaderError> {
    // Creation has no prior template, so its `old_*` columns are NULL
    // (RFC 0017 §3.1) — handle it before `require_at` on `old_version` /
    // `old_template`, which would (correctly) reject those NULLs for the
    // widening kinds. The variant omits a version (a leaf is always born at
    // v1), so the on-disk `new_version` (canonically `1`) is not read back
    // into it — the v1 contract is structural, not a decoded value.
    if cols.event_kind == EVENT_KIND_TEMPLATE_CREATED {
        return Ok(TemplateChange::Created {
            new_template: require_at(cols.new_template, i, audit_columns::NEW_TEMPLATE, file_row)?,
        });
    }

    let old_version = require_at(cols.old_version, i, audit_columns::OLD_VERSION, file_row)?;
    let old_template = require_at(cols.old_template, i, audit_columns::OLD_TEMPLATE, file_row)?;

    match cols.event_kind {
        EVENT_KIND_TEMPLATE_WIDENED => Ok(TemplateChange::Widened {
            old_version,
            new_version: require_at(cols.new_version, i, audit_columns::NEW_VERSION, file_row)?,
            old_template,
            new_template: require_at(cols.new_template, i, audit_columns::NEW_TEMPLATE, file_row)?,
            positions_widened: cols.positions_widened[i].clone(),
        }),
        EVENT_KIND_TEMPLATE_TYPE_EXPANDED => {
            let new_template =
                require_at(cols.new_template, i, audit_columns::NEW_TEMPLATE, file_row)?;
            require_template_unchanged("TypeExpanded", &old_template, &new_template, file_row)?;
            Ok(TemplateChange::TypeExpanded {
                old_version,
                new_version: require_at(cols.new_version, i, audit_columns::NEW_VERSION, file_row)?,
                old_template,
                new_template,
                slots_expanded: cols.slots_expanded[i].clone(),
            })
        }
        // RejectedDegenerate (the match is only entered for the three
        // template ordinals).
        _ => {
            let new_template =
                require_at(cols.new_template, i, audit_columns::NEW_TEMPLATE, file_row)?;
            require_template_unchanged(
                "RejectedDegenerate",
                &old_template,
                &new_template,
                file_row,
            )?;
            // Recover would_be_* from the JSON-encoded `reason`; a
            // foreign writer that put a free-form string there falls
            // back to empty rather than erroring.
            let (would_be_template, would_be_positions) = cols
                .reason
                .get(i)
                .and_then(|r| r.as_deref())
                .and_then(decode_rejection_reason)
                .unwrap_or_default();
            Ok(TemplateChange::RejectedDegenerate {
                version: old_version,
                current_template: old_template,
                would_be_template,
                would_be_positions,
            })
        }
    }
}

/// The §3.7 invariant that the non-widening template kinds carry the
/// unchanged template in both `old_template` and `new_template`.
fn require_template_unchanged(
    variant: &str,
    old_template: &str,
    new_template: &str,
    file_row: usize,
) -> Result<(), AuditReaderError> {
    if old_template != new_template {
        return Err(AuditReaderError::Conversion {
            column: audit_columns::NEW_TEMPLATE,
            detail: format!(
                "row {file_row}: {variant} has old_template != new_template ({old_template:?} \
                 != {new_template:?}) — RFC 0005 §3.7 requires equality for this variant",
            ),
        });
    }
    Ok(())
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
        // OPTIONAL since the §3.7 amendment: a `compaction` row has a
        // NULL list here. Surface it as empty; the per-row dispatch
        // only reads this for the template kinds.
        if list.is_null(row_idx) {
            out.push(Vec::new());
            continue;
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
        // OPTIONAL since the §3.7 amendment: NULL on a `compaction`
        // row. Empty here; only the template kinds read it.
        if list.is_null(row_idx) {
            out.push(Vec::new());
            continue;
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
//
// Each helper takes `row_offset: usize` and uses it to compute
// `file_row = row_offset + i` for per-row `Conversion` error
// details, so every Conversion error this module produces reports
// the same file-global row index as the top-level
// `UnknownEventKind` / `TimestampDecode` / `PartitionMismatch`
// variants — the §3.9 "internal index convention" CodeRabbit
// flagged on the first round.

fn required_string(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
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
                detail: format!("row {}: null on a REQUIRED column", row_offset + i),
            });
        }
        out.push(arr.value(i).to_string());
    }
    Ok(out)
}

fn required_u8(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<u8>, AuditReaderError> {
    let col = required_column(batch, name)?;
    let arr = col
        .as_primitive_opt::<UInt8Type>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!("expected UInt8Array, got {:?}", col.data_type()),
        })?;
    materialize_required_primitive(arr, name, row_offset)
}

fn required_timestamp(
    batch: &RecordBatch,
    name: &'static str,
    row_offset: usize,
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
                detail: format!("row {}: null on a REQUIRED column", row_offset + i),
            });
        }
        out.push(arr.value(i));
    }
    Ok(out)
}

fn materialize_required_primitive<T: arrow_array::types::ArrowPrimitiveType>(
    arr: &arrow_array::PrimitiveArray<T>,
    name: &'static str,
    row_offset: usize,
) -> Result<Vec<T::Native>, AuditReaderError> {
    if arr.null_count() == 0 {
        return Ok(arr.values().to_vec());
    }
    for i in 0..arr.len() {
        if arr.is_null(i) {
            return Err(AuditReaderError::Conversion {
                column: name,
                detail: format!("row {}: null on a REQUIRED column", row_offset + i),
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

/// Per-row `Option<u64>` for a nullable `UInt64` column (the §3.7
/// `template_id` / `compaction_generation` / `compaction_rows`
/// columns).
fn optional_u64(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<Option<u64>>, AuditReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(Vec::new());
    };
    let arr = col
        .as_primitive_opt::<UInt64Type>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!("expected UInt64Array, got {:?}", col.data_type()),
        })?;
    Ok((0..arr.len())
        .map(|i| (!arr.is_null(i)).then(|| arr.value(i)))
        .collect())
}

/// Per-row `Option<u32>` for a nullable `UInt32` column.
fn optional_u32(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<Option<u32>>, AuditReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(Vec::new());
    };
    let arr = col
        .as_primitive_opt::<UInt32Type>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!("expected UInt32Array, got {:?}", col.data_type()),
        })?;
    Ok((0..arr.len())
        .map(|i| (!arr.is_null(i)).then(|| arr.value(i)))
        .collect())
}

/// Per-row `Option<[u8; 16]>` for the nullable `triggering_line_hash`
/// `FixedSizeBinary(16)` column.
fn optional_fixed_bytes16(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<Option<[u8; 16]>>, AuditReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(Vec::new());
    };
    let arr = col
        .as_fixed_size_binary_opt()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: format!("expected FixedSizeBinaryArray, got {:?}", col.data_type()),
        })?;
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        if arr.is_null(i) {
            out.push(None);
        } else {
            let mut buf = [0u8; 16];
            buf.copy_from_slice(arr.value(i));
            out.push(Some(buf));
        }
    }
    Ok(out)
}

/// Per-row `Option<Vec<String>>` for the nullable
/// `compaction_input_files` `LIST<STRING>` column. The element field
/// is non-nullable, so a NULL element is a corrupt row.
fn optional_string_list(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<Option<Vec<String>>>, AuditReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(Vec::new());
    };
    let list = col
        .as_list_opt::<i32>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: "column is not a LIST<STRING> as declared".to_string(),
        })?;
    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        if list.is_null(row_idx) {
            out.push(None);
            continue;
        }
        let elements = list.value(row_idx);
        let strs = elements
            .as_string_opt::<i32>()
            .ok_or_else(|| AuditReaderError::Conversion {
                column: name,
                detail: "list element is not Utf8".to_string(),
            })?;
        let mut row = Vec::with_capacity(strs.len());
        for i in 0..strs.len() {
            if strs.is_null(i) {
                return Err(AuditReaderError::Conversion {
                    column: name,
                    detail: format!(
                        "batch row {row_idx} element {i}: NULL but the element field is non-nullable",
                    ),
                });
            }
            row.push(strs.value(i).to_string());
        }
        out.push(Some(row));
    }
    Ok(out)
}

/// Per-row `Option<Vec<u64>>` for the nullable `alias_member_ids`
/// `LIST<UInt64>` column. NULL list ⇒ `None` (not an alias row);
/// empty list ⇒ `Some(vec![])` — the §3.7 empty-vs-NULL distinction.
/// The element field is non-nullable, so a NULL element is a corrupt
/// row.
fn optional_u64_list(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<Vec<Option<Vec<u64>>>, AuditReaderError> {
    let Some(col) = optional_column(batch, name) else {
        return Ok(Vec::new());
    };
    let list = col
        .as_list_opt::<i32>()
        .ok_or_else(|| AuditReaderError::Conversion {
            column: name,
            detail: "column is not a LIST<UInt64> as declared".to_string(),
        })?;
    let mut out = Vec::with_capacity(list.len());
    for row_idx in 0..list.len() {
        if list.is_null(row_idx) {
            out.push(None);
            continue;
        }
        let elements = list.value(row_idx);
        let ids = elements.as_primitive_opt::<UInt64Type>().ok_or_else(|| {
            AuditReaderError::Conversion {
                column: name,
                detail: "list element is not UInt64".to_string(),
            }
        })?;
        let mut row = Vec::with_capacity(ids.len());
        for i in 0..ids.len() {
            if ids.is_null(i) {
                return Err(AuditReaderError::Conversion {
                    column: name,
                    detail: format!(
                        "batch row {row_idx} element {i}: NULL but the element field is non-nullable",
                    ),
                });
            }
            row.push(ids.value(i));
        }
        out.push(Some(row));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    //! Colocated unit tests for the audit-reader paths that the
    //! integration tests (`tests/audit_round_trip.rs` /
    //! `tests/audit_row_vs_path_validation.rs`) don't exercise
    //! directly — specifically the file-global row-index
    //! propagation and the `decode_timestamp` failure paths.

    use super::*;
    use crate::audit_record_batch::audit_events_to_batch;
    use arrow_array::{ArrayRef, UInt8Array};
    use ourios_core::audit::{AuditEvent, AuditPayload, TemplateChange, hash_triggering_line};
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    fn widened_event(tenant: &str) -> AuditEvent {
        AuditEvent {
            tenant_id: ourios_core::tenant::TenantId::new(tenant),
            timestamp: UNIX_EPOCH + Duration::from_secs(1_775_127_480),
            payload: AuditPayload::Template {
                template_id: 7,
                triggering_line_hash: hash_triggering_line(b"line"),
                triggering_line_sample: None,
                change: TemplateChange::Widened {
                    old_version: 1,
                    new_version: 2,
                    old_template: "[\"user\",\"<*>\"]".to_string(),
                    new_template: "[\"user\",\"<*>\",\"<*>\"]".to_string(),
                    positions_widened: vec![1],
                },
            },
        }
    }

    /// FLIPPED from expect-error (`UnknownEventKind`) to
    /// expect-opaque-event per the RFC-gated contract change in
    /// RFC 0005 §3.7 (amendment 2026-06-12, PR #183): a reader
    /// encountering an `event_kind` ordinal above its known range
    /// MUST NOT fail the file — the row surfaces as an opaque
    /// envelope-only [`AuditPayload::Unknown`]. The old test pinned
    /// the documented deferral ("hard error until a real new variant
    /// lands"); kinds 4–5 were that variant, so the deferral is
    /// resolved and the old assertion is exactly the behaviour the
    /// amendment removes (`CLAUDE.md` §6.2).
    #[test]
    fn batch_to_audit_events_surfaces_unknown_event_kind_as_opaque_event() {
        // Arrange — replace the event_kind column with a single 99
        // ordinal (outside the §3.7 mapping table), keeping every
        // other column intact.
        let valid = audit_events_to_batch(&[widened_event("acme")]).expect("batch builds");
        let event_kind_idx = valid
            .schema()
            .index_of(audit_columns::EVENT_KIND)
            .expect("schema has event_kind");
        let mut columns: Vec<ArrayRef> = valid.columns().to_vec();
        columns[event_kind_idx] = Arc::new(UInt8Array::from(vec![99u8]));
        let forged =
            RecordBatch::try_new(valid.schema(), columns).expect("forged batch type-checks");

        // Act — decoding must NOT fail the batch.
        let events = batch_to_audit_events(&forged, 50).expect("unknown kind must not error");

        // Assert — the row decodes to the opaque envelope: the raw
        // ordinal plus the stored event_type string verbatim, with
        // the envelope fields (tenant, timestamp) preserved.
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tenant_id.as_str(), "acme");
        assert_eq!(events[0].timestamp, widened_event("acme").timestamp);
        match &events[0].payload {
            AuditPayload::Unknown {
                event_kind,
                event_type,
            } => {
                assert_eq!(*event_kind, 99);
                // The forged batch kept the original string column —
                // preserved verbatim, not re-derived from the ordinal.
                assert_eq!(event_type, "template_widened");
            }
            other => panic!("expected AuditPayload::Unknown, got {other:?}"),
        }
    }

    /// The opaque-envelope event round-trips: a read-then-write of an
    /// [`AuditPayload::Unknown`] row preserves the envelope verbatim
    /// (RFC 0005 §3.7 unknown-`event_kind` tolerance) and leaves
    /// every payload column NULL, so re-decoding yields the same
    /// opaque event.
    #[test]
    fn unknown_event_round_trips_envelope_only() {
        // Arrange — an opaque event as a reader would surface it.
        let unknown = AuditEvent {
            tenant_id: ourios_core::tenant::TenantId::new("acme"),
            timestamp: UNIX_EPOCH + Duration::from_secs(1_775_127_480),
            payload: AuditPayload::Unknown {
                event_kind: 42,
                event_type: "some_future_kind".to_string(),
            },
        };

        // Act — write it back out and decode again.
        let batch = audit_events_to_batch(std::slice::from_ref(&unknown)).expect("batch builds");
        let events = batch_to_audit_events(&batch, 0).expect("decode");

        // Assert — byte-for-byte envelope preservation.
        assert_eq!(events, vec![unknown]);
    }

    /// `decode_timestamp` rejects negative i64 nanos as
    /// `TimestampDecode` — covers the `u64::try_from` branch.
    /// The `checked_add` branch is defensive against narrow-
    /// `SystemTime` platforms (macOS / Linux both have wide
    /// enough ranges that `u64::MAX` nanos doesn't overflow);
    /// we cover the construction-time check explicitly via the
    /// negative-i64 path which all platforms hit identically.
    #[test]
    fn decode_timestamp_rejects_negative_i64() {
        let err = decode_timestamp(-1, 42).expect_err("negative i64 must error");
        match err {
            AuditReaderError::TimestampDecode { row_index, nanos } => {
                assert_eq!(row_index, 42);
                assert_eq!(nanos, -1);
            }
            other => panic!("expected TimestampDecode, got {other:?}"),
        }
    }

    /// `decode_timestamp` round-trips a valid i64 nanos value
    /// through `checked_add` to a `SystemTime` — sanity check
    /// that the defensive replacement of `+` with
    /// `checked_add(...)` didn't break the happy path.
    #[test]
    fn decode_timestamp_accepts_post_epoch_nanos() {
        // 2026-04-02T10:58:00Z = 1_775_127_480 secs.
        let ns = 1_775_127_480_000_000_000_i64;
        let t = decode_timestamp(ns, 0).expect("valid nanos must decode");
        let recovered = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("post-epoch")
            .as_nanos();
        assert_eq!(recovered, u128::try_from(ns).unwrap());
    }
}
