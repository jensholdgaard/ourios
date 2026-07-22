//! `ourios-parquet` — Parquet schema, writer, reader, and
//! audit-event file series for Ourios.
//!
//! RFC 0005 is the normative on-disk contract. This crate
//! implements that contract; the §3.10 "Crate shape" plan splits
//! the work across follow-on PRs:
//!
//! 1. **scaffold** (this PR) — `data_schema()` / `audit_schema()`
//!    Arrow schemas plus column-name constants for greppability.
//!    The RFC0005.10 "schema-as-spec" pin test lands alongside.
//! 2. writer — `Writer` opening a file at a partition path,
//!    appending rows in the §3.2 column order, rotating row
//!    groups at the §3.5 threshold.
//! 3. reader — `Reader` with the §3.9 forward-/backward-compat
//!    contract (unknown columns ignored, missing OPTIONAL
//!    columns surface as `None`, row-vs-path validation).
//! 4. audit stream — `AuditWriter` / `AuditReader` for the §3.7
//!    parallel file series.
//!
//! Arrow's `SchemaRef` is the cross-crate interop point — the
//! follow-on writer/reader hand the same `SchemaRef` to
//! `parquet::arrow::ArrowWriter` / `ParquetRecordBatchReader`
//! without translating to `parquet::schema::types::SchemaDescriptor`
//! by hand.

#![deny(unsafe_code)]

pub mod audit_reader;
pub mod audit_record_batch;
pub mod audit_sink;
pub mod audit_writer;
pub mod compaction;
pub mod manifest;
pub mod partition;
pub mod promoted;
pub mod reader;
pub mod record_batch;
pub mod store;
pub mod writer;

pub use audit_reader::{AuditReader, AuditReaderError};
pub use audit_record_batch::{AuditBatchError, audit_events_to_batch};
pub use audit_sink::ParquetAuditSink;
pub use audit_writer::{AuditWriter, AuditWriterError, AuditWrittenFile, derive_audit_partition};
pub use compaction::{
    Committed, CompactionError, CompactionOutcome, CompactionPolicy, OrphanGc, compact_partition,
    compact_partition_with_flush_threshold, compact_partition_with_promoted, gc_orphans,
    plan_candidates,
};
pub use manifest::{MANIFEST_FILENAME, Manifest, ManifestError, Published};
pub use partition::{
    PartitionKey, TimestampOverflowError, effective_time_unix_nano, hour_partition_in_window,
    percent_decode_tenant, percent_encode_tenant,
};
pub use promoted::{PromotedAttributes, SERVICE_NAME_KEY};
pub use reader::{Reader, ReaderError, ShapeValidation, batch_to_mined_records};
pub use record_batch::{BatchError, mined_records_to_batch, mined_records_to_batch_with_promoted};
pub use store::{S3Config, Store, StoreConfig, StoreError};
pub use writer::{
    COMPACTED_RG_BYTES_ENV, COMPACTED_ROW_GROUP_FLUSH_BYTES, DEFAULT_ZSTD_LEVEL,
    ROW_GROUP_FLUSH_BYTES, SUB_BATCH_ROWS, Writer, WriterError, WrittenFile,
    encode_records_to_parquet, encode_records_to_parquet_with_promoted,
};

use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema as ArrowSchema, SchemaRef, TimeUnit};

/// Data-file column-name constants (RFC 0005 §3.2). Production
/// code addressing columns by name MUST use these so renames stay
/// greppable and the RFC0005.10 schema-pin test catches drift.
pub mod columns {
    pub const TENANT_ID: &str = "tenant_id";
    pub const TEMPLATE_ID: &str = "template_id";
    pub const TEMPLATE_VERSION: &str = "template_version";
    pub const TIME_UNIX_NANO: &str = "time_unix_nano";
    pub const OBSERVED_TIME_UNIX_NANO: &str = "observed_time_unix_nano";
    pub const EFFECTIVE_TIME_UNIX_NANO: &str = "effective_time_unix_nano";
    pub const SEVERITY_NUMBER: &str = "severity_number";
    pub const SEVERITY_TEXT: &str = "severity_text";
    pub const SCOPE_NAME: &str = "scope_name";
    pub const SCOPE_VERSION: &str = "scope_version";
    pub const ATTRIBUTES: &str = "attributes";
    pub const DROPPED_ATTRIBUTES_COUNT: &str = "dropped_attributes_count";
    pub const RESOURCE_ATTRIBUTES: &str = "resource_attributes";
    pub const TRACE_ID: &str = "trace_id";
    pub const SPAN_ID: &str = "span_id";
    pub const FLAGS: &str = "flags";
    pub const EVENT_NAME: &str = "event_name";
    pub const BODY_KIND: &str = "body_kind";
    pub const BODY: &str = "body";
    pub const PARAMS: &str = "params";
    pub const SEPARATORS: &str = "separators";
    pub const CONFIDENCE: &str = "confidence";
    pub const LOSSY_FLAG: &str = "lossy_flag";
    /// RFC 0018 §3.1 — `InstrumentationScope.attributes`, canonical JSON
    /// (OPTIONAL for the §3.5 migration; `[]` when empty, NULL only in
    /// pre-amendment files).
    pub const SCOPE_ATTRIBUTES: &str = "scope_attributes";
    /// RFC 0018 §3.1 — `ResourceLogs.schema_url` (OPTIONAL).
    pub const RESOURCE_SCHEMA_URL: &str = "resource_schema_url";
    /// RFC 0018 §3.1 — `ScopeLogs.schema_url` (OPTIONAL).
    pub const SCOPE_SCHEMA_URL: &str = "scope_schema_url";
}

/// Audit-event file column-name constants (RFC 0005 §3.7).
pub mod audit_columns {
    pub const TENANT_ID: &str = "tenant_id";
    pub const TIMESTAMP: &str = "timestamp";
    pub const EVENT_KIND: &str = "event_kind";
    pub const EVENT_TYPE: &str = "event_type";
    pub const TEMPLATE_ID: &str = "template_id";
    pub const OLD_VERSION: &str = "old_version";
    pub const NEW_VERSION: &str = "new_version";
    pub const OLD_TEMPLATE: &str = "old_template";
    pub const NEW_TEMPLATE: &str = "new_template";
    pub const POSITIONS_WIDENED: &str = "positions_widened";
    pub const SLOTS_EXPANDED: &str = "slots_expanded";
    pub const TRIGGERING_LINE_HASH: &str = "triggering_line_hash";
    pub const TRIGGERING_LINE_SAMPLE: &str = "triggering_line_sample";
    pub const REASON: &str = "reason";
    // Compaction-event columns (RFC 0005 §3.7 amendment 2026-06-03 /
    // RFC 0009 §3.6); NULL for the template event kinds.
    pub const COMPACTION_PARTITION: &str = "compaction_partition";
    pub const COMPACTION_INPUT_FILES: &str = "compaction_input_files";
    pub const COMPACTION_OUTPUT_FILE: &str = "compaction_output_file";
    pub const COMPACTION_GENERATION: &str = "compaction_generation";
    pub const COMPACTION_ROWS: &str = "compaction_rows";
    // Alias-event columns (RFC 0005 §3.7 amendment 2026-06-12 /
    // RFC 0001 §6.7); NULL for all other kinds.
    pub const ALIAS_REPRESENTATIVE_ID: &str = "alias_representative_id";
    pub const ALIAS_MEMBER_IDS: &str = "alias_member_ids";
    pub const ALIAS_ACTOR: &str = "alias_actor";
    // Quarantine-event columns (RFC 0025 §3.3); NULL for all other
    // kinds. Appended after the alias group — additive per §3.7.
    pub const QUARANTINE_PARTITION: &str = "quarantine_partition";
    pub const QUARANTINE_ERROR: &str = "quarantine_error";
    /// The rejecting token's audit label on an `ingest_denied` event
    /// (RFC 0026 §3.4) — never the token value.
    pub const DENIED_TOKEN_NAME: &str = "denied_token_name";
}

/// Build the data-file Arrow schema per RFC 0005 §3.2.
///
/// Column order matches the RFC's §3.2 declaration order; readers
/// MUST address columns by name (the §3.2 normative rule), but the
/// declared order is what shows up in `cargo doc` and what the
/// schema-pin test asserts so writer / reader implementations stay
/// in lockstep with the RFC.
#[must_use]
pub fn data_schema() -> SchemaRef {
    let utc: Arc<str> = "UTC".into();
    let params_element = Field::new(
        "element",
        DataType::Struct(
            vec![
                Field::new("type_tag", DataType::Int32, false),
                Field::new("value", DataType::Binary, true),
            ]
            .into(),
        ),
        false,
    );
    let separators_element = Field::new("element", DataType::Binary, false);

    Arc::new(ArrowSchema::new(vec![
        Field::new(columns::TENANT_ID, DataType::Utf8, false),
        Field::new(columns::TEMPLATE_ID, DataType::UInt64, false),
        Field::new(columns::TEMPLATE_VERSION, DataType::UInt32, false),
        Field::new(
            columns::TIME_UNIX_NANO,
            DataType::Timestamp(TimeUnit::Nanosecond, Some(utc.clone())),
            false,
        ),
        Field::new(
            columns::OBSERVED_TIME_UNIX_NANO,
            DataType::Timestamp(TimeUnit::Nanosecond, Some(utc.clone())),
            true,
        ),
        // OPTIONAL per §3.8 rule 1 (additive amendment 2026-06-11);
        // the writer always populates it, NULL appears only in
        // pre-amendment files (the §3.9 rule-2 read default is the
        // row's `time_unix_nano`, not `None`).
        Field::new(
            columns::EFFECTIVE_TIME_UNIX_NANO,
            DataType::Timestamp(TimeUnit::Nanosecond, Some(utc)),
            true,
        ),
        Field::new(columns::SEVERITY_NUMBER, DataType::UInt8, false),
        Field::new(columns::SEVERITY_TEXT, DataType::Utf8, true),
        Field::new(columns::SCOPE_NAME, DataType::Utf8, true),
        Field::new(columns::SCOPE_VERSION, DataType::Utf8, true),
        Field::new(columns::ATTRIBUTES, DataType::Utf8, false),
        Field::new(columns::DROPPED_ATTRIBUTES_COUNT, DataType::UInt32, false),
        Field::new(columns::RESOURCE_ATTRIBUTES, DataType::Utf8, false),
        Field::new(columns::TRACE_ID, DataType::FixedSizeBinary(16), true),
        Field::new(columns::SPAN_ID, DataType::FixedSizeBinary(8), true),
        Field::new(columns::FLAGS, DataType::UInt32, false),
        Field::new(columns::EVENT_NAME, DataType::Utf8, true),
        Field::new(columns::BODY_KIND, DataType::UInt8, false),
        Field::new(columns::BODY, DataType::Binary, true),
        Field::new(
            columns::PARAMS,
            DataType::List(Arc::new(params_element)),
            false,
        ),
        Field::new(
            columns::SEPARATORS,
            DataType::List(Arc::new(separators_element)),
            false,
        ),
        Field::new(columns::CONFIDENCE, DataType::Float32, false),
        Field::new(columns::LOSSY_FLAG, DataType::Boolean, false),
        // RFC 0018 §3.1 — additive OPTIONAL columns (§3.5 migration: readers
        // tolerate their absence in pre-amendment files).
        Field::new(columns::SCOPE_ATTRIBUTES, DataType::Utf8, true),
        Field::new(columns::RESOURCE_SCHEMA_URL, DataType::Utf8, true),
        Field::new(columns::SCOPE_SCHEMA_URL, DataType::Utf8, true),
    ]))
}

/// [`data_schema`] plus the RFC 0022 promoted attribute columns for
/// `promoted` — additive `OPTIONAL` Utf8 fields appended after the
/// §3.2 base columns (`resource.<key>` columns first,
/// `resource.service.name` always leading, then `attr.<key>`). This
/// is the writer's declared schema; [`data_schema`] stays the base
/// shape readers address by name (§3.9 tolerates both absent and
/// unknown columns, so files written under any promoted set coexist).
#[must_use]
pub fn data_schema_with_promoted(promoted: &PromotedAttributes) -> SchemaRef {
    let base = data_schema();
    let mut fields: Vec<Field> = base.fields().iter().map(|f| f.as_ref().clone()).collect();
    fields.extend(promoted.fields());
    Arc::new(ArrowSchema::new(fields))
}

/// Build the audit-event file Arrow schema per RFC 0005 §3.7.
///
/// Both `event_kind` (`UInt8` Arrow — Parquet stores it physically
/// as `INT32` since Parquet has no narrower integer physical type,
/// with the §3.7 logical type `INTEGER(8, signed=false)`) and
/// `event_type` (`Utf8` Arrow → `STRING` Parquet) are persisted
/// per §3.7's dual-column rule. The ordinal is the compact
/// internal representation; the string is the predicate-pushdown
/// surface RFC 0001 §9 requires for the §6.7 drift query.
#[must_use]
pub fn audit_schema() -> SchemaRef {
    let utc: Arc<str> = "UTC".into();
    let positions_element = Field::new("element", DataType::Int32, false);
    let types_added_element = Field::new("element", DataType::Int32, false);
    let slot_expansion_struct = DataType::Struct(
        vec![
            Field::new("slot_index", DataType::Int32, false),
            Field::new(
                "types_added",
                DataType::List(Arc::new(types_added_element)),
                false,
            ),
        ]
        .into(),
    );
    let slots_expanded_element = Field::new("element", slot_expansion_struct, false);

    Arc::new(ArrowSchema::new(vec![
        Field::new(audit_columns::TENANT_ID, DataType::Utf8, false),
        Field::new(
            audit_columns::TIMESTAMP,
            DataType::Timestamp(TimeUnit::Nanosecond, Some(utc)),
            false,
        ),
        Field::new(audit_columns::EVENT_KIND, DataType::UInt8, false),
        Field::new(audit_columns::EVENT_TYPE, DataType::Utf8, false),
        // Template-specific columns: OPTIONAL per the §3.7 amendment
        // (2026-06-03) — required-by-convention for the template event
        // kinds (0–2), NULL for `compaction` (kind 3).
        Field::new(audit_columns::TEMPLATE_ID, DataType::UInt64, true),
        Field::new(audit_columns::OLD_VERSION, DataType::UInt32, true),
        Field::new(audit_columns::NEW_VERSION, DataType::UInt32, true),
        Field::new(audit_columns::OLD_TEMPLATE, DataType::Utf8, true),
        Field::new(audit_columns::NEW_TEMPLATE, DataType::Utf8, true),
        Field::new(
            audit_columns::POSITIONS_WIDENED,
            DataType::List(Arc::new(positions_element)),
            true,
        ),
        Field::new(
            audit_columns::SLOTS_EXPANDED,
            DataType::List(Arc::new(slots_expanded_element)),
            true,
        ),
        Field::new(
            audit_columns::TRIGGERING_LINE_HASH,
            DataType::FixedSizeBinary(16),
            true,
        ),
        Field::new(audit_columns::TRIGGERING_LINE_SAMPLE, DataType::Utf8, true),
        Field::new(audit_columns::REASON, DataType::Utf8, true),
        // Compaction-event columns (RFC 0009 §3.6): OPTIONAL, NULL for
        // the template event kinds.
        Field::new(audit_columns::COMPACTION_PARTITION, DataType::Utf8, true),
        Field::new(
            audit_columns::COMPACTION_INPUT_FILES,
            DataType::List(Arc::new(Field::new("element", DataType::Utf8, false))),
            true,
        ),
        Field::new(audit_columns::COMPACTION_OUTPUT_FILE, DataType::Utf8, true),
        Field::new(audit_columns::COMPACTION_GENERATION, DataType::UInt64, true),
        Field::new(audit_columns::COMPACTION_ROWS, DataType::UInt64, true),
        // Alias-event columns (RFC 0001 §6.7 / §3.7 amendment
        // 2026-06-12): OPTIONAL, NULL for all other kinds;
        // required-by-convention non-null for kinds 4–5 (the
        // member list possibly empty — an empty list is valid and
        // distinct from NULL).
        Field::new(
            audit_columns::ALIAS_REPRESENTATIVE_ID,
            DataType::UInt64,
            true,
        ),
        Field::new(
            audit_columns::ALIAS_MEMBER_IDS,
            DataType::List(Arc::new(Field::new("element", DataType::UInt64, false))),
            true,
        ),
        Field::new(audit_columns::ALIAS_ACTOR, DataType::Utf8, true),
        // Rejection-event columns, appended after the alias group
        // (§3.7 additive): the quarantine pair (RFC 0025 §3.3,
        // required-by-convention non-null for kind 7) and the denial
        // token label (RFC 0026 §3.4, non-null for kind 8). OPTIONAL,
        // NULL for all other kinds.
        Field::new(audit_columns::QUARANTINE_PARTITION, DataType::Utf8, true),
        Field::new(audit_columns::QUARANTINE_ERROR, DataType::Utf8, true),
        Field::new(audit_columns::DENIED_TOKEN_NAME, DataType::Utf8, true),
    ]))
}
