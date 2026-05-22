//! Convert a slice of [`AuditEvent`]s into an Arrow `RecordBatch`
//! matching [`crate::audit_schema()`].
//!
//! Column order mirrors RFC 0005 §3.7 exactly. The [`crate::tests::
//! schema_pin`] test (RFC0005.10) catches drift between this
//! builder's array shape and the declared schema; the audit
//! round-trip test (RFC0005.7) pins the per-variant column
//! semantics from the §3.7 mapping table.
//!
//! **Per-variant column population** (per RFC 0005 §3.7's normative
//! mapping table):
//!
//! | Variant | `event_kind` | `positions_widened` | `slots_expanded` | `old_template` / `new_template` | `reason` |
//! |---|---|---|---|---|---|
//! | `TemplateWidened` | `0` | event's positions | `[]` | event's pre / post | `NULL` |
//! | `TemplateTypeExpanded` | `1` | `[]` | event's slots | both = pre = post | `NULL` |
//! | `TemplateWideningRejectedDegenerate` | `2` | `[]` | `[]` | both = `current_template` | JSON of `would_be_*` |
//!
//! **Rejection-variant `reason` payload.** The in-memory
//! [`AuditEventKind::TemplateWideningRejectedDegenerate`] carries
//! `would_be_template: String` and `would_be_positions: Vec<u16>`,
//! but the §3.7 column table has no dedicated columns for them.
//! Per §3.7, the `reason` column is "the degenerate-template
//! guard's diagnostic string"; we encode the structured diagnostic
//! as a JSON object so the round-trip preserves the in-memory
//! shape without amending the schema:
//!
//! ```json
//! { "would_be_template": "<...>", "would_be_positions": [1, 2, 3] }
//! ```
//!
//! Producers other than this writer that want to put a free-form
//! string in `reason` (e.g. a foreign writer recording a different
//! flavour of rejection) will not parse with the round-trip
//! decoder; that case falls back to surfacing an empty
//! `would_be_template` / `would_be_positions` on the reader side
//! rather than erroring, since the audit *event itself* is still
//! valid.

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use arrow_array::builder::{
    FixedSizeBinaryBuilder, GenericListBuilder, Int32Builder, StringBuilder, StructBuilder,
    TimestampNanosecondBuilder, UInt8Builder, UInt32Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{ArrowError, DataType, Field};
use ourios_core::audit::{AuditEvent, AuditEventKind, ParamType, SlotExpansion};

use crate::audit_schema;

/// Stable on-disk ordinals for [`AuditEventKind`] per RFC 0005
/// §3.7's normative mapping table.
pub const EVENT_KIND_TEMPLATE_WIDENED: u8 = 0;
pub const EVENT_KIND_TEMPLATE_TYPE_EXPANDED: u8 = 1;
pub const EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE: u8 = 2;

/// Canonical `event_type` strings per RFC 0005 §3.7's normative
/// mapping table. Both columns are persisted (the ordinal is the
/// compact internal handle, the string is the predicate-pushdown
/// surface RFC 0001 §9 names).
pub const EVENT_TYPE_TEMPLATE_WIDENED: &str = "template_widened";
pub const EVENT_TYPE_TEMPLATE_TYPE_EXPANDED: &str = "template_type_expanded";
pub const EVENT_TYPE_TEMPLATE_WIDENING_REJECTED_DEGENERATE: &str =
    "template_widening_rejected_degenerate";

/// Build an Arrow `RecordBatch` matching [`audit_schema`] from a
/// slice of [`AuditEvent`]s.
///
/// # Errors
///
/// - [`AuditBatchError::PreEpochTimestamp`] if an event's
///   `timestamp` is earlier than 1970 (`SystemTime::duration_since(
///   UNIX_EPOCH)` returns `Err`).
/// - [`AuditBatchError::TimestampOverflow`] if an event's
///   `timestamp` nanos exceed `i64::MAX` — same RFC 0005 §3.2 /
///   §3.7 `INT64` overflow contract the data writer enforces.
/// - [`AuditBatchError::Arrow`] when Arrow itself rejects the
///   constructed batch (internal-bug signal; the builders are
///   constructed against `audit_schema()` directly).
pub fn audit_events_to_batch(events: &[AuditEvent]) -> Result<RecordBatch, AuditBatchError> {
    let mut b = Builders::with_capacity(events.len());
    for e in events {
        b.append(e)?;
    }
    let arrays = b.finish();
    RecordBatch::try_new(audit_schema(), arrays).map_err(AuditBatchError::Arrow)
}

/// Errors produced by [`audit_events_to_batch`].
#[derive(Debug)]
pub enum AuditBatchError {
    /// An event's `timestamp` is earlier than the Unix epoch. The
    /// §3.7 `timestamp` column is `TIMESTAMP(NANOS, UTC)` backed
    /// by `INT64` — negative nanos-since-epoch would be the wire
    /// shape, but no real audit event is emitted before 1970, so
    /// we reject rather than silently encode a confusing negative.
    PreEpochTimestamp,
    /// An event's `timestamp` nanos-since-epoch exceed `i64::MAX`.
    /// Carries the offending value for diagnostics.
    TimestampOverflow { nanos: u128 },
    /// A `TemplateTypeExpanded` (or
    /// `TemplateWideningRejectedDegenerate`) event was supplied
    /// with `old_template != new_template`. Per RFC 0005 §3.7,
    /// `TemplateTypeExpanded` and `TemplateWideningRejectedDegenerate`
    /// carry the unchanged template (equal to `old_template`); a
    /// divergent pair would persist a row that violates the §3.7
    /// invariant, so the writer rejects rather than emit corrupted
    /// audit data. Reach this and the upstream producer has a bug.
    TemplateMustNotChange {
        variant: &'static str,
        old_template: String,
        new_template: String,
    },
    /// Arrow rejected the constructed `RecordBatch` (column-length
    /// mismatch, schema-shape mismatch). Internal bug if it ever
    /// fires — the array builders are constructed against
    /// `audit_schema()` directly.
    Arrow(ArrowError),
}

impl fmt::Display for AuditBatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PreEpochTimestamp => write!(
                f,
                "audit event timestamp is earlier than the Unix epoch (RFC 0005 §3.7's \
                 timestamp column is TIMESTAMP(NANOS, UTC); pre-epoch audit events are \
                 not representable)",
            ),
            Self::TimestampOverflow { nanos } => write!(
                f,
                "audit event timestamp = {nanos} ns exceeds i64::MAX (RFC 0005 §3.7's \
                 INT64-backed timestamp overflow contract)",
            ),
            Self::TemplateMustNotChange {
                variant,
                old_template,
                new_template,
            } => write!(
                f,
                "audit event {variant} has old_template = {old_template:?} != new_template \
                 = {new_template:?}, but RFC 0005 §3.7 requires they be equal for this \
                 variant (template tokens don't change)",
            ),
            Self::Arrow(e) => write!(f, "arrow rejected RecordBatch: {e}"),
        }
    }
}

impl std::error::Error for AuditBatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PreEpochTimestamp
            | Self::TimestampOverflow { .. }
            | Self::TemplateMustNotChange { .. } => None,
            Self::Arrow(e) => Some(e),
        }
    }
}

/// Stable on-disk ordinal for [`ParamType`] used inside the
/// `slots_expanded.types_added` list. Identical to the data-file
/// `params.type_tag` ordinal in `record_batch::param_type_ordinal`
/// — kept duplicated rather than re-exported because re-exporting
/// from a private module would force `record_batch`'s helper onto
/// the public surface; the two ordinals are pinned together by
/// the matching match arms (a future variant addition forces
/// both call sites to update).
const fn param_type_ordinal(t: ParamType) -> i32 {
    match t {
        ParamType::Ip => 0,
        ParamType::Uuid => 1,
        ParamType::Num => 2,
        ParamType::Hex => 3,
        ParamType::Ts => 4,
        ParamType::Path => 5,
        ParamType::Str => 6,
        ParamType::Overflow => 7,
        ParamType::Unknown(ord) => ord,
    }
}

/// One builder per column in `audit_schema()` order.
struct Builders {
    tenant_id: StringBuilder,
    timestamp: TimestampNanosecondBuilder,
    event_kind: UInt8Builder,
    event_type: StringBuilder,
    template_id: UInt64Builder,
    old_version: UInt32Builder,
    new_version: UInt32Builder,
    old_template: StringBuilder,
    new_template: StringBuilder,
    positions_widened: GenericListBuilder<i32, Int32Builder>,
    slots_expanded: GenericListBuilder<i32, StructBuilder>,
    triggering_line_hash: FixedSizeBinaryBuilder,
    triggering_line_sample: StringBuilder,
    reason: StringBuilder,
}

impl Builders {
    fn with_capacity(cap: usize) -> Self {
        let positions_element = Field::new("element", DataType::Int32, false);
        let types_added_element = Field::new("element", DataType::Int32, false);
        let slot_struct_fields = vec![
            Field::new("slot_index", DataType::Int32, false),
            Field::new(
                "types_added",
                DataType::List(Arc::new(types_added_element.clone())),
                false,
            ),
        ];
        let slot_struct_dt: DataType = DataType::Struct(slot_struct_fields.clone().into());
        let slots_element = Field::new("element", slot_struct_dt, false);

        // `slots_expanded` list element is a struct of
        // (slot_index, types_added: list<int32>). Build the
        // StructBuilder by hand so the inner list's element-field
        // metadata (name + non-nullable) matches `audit_schema()`.
        let types_added_list_builder: GenericListBuilder<i32, Int32Builder> =
            GenericListBuilder::new(Int32Builder::new()).with_field(types_added_element);
        let slot_struct_builder = StructBuilder::new(
            slot_struct_fields,
            vec![
                Box::new(Int32Builder::new()),
                Box::new(types_added_list_builder),
            ],
        );

        Self {
            tenant_id: StringBuilder::with_capacity(cap, 0),
            timestamp: TimestampNanosecondBuilder::with_capacity(cap).with_timezone("UTC"),
            event_kind: UInt8Builder::with_capacity(cap),
            event_type: StringBuilder::with_capacity(cap, 0),
            template_id: UInt64Builder::with_capacity(cap),
            old_version: UInt32Builder::with_capacity(cap),
            new_version: UInt32Builder::with_capacity(cap),
            old_template: StringBuilder::with_capacity(cap, 0),
            new_template: StringBuilder::with_capacity(cap, 0),
            positions_widened: GenericListBuilder::new(Int32Builder::new())
                .with_field(positions_element),
            slots_expanded: GenericListBuilder::new(slot_struct_builder).with_field(slots_element),
            triggering_line_hash: FixedSizeBinaryBuilder::with_capacity(cap, 16),
            triggering_line_sample: StringBuilder::with_capacity(cap, 0),
            reason: StringBuilder::with_capacity(cap, 0),
        }
    }

    fn append(&mut self, e: &AuditEvent) -> Result<(), AuditBatchError> {
        self.tenant_id.append_value(e.tenant_id.as_str());
        self.timestamp
            .append_value(system_time_to_i64_nanos(e.timestamp)?);
        self.template_id.append_value(e.template_id);
        self.triggering_line_hash
            .append_value(e.triggering_line_hash)
            .map_err(AuditBatchError::Arrow)?;
        match e.triggering_line_sample.as_deref() {
            Some(s) => self.triggering_line_sample.append_value(s),
            None => self.triggering_line_sample.append_null(),
        }

        match &e.kind {
            AuditEventKind::TemplateWidened {
                old_version,
                new_version,
                old_template,
                new_template,
                positions_widened,
            } => {
                self.event_kind.append_value(EVENT_KIND_TEMPLATE_WIDENED);
                self.event_type.append_value(EVENT_TYPE_TEMPLATE_WIDENED);
                self.old_version.append_value(*old_version);
                self.new_version.append_value(*new_version);
                self.old_template.append_value(old_template);
                self.new_template.append_value(new_template);
                append_positions(&mut self.positions_widened, positions_widened);
                append_slots(&mut self.slots_expanded, &[]);
                self.reason.append_null();
            }
            AuditEventKind::TemplateTypeExpanded {
                old_version,
                new_version,
                old_template,
                new_template,
                slots_expanded,
            } => {
                // §3.7 invariant: TemplateTypeExpanded carries the
                // unchanged template, so `old_template ==
                // new_template`. The in-memory variant has both
                // fields independently (the miner builds them
                // separately), so we enforce the invariant at the
                // serialisation boundary rather than trusting the
                // producer. Persisting divergent strings would be a
                // §3.7 contract violation a future reader couldn't
                // disambiguate.
                if old_template != new_template {
                    return Err(AuditBatchError::TemplateMustNotChange {
                        variant: "TemplateTypeExpanded",
                        old_template: old_template.clone(),
                        new_template: new_template.clone(),
                    });
                }
                self.event_kind
                    .append_value(EVENT_KIND_TEMPLATE_TYPE_EXPANDED);
                self.event_type
                    .append_value(EVENT_TYPE_TEMPLATE_TYPE_EXPANDED);
                self.old_version.append_value(*old_version);
                self.new_version.append_value(*new_version);
                self.old_template.append_value(old_template);
                self.new_template.append_value(new_template);
                append_positions(&mut self.positions_widened, &[]);
                append_slots(&mut self.slots_expanded, slots_expanded);
                self.reason.append_null();
            }
            AuditEventKind::TemplateWideningRejectedDegenerate {
                version,
                current_template,
                would_be_template,
                would_be_positions,
            } => {
                self.event_kind
                    .append_value(EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE);
                self.event_type
                    .append_value(EVENT_TYPE_TEMPLATE_WIDENING_REJECTED_DEGENERATE);
                // §3.7 column commentary: rejection rows carry the
                // unchanged template in both old / new (templates
                // don't change when the widening is rejected); the
                // version pair collapses to the single `version`
                // the in-memory variant carries.
                self.old_version.append_value(*version);
                self.new_version.append_value(*version);
                self.old_template.append_value(current_template);
                self.new_template.append_value(current_template);
                append_positions(&mut self.positions_widened, &[]);
                append_slots(&mut self.slots_expanded, &[]);
                self.reason.append_value(encode_rejection_reason(
                    would_be_template,
                    would_be_positions,
                ));
            }
        }

        Ok(())
    }

    fn finish(mut self) -> Vec<ArrayRef> {
        vec![
            Arc::new(self.tenant_id.finish()),
            Arc::new(self.timestamp.finish()),
            Arc::new(self.event_kind.finish()),
            Arc::new(self.event_type.finish()),
            Arc::new(self.template_id.finish()),
            Arc::new(self.old_version.finish()),
            Arc::new(self.new_version.finish()),
            Arc::new(self.old_template.finish()),
            Arc::new(self.new_template.finish()),
            Arc::new(self.positions_widened.finish()),
            Arc::new(self.slots_expanded.finish()),
            Arc::new(self.triggering_line_hash.finish()),
            Arc::new(self.triggering_line_sample.finish()),
            Arc::new(self.reason.finish()),
        ]
    }
}

fn append_positions(builder: &mut GenericListBuilder<i32, Int32Builder>, positions: &[u16]) {
    for p in positions {
        builder.values().append_value(i32::from(*p));
    }
    builder.append(true);
}

fn append_slots(builder: &mut GenericListBuilder<i32, StructBuilder>, slots: &[SlotExpansion]) {
    for s in slots {
        let struct_b = builder.values();
        struct_b
            .field_builder::<Int32Builder>(0)
            .expect("slot_index field index 0")
            .append_value(i32::from(s.slot_index));
        let types_list = struct_b
            .field_builder::<GenericListBuilder<i32, Int32Builder>>(1)
            .expect("types_added field index 1");
        for t in &s.added_types {
            types_list.values().append_value(param_type_ordinal(*t));
        }
        types_list.append(true);
        struct_b.append(true);
    }
    builder.append(true);
}

/// Encode the rejection variant's `would_be_template` /
/// `would_be_positions` pair as a JSON object for the `reason`
/// column. See the module-level note on the §3.7 "diagnostic
/// string" framing.
///
/// # Panics
///
/// Structurally impossible. The inner [`ReasonPayload`] is two
/// owned-or-borrowed scalars; `serde_json::to_string` only fails
/// when the `Serialize` impl produces an error (e.g. an
/// invalid-UTF-8 map key) which neither field can ever do.
#[must_use]
pub fn encode_rejection_reason(would_be_template: &str, would_be_positions: &[u16]) -> String {
    // Serde's derive-based serializer streams field-by-field
    // straight into the output buffer — no intermediate
    // `serde_json::Value` tree, no per-element boxing. The
    // resulting JSON shape (key order, types) matches the
    // matching `decode_rejection_reason` parser in
    // `audit_reader.rs`.
    #[derive(serde::Serialize)]
    struct ReasonPayload<'a> {
        would_be_template: &'a str,
        would_be_positions: &'a [u16],
    }
    serde_json::to_string(&ReasonPayload {
        would_be_template,
        would_be_positions,
    })
    .expect("ReasonPayload is always serialisable")
}

fn system_time_to_i64_nanos(t: SystemTime) -> Result<i64, AuditBatchError> {
    let d = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| AuditBatchError::PreEpochTimestamp)?;
    let nanos = d.as_nanos();
    i64::try_from(nanos).map_err(|_| AuditBatchError::TimestampOverflow { nanos })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ourios_core::audit::{AuditEventKind, hash_triggering_line};
    use ourios_core::tenant::TenantId;
    use std::time::{Duration, UNIX_EPOCH};

    fn ts(offset_secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(offset_secs)
    }

    fn widened_event() -> AuditEvent {
        AuditEvent {
            kind: AuditEventKind::TemplateWidened {
                old_version: 1,
                new_version: 2,
                old_template: "[\"user\",\"<*>\",\"in\"]".to_string(),
                new_template: "[\"user\",\"<*>\",\"<*>\"]".to_string(),
                positions_widened: vec![2],
            },
            tenant_id: TenantId::new("acme"),
            template_id: 7,
            triggering_line_hash: hash_triggering_line(b"trigger"),
            triggering_line_sample: Some("user 42 in".to_string()),
            timestamp: ts(1_775_127_480),
        }
    }

    fn type_expanded_event() -> AuditEvent {
        AuditEvent {
            kind: AuditEventKind::TemplateTypeExpanded {
                old_version: 2,
                new_version: 3,
                old_template: "[\"user\",\"<*>\"]".to_string(),
                new_template: "[\"user\",\"<*>\"]".to_string(),
                slots_expanded: vec![SlotExpansion {
                    slot_index: 1,
                    added_types: vec![ParamType::Num, ParamType::Ip],
                }],
            },
            tenant_id: TenantId::new("acme"),
            template_id: 7,
            triggering_line_hash: hash_triggering_line(b"trigger-2"),
            triggering_line_sample: None,
            timestamp: ts(1_775_127_490),
        }
    }

    fn rejection_event() -> AuditEvent {
        AuditEvent {
            kind: AuditEventKind::TemplateWideningRejectedDegenerate {
                version: 5,
                current_template: "[\"lit\",\"<*>\"]".to_string(),
                would_be_template: "[\"<*>\",\"<*>\"]".to_string(),
                would_be_positions: vec![0, 1],
            },
            tenant_id: TenantId::new("acme"),
            template_id: 9,
            triggering_line_hash: hash_triggering_line(b"degenerate"),
            triggering_line_sample: Some("zzz qqq".to_string()),
            timestamp: ts(1_775_127_500),
        }
    }

    #[test]
    fn builds_batch_for_one_of_each_variant() {
        let batch =
            audit_events_to_batch(&[widened_event(), type_expanded_event(), rejection_event()])
                .expect("batch builds");
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.schema(), audit_schema());
    }

    #[test]
    fn rejection_reason_is_json_with_would_be_fields() {
        let r = rejection_event();
        let AuditEventKind::TemplateWideningRejectedDegenerate {
            would_be_template,
            would_be_positions,
            ..
        } = &r.kind
        else {
            unreachable!();
        };
        let s = encode_rejection_reason(would_be_template, would_be_positions);
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
        assert_eq!(v["would_be_template"], "[\"<*>\",\"<*>\"]");
        assert_eq!(v["would_be_positions"], serde_json::json!([0, 1]));
    }

    #[test]
    fn pre_epoch_timestamp_rejected() {
        // SystemTime arithmetic clamps before UNIX_EPOCH; build a
        // pre-epoch time by subtracting from the epoch.
        let pre = SystemTime::UNIX_EPOCH
            .checked_sub(Duration::from_secs(1))
            .expect("epoch minus one second");
        let mut e = widened_event();
        e.timestamp = pre;
        let err = audit_events_to_batch(std::slice::from_ref(&e))
            .expect_err("pre-epoch timestamp must error");
        assert!(matches!(err, AuditBatchError::PreEpochTimestamp));
    }
}
