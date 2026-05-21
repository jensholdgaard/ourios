//! Convert a slice of `MinedRecord`s into an Arrow `RecordBatch`
//! matching [`crate::data_schema()`].
//!
//! Column order mirrors the RFC 0005 §3.2 schema declaration
//! exactly. The [`crate::tests`](crate) schema-pin test
//! (RFC0005.10) catches drift between the Rust array shape and
//! the declared schema.
//!
//! **`AnyValue` → canonical JSON.** RFC 0005 §3.3 mandates
//! OTLP-canonical JSON for the `attributes`, `resource_attributes`,
//! and (when `body_kind = Structured`) `body` columns. The
//! current builder handles **only the empty case** for the
//! `KeyValue` lists — it emits the literal `"[]"` directly into
//! the column (the RFC 0005 §3.2 `Vec::new()` ↔ `[]` rule) — and
//! returns [`BatchError::AttributesNotYetEncoded`] on any
//! non-empty input. Corpus / bench inputs today carry empty
//! attributes; the RFC 0003 receiver is what populates them, and
//! the canonicalisation PR named in the PR-E1 breadcrumb on
//! [`ourios_core::otlp::Body::Structured`] is the one that fills
//! in the proto3-JSON-with-OTLP-overrides encoder. Surfacing a
//! structured error rather than panicking (or emitting non-JSON
//! `Debug` bytes masquerading as JSON) lets the writer fail a
//! batch gracefully without crashing the ingest process.

use std::fmt;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float32Builder, GenericListBuilder,
    Int32Builder, StringBuilder, StructBuilder, TimestampNanosecondBuilder, UInt8Builder,
    UInt32Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{ArrowError, DataType, Field};
use ourios_core::audit::ParamType;
use ourios_core::otlp::KeyValue;
use ourios_core::record::{BodyKind, MinedRecord};

use crate::partition::TimestampOverflowError;
use crate::{columns, data_schema};

/// Build an Arrow `RecordBatch` matching `data_schema()` from a
/// slice of [`MinedRecord`]s.
///
/// # Errors
///
/// - [`BatchError::TimestampOverflow`] when a record's
///   `time_unix_nano` or `observed_time_unix_nano` exceeds
///   `i64::MAX` (RFC 0005 §3.2's `u64`→`i64` overflow contract).
/// - [`BatchError::Arrow`] when Arrow itself rejects the
///   constructed batch (column-length mismatch or similar — an
///   internal-bug-not-user-input signal, since the builders are
///   constructed against `data_schema()` directly).
pub fn mined_records_to_batch(records: &[MinedRecord]) -> Result<RecordBatch, BatchError> {
    let mut b = Builders::with_capacity(records.len());
    for r in records {
        b.append(r)?;
    }
    let arrays = b.finish();
    RecordBatch::try_new(data_schema(), arrays).map_err(BatchError::Arrow)
}

/// Errors produced by [`mined_records_to_batch`].
#[derive(Debug)]
pub enum BatchError {
    /// A record's nanosecond timestamp exceeded `i64::MAX`
    /// (RFC 0005 §3.2 overflow contract). Carries which field
    /// overflowed (`time_unix_nano` or
    /// `observed_time_unix_nano`) and the offending value.
    TimestampOverflow { field: &'static str, value: u64 },
    /// A record carried a non-empty `attributes` or
    /// `resource_attributes` `Vec<KeyValue>`. The canonical-JSON
    /// encoder is deferred to the RFC 0005 §3.3 canonicalisation
    /// PR (see the PR-E1 breadcrumb on
    /// [`ourios_core::otlp::Body::Structured`]); until then the
    /// writer returns this error rather than crashing the ingest
    /// process. Carries the column name and entry count.
    AttributesNotYetEncoded { column: &'static str, count: usize },
    /// A record carried [`BodyKind::Absent`] (the in-memory
    /// "wire delivered no body" variant). RFC 0005 §3.2's
    /// `body_kind` column pins exactly two ordinals (`0 = String,
    /// 1 = Structured`); silently mapping `Absent` to one of
    /// them would misclassify wire-absent rows. Until a future
    /// RFC 0005 amendment either adds a third ordinal or adds a
    /// separate `body_present` boolean column, the writer
    /// rejects these records rather than corrupting the
    /// `body_kind` semantics.
    UnsupportedAbsentBody,
    /// A record carried `body_kind = Structured`. RFC 0005 §3.3
    /// requires the `body` column for these rows to hold
    /// OTLP-canonical JSON, but the miner today populates
    /// `MinedRecord.body` with an interim `Debug` rendering
    /// (see the PR-E1 breadcrumb on
    /// [`ourios_core::otlp::Body::Structured`]). Writing the
    /// interim bytes would silently store non-canonical /
    /// non-JSON content into a §3.3-governed column. Symmetric
    /// to [`Self::AttributesNotYetEncoded`]: the writer fails
    /// the batch until the canonicalisation PR replaces the
    /// miner's `format!("{any_value:?}")` call site with a real
    /// proto3-JSON-with-OTLP-overrides encoder.
    StructuredBodyNotYetCanonical,
    /// A clean-attach `body_kind = String` record had too few
    /// `separators` entries to satisfy the RFC 0005 §3.2
    /// invariant ("`tokens.len() + 1` elements when
    /// `body_kind = String`"). The writer doesn't know
    /// `tokens.len()` directly (the template store is the
    /// reader's concern), but `tokens.len() >= params.len()`
    /// always — so `separators.len() < params.len() + 1` is a
    /// definite contract violation. Carves out `lossy_flag =
    /// true` rows (parse-failure / tokenizer-failure paths
    /// where RFC 0001 §6.6 reconstruction returns the retained
    /// body verbatim and doesn't walk `separators`).
    InvalidSeparatorsForString {
        /// Lower bound on the valid separator count, derived
        /// as `params.len() + 1`.
        expected_at_least: usize,
        /// Actual separator-vec length on the offending record.
        actual: usize,
    },
    /// A `lossy_flag = true` String record had `body = None`.
    /// The §6.6 reconstruction path for lossy rows returns the
    /// retained body verbatim — without a body to fall back
    /// to, the record is unreconstructable downstream. Reject
    /// at write time so the file never contains a row that
    /// would surface this corruption on read.
    MissingBodyForLossyString,
    /// Arrow rejected the constructed `RecordBatch` (schema
    /// mismatch, column-length mismatch, etc.). Internal bug if
    /// it ever fires — the array builders are constructed against
    /// `data_schema()` directly.
    Arrow(ArrowError),
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimestampOverflow { field, value } => write!(
                f,
                "{field} = {value} exceeds i64::MAX (RFC 0005 §3.2 u64→i64 overflow contract)",
            ),
            Self::AttributesNotYetEncoded { column, count } => write!(
                f,
                "{column}: canonical-JSON encoding of {count} KeyValue entries is deferred to \
                 the RFC 0005 §3.3 canonicalisation PR (corpus / bench inputs today carry \
                 empty attributes; the RFC 0003 receiver is what populates them)",
            ),
            Self::UnsupportedAbsentBody => write!(
                f,
                "record carries BodyKind::Absent (wire-absent body), which RFC 0005 §3.2's \
                 body_kind column does not yet encode (the column pins ordinals 0=String, \
                 1=Structured); a future RFC 0005 amendment is required to represent this \
                 in the schema",
            ),
            Self::StructuredBodyNotYetCanonical => write!(
                f,
                "record carries body_kind = Structured but the body column would receive the \
                 miner's interim Debug rendering rather than RFC 0005 §3.3's OTLP-canonical \
                 JSON; the canonicalisation PR (see PR-E1 breadcrumb on \
                 ourios_core::otlp::Body::Structured) must land before structured rows can \
                 be written",
            ),
            Self::InvalidSeparatorsForString {
                expected_at_least,
                actual,
            } => write!(
                f,
                "clean-attach String record has separators.len() = {actual} which is below \
                 the lower bound expected_at_least = {expected_at_least} (params.len() + 1) \
                 required by RFC 0005 §3.2's `tokens.len() + 1` invariant for body_kind = \
                 String. Parse-failure / tokenizer-failure rows (lossy_flag = true) are \
                 exempt from this check",
            ),
            Self::MissingBodyForLossyString => write!(
                f,
                "lossy_flag = true String record has body = None, but RFC 0001 §6.6's lossy \
                 reconstruction path returns the retained body verbatim — without one, the \
                 record is unreconstructable on read",
            ),
            Self::Arrow(e) => write!(f, "arrow rejected RecordBatch: {e}"),
        }
    }
}

impl std::error::Error for BatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TimestampOverflow { .. }
            | Self::AttributesNotYetEncoded { .. }
            | Self::UnsupportedAbsentBody
            | Self::StructuredBodyNotYetCanonical
            | Self::InvalidSeparatorsForString { .. }
            | Self::MissingBodyForLossyString => None,
            Self::Arrow(e) => Some(e),
        }
    }
}

impl From<TimestampOverflowError> for BatchError {
    fn from(e: TimestampOverflowError) -> Self {
        Self::TimestampOverflow {
            field: e.field,
            value: e.value,
        }
    }
}

/// One builder per column in `data_schema()` order. Each `append`
/// call pushes one row across every column; `finish` returns the
/// column array vector ready for `RecordBatch::try_new`.
struct Builders {
    tenant_id: StringBuilder,
    template_id: UInt64Builder,
    template_version: UInt32Builder,
    time_unix_nano: TimestampNanosecondBuilder,
    observed_time_unix_nano: TimestampNanosecondBuilder,
    severity_number: UInt8Builder,
    severity_text: StringBuilder,
    scope_name: StringBuilder,
    scope_version: StringBuilder,
    attributes: StringBuilder,
    dropped_attributes_count: UInt32Builder,
    resource_attributes: StringBuilder,
    trace_id: FixedSizeBinaryBuilder,
    span_id: FixedSizeBinaryBuilder,
    flags: UInt32Builder,
    event_name: StringBuilder,
    body_kind: UInt8Builder,
    body: BinaryBuilder,
    params: GenericListBuilder<i32, StructBuilder>,
    separators: GenericListBuilder<i32, BinaryBuilder>,
    confidence: Float32Builder,
    lossy_flag: BooleanBuilder,
}

impl Builders {
    fn with_capacity(cap: usize) -> Self {
        let params_value_builder = StructBuilder::new(
            vec![
                Field::new("type_tag", DataType::Int32, false),
                Field::new("value", DataType::Binary, true),
            ],
            vec![
                Box::new(Int32Builder::new()),
                Box::new(BinaryBuilder::new()),
            ],
        );
        Self {
            tenant_id: StringBuilder::with_capacity(cap, 0),
            template_id: UInt64Builder::with_capacity(cap),
            template_version: UInt32Builder::with_capacity(cap),
            time_unix_nano: TimestampNanosecondBuilder::with_capacity(cap).with_timezone("UTC"),
            observed_time_unix_nano: TimestampNanosecondBuilder::with_capacity(cap)
                .with_timezone("UTC"),
            severity_number: UInt8Builder::with_capacity(cap),
            severity_text: StringBuilder::with_capacity(cap, 0),
            scope_name: StringBuilder::with_capacity(cap, 0),
            scope_version: StringBuilder::with_capacity(cap, 0),
            attributes: StringBuilder::with_capacity(cap, 0),
            dropped_attributes_count: UInt32Builder::with_capacity(cap),
            resource_attributes: StringBuilder::with_capacity(cap, 0),
            trace_id: FixedSizeBinaryBuilder::with_capacity(cap, 16),
            span_id: FixedSizeBinaryBuilder::with_capacity(cap, 8),
            flags: UInt32Builder::with_capacity(cap),
            event_name: StringBuilder::with_capacity(cap, 0),
            body_kind: UInt8Builder::with_capacity(cap),
            body: BinaryBuilder::with_capacity(cap, 0),
            params: GenericListBuilder::new(params_value_builder).with_field(Field::new(
                "element",
                DataType::Struct(
                    vec![
                        Field::new("type_tag", DataType::Int32, false),
                        Field::new("value", DataType::Binary, true),
                    ]
                    .into(),
                ),
                false,
            )),
            separators: GenericListBuilder::new(BinaryBuilder::new()).with_field(Field::new(
                "element",
                DataType::Binary,
                false,
            )),
            confidence: Float32Builder::with_capacity(cap),
            lossy_flag: BooleanBuilder::with_capacity(cap),
        }
    }

    fn append(&mut self, r: &MinedRecord) -> Result<(), BatchError> {
        self.tenant_id.append_value(r.tenant_id.as_str());
        self.template_id.append_value(r.template_id);
        self.template_version.append_value(r.template_version);

        let t = i64::try_from(r.time_unix_nano).map_err(|_| BatchError::TimestampOverflow {
            field: "time_unix_nano",
            value: r.time_unix_nano,
        })?;
        self.time_unix_nano.append_value(t);

        match r.observed_time_unix_nano {
            Some(ts) => {
                let v = i64::try_from(ts).map_err(|_| BatchError::TimestampOverflow {
                    field: "observed_time_unix_nano",
                    value: ts,
                })?;
                self.observed_time_unix_nano.append_value(v);
            }
            None => self.observed_time_unix_nano.append_null(),
        }

        self.severity_number.append_value(r.severity_number);
        append_option_str(&mut self.severity_text, r.severity_text.as_deref());
        append_option_str(&mut self.scope_name, r.scope_name.as_deref());
        append_option_str(&mut self.scope_version, r.scope_version.as_deref());

        append_attributes(&mut self.attributes, columns::ATTRIBUTES, &r.attributes)?;
        self.dropped_attributes_count
            .append_value(r.dropped_attributes_count);
        append_attributes(
            &mut self.resource_attributes,
            columns::RESOURCE_ATTRIBUTES,
            &r.resource_attributes,
        )?;

        match r.trace_id {
            Some(b) => self.trace_id.append_value(b).map_err(BatchError::Arrow)?,
            None => self.trace_id.append_null(),
        }
        match r.span_id {
            Some(b) => self.span_id.append_value(b).map_err(BatchError::Arrow)?,
            None => self.span_id.append_null(),
        }
        self.flags.append_value(r.flags);
        append_option_str(&mut self.event_name, r.event_name.as_deref());

        self.body_kind.append_value(body_kind_ordinal(r.body_kind)?);
        // RFC 0005 §3.3: when `body_kind = Structured`, the body
        // column carries OTLP-canonical JSON. The miner today
        // populates `body` with `format!("{any_value:?}")` per
        // the PR-E1 breadcrumb on `ourios_core::otlp::Body::
        // Structured` — that's *not* canonical JSON, so writing
        // it would store non-conforming bytes for a §3.3-
        // governed column. Reject these records until the
        // canonicalisation PR lands (symmetric to the
        // `AttributesNotYetEncoded` deferral above).
        if r.body_kind == BodyKind::Structured {
            return Err(BatchError::StructuredBodyNotYetCanonical);
        }
        match r.body.as_deref() {
            Some(s) => self.body.append_value(s.as_bytes()),
            None => self.body.append_null(),
        }

        // RFC 0005 §3.2 / RFC 0001 §6.6 reconstruction
        // invariants for `body_kind = String`:
        // - Clean attach (`lossy_flag = false`): `separators.len()
        //   >= params.len() + 1`. The writer can't reach
        //   `tokens.len()` (the template store is the reader's
        //   concern), but `tokens.len() >= params.len()` always
        //   — so falling below `params.len() + 1` is a definite
        //   contract violation.
        // - Lossy attach (`lossy_flag = true`): reconstruction
        //   returns the retained `body` verbatim and doesn't
        //   walk `separators`. The carve-out is real, but
        //   `body` MUST be present — without it the lossy
        //   record is unreconstructable on read.
        if r.body_kind == BodyKind::String {
            if r.lossy_flag {
                if r.body.is_none() {
                    return Err(BatchError::MissingBodyForLossyString);
                }
            } else {
                let expected_at_least = r.params.len() + 1;
                if r.separators.len() < expected_at_least {
                    return Err(BatchError::InvalidSeparatorsForString {
                        expected_at_least,
                        actual: r.separators.len(),
                    });
                }
            }
        }
        append_params(&mut self.params, &r.params);
        append_separators(&mut self.separators, &r.separators);

        self.confidence.append_value(r.confidence);
        self.lossy_flag.append_value(r.lossy_flag);

        Ok(())
    }

    fn finish(mut self) -> Vec<ArrayRef> {
        // Column order MUST match data_schema(); RFC0005.10's
        // schema-pin test catches drift on the schema side, the
        // RecordBatch::try_new call below catches drift on the
        // batch side (mismatched field count / type panics
        // surface as `BatchError::Arrow`).
        let _ = columns::TENANT_ID; // greppability-only; silences unused-warn if columns:: is not referenced elsewhere in this fn
        vec![
            Arc::new(self.tenant_id.finish()),
            Arc::new(self.template_id.finish()),
            Arc::new(self.template_version.finish()),
            Arc::new(self.time_unix_nano.finish()),
            Arc::new(self.observed_time_unix_nano.finish()),
            Arc::new(self.severity_number.finish()),
            Arc::new(self.severity_text.finish()),
            Arc::new(self.scope_name.finish()),
            Arc::new(self.scope_version.finish()),
            Arc::new(self.attributes.finish()),
            Arc::new(self.dropped_attributes_count.finish()),
            Arc::new(self.resource_attributes.finish()),
            Arc::new(self.trace_id.finish()),
            Arc::new(self.span_id.finish()),
            Arc::new(self.flags.finish()),
            Arc::new(self.event_name.finish()),
            Arc::new(self.body_kind.finish()),
            Arc::new(self.body.finish()),
            Arc::new(self.params.finish()),
            Arc::new(self.separators.finish()),
            Arc::new(self.confidence.finish()),
            Arc::new(self.lossy_flag.finish()),
        ]
    }
}

fn append_option_str(b: &mut StringBuilder, v: Option<&str>) {
    match v {
        Some(s) => b.append_value(s),
        None => b.append_null(),
    }
}

/// Map an in-memory [`BodyKind`] to the §3.2 on-disk `body_kind`
/// ordinal. The schema pins exactly two ordinals (`0 = String,
/// 1 = Structured`); `BodyKind::Absent` has no on-disk
/// representation today and the writer rejects records carrying
/// it via [`BatchError::UnsupportedAbsentBody`] rather than
/// silently misclassifying them.
fn body_kind_ordinal(k: BodyKind) -> Result<u8, BatchError> {
    match k {
        BodyKind::String => Ok(0),
        BodyKind::Structured => Ok(1),
        BodyKind::Absent => Err(BatchError::UnsupportedAbsentBody),
    }
}

fn append_params(
    builder: &mut GenericListBuilder<i32, StructBuilder>,
    params: &[ourios_core::record::Param],
) {
    let values = builder.values();
    for p in params {
        values
            .field_builder::<Int32Builder>(0)
            .expect("type_tag field index 0")
            .append_value(param_type_ordinal(p.type_tag));
        values
            .field_builder::<BinaryBuilder>(1)
            .expect("value field index 1")
            .append_value(p.value.as_bytes());
        values.append(true);
    }
    builder.append(true);
}

/// Stable on-disk ordinal for [`ParamType`] per RFC 0001 §6.5
/// and RFC 0005 §3.2 ("The `params.type_tag` integer enum is
/// `0..=7` matching RFC 0001's `ParamType` ordering: `IP, UUID,
/// NUM, HEX, TS, PATH, STR, OVERFLOW`").
///
/// Using an explicit `match` rather than `enum as i32` makes the
/// on-disk encoding immune to a future reorder of the
/// `ParamType` variants in `ourios-core` — a reorder would no
/// longer silently rewrite the column's semantic content. Adding
/// a new variant is a §3.8 schema amendment and the match arm is
/// the compile-time signal that the new ordinal needs picking.
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
        // `Unknown(N)` is the reader-side catch-all for ordinals
        // a future writer added that this enum doesn't yet
        // recognise (RFC 0005 §3.9). Round-trip the raw ordinal
        // on read-then-write so the unknown variant doesn't get
        // lost or remapped.
        ParamType::Unknown(ord) => ord,
    }
}

fn append_separators(builder: &mut GenericListBuilder<i32, BinaryBuilder>, separators: &[String]) {
    for s in separators {
        builder.values().append_value(s.as_bytes());
    }
    builder.append(true);
}

/// Interim canonical-JSON appender per the PR-E1 breadcrumb on
/// [`ourios_core::otlp::Body::Structured`].
///
/// - Empty input → appends the literal `"[]"` directly into the
///   builder (RFC 0005 §3.2's `Vec::new()` ↔ `[]` round-trip
///   rule). The `&'static str` argument means no per-row `String`
///   allocation — important on the hot path where corpus / bench
///   inputs today carry empty attributes for every record.
/// - Non-empty input → returns
///   [`BatchError::AttributesNotYetEncoded`] so the writer fails
///   the batch gracefully rather than crashing the ingest process.
///   The original cut emitted `format!("{attrs:?}")` (Rust `Debug`
///   rendering) which is *not* valid JSON; the structured error
///   surfaces the gap loudly without inviting downstream code to
///   silently store non-JSON masquerading as JSON. RFC 0005 §3.3
///   names the normative encoding (proto3 JSON with OTLP
///   overrides); implementing it is the canonicalisation PR's
///   job.
fn append_attributes(
    b: &mut StringBuilder,
    column: &'static str,
    attrs: &[KeyValue],
) -> Result<(), BatchError> {
    if attrs.is_empty() {
        b.append_value("[]");
        return Ok(());
    }
    Err(BatchError::AttributesNotYetEncoded {
        column,
        count: attrs.len(),
    })
}

#[cfg(test)]
mod tests {
    use arrow_array::cast::AsArray;
    use ourios_core::otlp::{AnyValue, KeyValue, any_value};
    use ourios_core::record::BodyKind;
    use ourios_core::tenant::TenantId;

    use super::*;

    fn empty_record() -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new("t"),
            template_id: 0,
            template_version: 0,
            severity_number: 0,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            time_unix_nano: 1,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::String,
            params: Vec::new(),
            // RFC 0005 §3.2: `tokens.len() + 1` separators when
            // body_kind = String. With zero tokens, one
            // (empty-string) separator span — the minimum that
            // satisfies the §3.2 invariant for clean-attach
            // String records.
            separators: vec![String::new()],
            body: None,
            confidence: 0.0,
            lossy_flag: false,
        }
    }

    /// Sanity: the empty-attributes path serialises to literal `[]`
    /// (the §3.2 `Vec::new()` ↔ `[]` round-trip rule) and does not
    /// hit the `AttributesNotYetEncoded` branch.
    #[test]
    fn empty_attributes_serialise_to_open_bracket_close_bracket() {
        let batch = mined_records_to_batch(&[empty_record()]).expect("batch builds");
        let attrs_idx = batch.schema().index_of(crate::columns::ATTRIBUTES).unwrap();
        let arr = batch.column(attrs_idx).as_string::<i32>();
        assert_eq!(arr.value(0), "[]");
        let resource_idx = batch
            .schema()
            .index_of(crate::columns::RESOURCE_ATTRIBUTES)
            .unwrap();
        let res = batch.column(resource_idx).as_string::<i32>();
        assert_eq!(res.value(0), "[]");
    }

    /// Non-empty attributes return `AttributesNotYetEncoded`
    /// rather than panicking via `unimplemented!()`. Pins the
    /// graceful-error contract until the RFC 0005 §3.3
    /// canonicalisation PR replaces this branch with a real
    /// encoder.
    #[test]
    fn non_empty_attributes_returns_not_yet_encoded_error() {
        let mut rec = empty_record();
        rec.attributes = vec![KeyValue {
            key: "client.address".to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue("10.0.0.1".to_string())),
            }),
            ..KeyValue::default()
        }];
        let err = mined_records_to_batch(&[rec]).expect_err("non-empty attrs must error");
        match err {
            BatchError::AttributesNotYetEncoded { column, count } => {
                assert_eq!(column, "attributes");
                assert_eq!(count, 1);
            }
            other => panic!("expected AttributesNotYetEncoded, got {other:?}"),
        }
    }

    /// RFC 0005 §3.2 invariant: clean-attach `body_kind = String`
    /// rows MUST satisfy `separators.len() >= params.len() + 1`.
    /// Reject otherwise so a downstream §6.6 reconstruction
    /// never gets a shape-mismatched record. Tests both the
    /// "empty separators" case (params=0, separators=0) and the
    /// "non-empty but too few" case (params=2, separators=1).
    #[test]
    fn invalid_separators_on_clean_string_returns_error() {
        // Case 1: empty separators with empty params (lower
        // bound is 1, actual is 0).
        let mut rec = empty_record();
        rec.params = Vec::new();
        rec.separators = Vec::new();
        rec.lossy_flag = false;
        let err = mined_records_to_batch(&[rec]).expect_err("empty separators must error");
        match err {
            BatchError::InvalidSeparatorsForString {
                expected_at_least,
                actual,
            } => {
                assert_eq!(expected_at_least, 1);
                assert_eq!(actual, 0);
            }
            other => panic!("expected InvalidSeparatorsForString, got {other:?}"),
        }

        // Case 2: non-empty but below the lower bound
        // (params.len() = 2 ⇒ separators must be at least 3).
        let mut rec = empty_record();
        rec.params = vec![
            ourios_core::record::Param {
                type_tag: ParamType::Num,
                value: "1".to_string(),
            },
            ourios_core::record::Param {
                type_tag: ParamType::Num,
                value: "2".to_string(),
            },
        ];
        rec.separators = vec![String::new(), String::new()];
        rec.lossy_flag = false;
        let err = mined_records_to_batch(&[rec])
            .expect_err("non-empty but below the lower bound must error");
        match err {
            BatchError::InvalidSeparatorsForString {
                expected_at_least,
                actual,
            } => {
                assert_eq!(expected_at_least, 3);
                assert_eq!(actual, 2);
            }
            other => panic!("expected InvalidSeparatorsForString, got {other:?}"),
        }
    }

    /// The lossy carve-out requires the retained `body` to be
    /// present: without it, RFC 0001 §6.6 reconstruction has
    /// nothing to fall back to. Reject the missing-body case.
    #[test]
    fn lossy_string_without_body_returns_missing_body_error() {
        let mut rec = empty_record();
        rec.separators = Vec::new();
        rec.lossy_flag = true;
        rec.body = None;
        let err = mined_records_to_batch(&[rec]).expect_err("lossy + body=None must error");
        assert!(
            matches!(err, BatchError::MissingBodyForLossyString),
            "expected MissingBodyForLossyString, got {err:?}",
        );
    }

    /// The carve-out: `lossy_flag = true` (parse-failure /
    /// tokenizer-failure rows where reconstruction returns the
    /// retained body verbatim) is allowed to have empty
    /// separators — §6.6's reconstruction doesn't walk them on
    /// the lossy path.
    #[test]
    fn empty_separators_on_lossy_string_is_allowed() {
        let mut rec = empty_record();
        rec.separators = Vec::new();
        rec.lossy_flag = true;
        rec.body = Some("orig line".to_string());
        mined_records_to_batch(&[rec]).expect("lossy_flag carve-out must not error");
    }

    /// `BodyKind::Structured` rows can't yet be written
    /// faithfully — the miner stores an interim Debug
    /// rendering of the `AnyValue` rather than canonical JSON.
    /// The writer rejects until the canonicalisation PR lands.
    #[test]
    fn structured_body_kind_returns_not_yet_canonical_error() {
        let mut rec = empty_record();
        rec.body_kind = BodyKind::Structured;
        rec.body = Some("{\"placeholder\":true}".to_string());
        let err = mined_records_to_batch(&[rec]).expect_err("structured body must error");
        assert!(
            matches!(err, BatchError::StructuredBodyNotYetCanonical),
            "expected StructuredBodyNotYetCanonical, got {err:?}",
        );
    }

    /// `BodyKind::Absent` is not representable in the §3.2
    /// `body_kind` column today (the ordinals pin to
    /// `0 = String, 1 = Structured`). The writer rejects such
    /// records rather than silently lumping them with String.
    #[test]
    fn absent_body_kind_returns_unsupported_error() {
        let mut rec = empty_record();
        rec.body_kind = BodyKind::Absent;
        let err = mined_records_to_batch(&[rec]).expect_err("Absent body must error");
        assert!(
            matches!(err, BatchError::UnsupportedAbsentBody),
            "expected UnsupportedAbsentBody, got {err:?}",
        );
    }

    /// Same contract on the `resource_attributes` side: empty in
    /// the primary `attributes` column, populated in
    /// `resource_attributes`, still errors with the right column
    /// name.
    #[test]
    fn non_empty_resource_attributes_errors_on_correct_column() {
        let mut rec = empty_record();
        rec.resource_attributes = vec![KeyValue {
            key: "service.name".to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue("ourios".to_string())),
            }),
            ..KeyValue::default()
        }];
        let err = mined_records_to_batch(&[rec]).expect_err("non-empty resource attrs must error");
        match err {
            BatchError::AttributesNotYetEncoded { column, count } => {
                assert_eq!(column, "resource_attributes");
                assert_eq!(count, 1);
            }
            other => panic!("expected AttributesNotYetEncoded, got {other:?}"),
        }
    }
}
