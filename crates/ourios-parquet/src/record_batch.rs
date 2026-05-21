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
//! `unimplemented!()`s on any non-empty input. Corpus / bench
//! inputs today carry empty attributes; the RFC 0003 receiver is
//! what populates them, and the canonicalisation PR named in the
//! PR-E1 breadcrumb on [`ourios_core::otlp::Body::Structured`]
//! is the one that fills in the proto3-JSON-with-OTLP-overrides
//! encoder. Fail-loud rather than emit non-JSON `Debug` bytes
//! masquerading as JSON.

use std::fmt;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, FixedSizeBinaryBuilder, Float32Builder, GenericListBuilder,
    Int32Builder, StringBuilder, StructBuilder, TimestampNanosecondBuilder, UInt8Builder,
    UInt32Builder, UInt64Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{ArrowError, DataType, Field};
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
            Self::Arrow(e) => write!(f, "arrow rejected RecordBatch: {e}"),
        }
    }
}

impl std::error::Error for BatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TimestampOverflow { .. } => None,
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

        append_attributes(&mut self.attributes, &r.attributes);
        self.dropped_attributes_count
            .append_value(r.dropped_attributes_count);
        append_attributes(&mut self.resource_attributes, &r.resource_attributes);

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

        self.body_kind.append_value(body_kind_ordinal(r.body_kind));
        match r.body.as_deref() {
            Some(s) => self.body.append_value(s.as_bytes()),
            None => self.body.append_null(),
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

fn body_kind_ordinal(k: BodyKind) -> u8 {
    // RFC 0005 §3.2 body_kind column: 0 = String, 1 = Structured.
    // BodyKind::Absent is an in-memory-only variant for the
    // wire-absent case; on disk it maps to "no body" via a NULL
    // body column, but we still need an ordinal. Reuse `0`
    // (String) for now since `Absent` rows carry no template
    // and a future RFC 0005 amendment may introduce a third
    // ordinal — see RFC0005.10 (the schema-pin) for the contract.
    match k {
        BodyKind::String | BodyKind::Absent => 0,
        BodyKind::Structured => 1,
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
            .append_value(p.type_tag as i32);
        values
            .field_builder::<BinaryBuilder>(1)
            .expect("value field index 1")
            .append_value(p.value.as_bytes());
        values.append(true);
    }
    builder.append(true);
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
/// - Non-empty input → **panic with a clear deferral message**.
///   The original cut of this function emitted `format!(
///   "{attrs:?}")` (Rust `Debug` rendering) which is *not* valid
///   JSON and contradicts §3.3 / the module-level contract;
///   panicking instead surfaces the gap loudly to any future
///   caller that ingests non-empty attributes before the
///   canonicalisation PR lands. RFC 0005 §3.3 names the
///   normative encoding (proto3 JSON with OTLP overrides);
///   implementing it is that PR's job.
fn append_attributes(b: &mut StringBuilder, attrs: &[KeyValue]) {
    if attrs.is_empty() {
        b.append_value("[]");
        return;
    }
    unimplemented!(
        "ourios-parquet: canonical JSON encoding of non-empty KeyValue lists is deferred to \
         the RFC 0005 §3.3 canonicalisation PR (see the PR-E1 breadcrumb on \
         ourios_core::otlp::Body::Structured). Got {} entries — corpus / bench inputs \
         today carry empty attributes; the RFC 0003 receiver is what populates them.",
        attrs.len(),
    );
}
