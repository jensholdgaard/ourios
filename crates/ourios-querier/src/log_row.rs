//! Query-row body rendering (RFC 0017 §3.3 / §3.4).
//!
//! Turns a stored [`MinedRecord`] into the body a query returns, honouring the
//! three-zone reader-render model (RFC 0001 §6.3 / §6.6):
//!
//! - a **string** body renders from its versioned template tokens (looked up
//!   in the read-time registry) + params + separators — bit-identical when the
//!   row is clean (`CLAUDE.md` §3.3), or the retained `body` verbatim when the
//!   row is lossy / a parse failure / not in the registry;
//! - a **structured** body (`body_kind = Structured` — any non-`String` OTLP
//!   `AnyValue`: kvlist / array *and* scalars like int / bool / bytes) is
//!   decoded from its canonical JSON and returned **as the typed `AnyValue`**
//!   — never flattened to a byte line.
//!
//! The string/lossy/absent zones reuse `ourios_miner::reconstruct::render`
//! verbatim; this layer only adds the registry lookup (the versioned tokens
//! §6.6 left out) and the structured → `AnyValue` decode.

use ourios_core::otlp::canonical::decode_any_value;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_miner::reconstruct::{Reconstruction, render};

use crate::TemplateRegistry;

/// One returned query row — a faithful OTLP `LogRecord` (RFC 0017 §3.4).
///
/// Every field is Ourios-owned (no `arrow` / `DataFusion` / SQL type crosses
/// this boundary — hazard `CLAUDE.md` §4.6 / RFC0017.7); `KeyValue` / `AnyValue`
/// are the `opentelemetry-proto` types Ourios already re-exports through
/// `ourios_core::otlp`, not engine types. It mirrors the stored
/// [`MinedRecord`] field-for-field over the OTLP `LogRecord` set (RFC 0005 §3.2),
/// so a read drops no field the wire carried and the schema kept (RFC0017.8),
/// plus the `template_id` / `template_version` the row was mined under.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct LogRow {
    /// Source event time (`0` = unknown, per OTLP).
    pub time_unix_nano: u64,
    /// Collector observation time, when set.
    pub observed_time_unix_nano: Option<u64>,
    /// OTLP `SeverityNumber` (preserved verbatim — RFC 0018 §3.5).
    pub severity_number: u8,
    /// Source's original severity string.
    pub severity_text: Option<String>,
    /// Trace correlation, when set.
    pub trace_id: Option<[u8; 16]>,
    /// Span correlation, when set.
    pub span_id: Option<[u8; 8]>,
    /// W3C trace flags (lower 8 bits).
    pub flags: u32,
    /// Structured-event identifier.
    pub event_name: Option<String>,
    /// `InstrumentationScope.name`.
    pub scope_name: Option<String>,
    /// `InstrumentationScope.version`.
    pub scope_version: Option<String>,
    /// `InstrumentationScope.attributes` (RFC 0018 §3.1), decoded to typed
    /// key/values — not an opaque JSON blob.
    pub scope_attributes: Vec<KeyValue>,
    /// `ResourceLogs.schema_url` (RFC 0018 §3.1), when set.
    pub resource_schema_url: Option<String>,
    /// `ScopeLogs.schema_url` (RFC 0018 §3.1), when set.
    pub scope_schema_url: Option<String>,
    /// Per-record attributes, decoded to typed key/values.
    pub attributes: Vec<KeyValue>,
    /// Resource attributes, decoded to typed key/values.
    pub resource_attributes: Vec<KeyValue>,
    /// Truncation indicator carried verbatim from the wire.
    pub dropped_attributes_count: u32,
    /// The Ourios template the row was mined under.
    pub template_id: u64,
    /// The leaf `template_version` the row was stamped with (selects the
    /// token set the body was rendered against — RFC 0017 §3.5).
    pub template_version: u32,
    /// The rendered / structured body (RFC 0017 §3.3).
    pub body: LogBody,
}

impl LogRow {
    /// Build a `LogRow` from a stored [`MinedRecord`], rendering its body
    /// against the read-time `registry` (RFC 0017 §3.3/§3.4). Every OTLP field
    /// the schema stored is carried through unchanged (RFC0017.8).
    #[must_use]
    pub fn from_record(record: &MinedRecord, registry: &TemplateRegistry) -> Self {
        Self {
            time_unix_nano: record.time_unix_nano,
            observed_time_unix_nano: record.observed_time_unix_nano,
            severity_number: record.severity_number,
            severity_text: record.severity_text.clone(),
            trace_id: record.trace_id,
            span_id: record.span_id,
            flags: record.flags,
            event_name: record.event_name.clone(),
            scope_name: record.scope_name.clone(),
            scope_version: record.scope_version.clone(),
            scope_attributes: record.scope_attributes.clone(),
            resource_schema_url: record.resource_schema_url.clone(),
            scope_schema_url: record.scope_schema_url.clone(),
            attributes: record.attributes.clone(),
            resource_attributes: record.resource_attributes.clone(),
            dropped_attributes_count: record.dropped_attributes_count,
            template_id: record.template_id,
            template_version: record.template_version,
            body: render_log_body(record, registry),
        }
    }
}

/// The body of a returned query row (RFC 0017 §3.4). A sum type so invalid
/// states are unrepresentable: a string body always carries its
/// [`Reconstruction`] marker, and a structured body is faithful by
/// construction (the canonical JSON round-trips, no template walk).
///
/// Marked `#[non_exhaustive]` — like the public, expected-to-evolve
/// [`crate::QueryError`] — so a future body representation can be added
/// without breaking downstream exhaustive `match`es.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum LogBody {
    /// `body_kind = String` — the §3.3 three-zone result: the
    /// rendered bytes plus whether they were faithfully reconstructed or
    /// returned from the retained `body` verbatim.
    Rendered {
        line: Vec<u8>,
        reconstruction: Reconstruction,
    },
    /// `body_kind = Structured` — the `AnyValue` decoded from the canonical
    /// JSON `body`, returned as the typed value (any non-`String` variant:
    /// kvlist / array or a scalar int / bool / bytes), never flattened.
    Structured(AnyValue),
    /// `body_kind = Absent` — the wire delivered no body (RFC 0025
    /// §3.2). Deliberately distinct from
    /// [`LogBody::Rendered`] with an empty line: an empty-string
    /// body and no body are different legal records, and the query
    /// surface must not collapse them.
    Absent,
}

/// Render `record`'s body for a query row against the read-time `registry`
/// (RFC 0017 §3.3).
///
/// A structured row returns [`LogBody::Structured`] with its decoded
/// `AnyValue`; a structured row whose `body` is absent or undecodable (a
/// corrupt row — there is no structure to return) falls back to the
/// render-contract empty / [`Reconstruction::RetainedVerbatim`], never
/// `Structured` over nothing. An absent row returns [`LogBody::Absent`]
/// (RFC 0025 §3.2). A string row renders via its versioned
/// tokens; when its `(template_id, template_version)` is not in the registry
/// the empty token slice makes `render` fall back to the retained `body`
/// verbatim (§3.3), never a wrong reconstruction.
#[must_use]
pub fn render_log_body(record: &MinedRecord, registry: &TemplateRegistry) -> LogBody {
    if record.body_kind == BodyKind::Absent {
        // RFC 0025 §3.2: absence renders as no body at all — never
        // an empty string, which is a different legal record.
        return LogBody::Absent;
    }
    if record.body_kind == BodyKind::Structured {
        if let Some(body) = record.body.as_deref()
            && let Ok(value) = decode_any_value(body.as_bytes())
        {
            return LogBody::Structured(value);
        }
        // Corrupt structured row — body absent OR present-but-undecodable.
        // No structure to return, and it is *not* faithful, so surface empty /
        // RetainedVerbatim directly. We must NOT delegate to `render`: its
        // `BodyKind::Structured` arm returns the raw `body` bytes as `Faithful`
        // whenever a body is present, which for an *undecodable* body would
        // falsely claim faithfulness.
        return LogBody::Rendered {
            line: Vec::new(),
            reconstruction: Reconstruction::RetainedVerbatim,
        };
    }

    let tokens = registry
        .get(&(record.template_id, record.template_version))
        .map_or(&[][..], Vec::as_slice);
    let (line, reconstruction) = render(record, tokens);
    LogBody::Rendered {
        line,
        reconstruction,
    }
}

#[cfg(test)]
mod tests {
    use ourios_core::otlp::any_value::Value;
    use ourios_core::otlp::canonical::encode_any_value;
    use ourios_core::record::Param;
    use ourios_core::tenant::TenantId;
    use ourios_miner::tree::OwnedToken;

    use super::{
        AnyValue, BodyKind, LogBody, MinedRecord, Reconstruction, TemplateRegistry, render_log_body,
    };

    fn record(body_kind: BodyKind) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new("t"),
            template_id: 0,
            template_version: 0,
            severity_number: 0,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
            time_unix_nano: 0,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: Vec::new(),
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind,
            params: Vec::new(),
            separators: Vec::new(),
            body: None,
            confidence: 0.0,
            lossy_flag: false,
        }
    }

    #[test]
    fn structured_decode_success_returns_typed_value() {
        // A scalar structured body (not a map/array) still round-trips.
        let value = AnyValue {
            value: Some(Value::IntValue(7)),
        };
        let mut r = record(BodyKind::Structured);
        r.body = Some(String::from_utf8(encode_any_value(&value).unwrap()).unwrap());
        assert_eq!(
            render_log_body(&r, &TemplateRegistry::new()),
            LogBody::Structured(value),
        );
    }

    #[test]
    fn structured_undecodable_body_is_retained_not_faithful() {
        // Present but not valid canonical JSON — must NOT claim Faithful over
        // the raw bytes; there is no structure to return.
        let mut r = record(BodyKind::Structured);
        r.body = Some("{not valid json".to_owned());
        assert_eq!(
            render_log_body(&r, &TemplateRegistry::new()),
            LogBody::Rendered {
                line: Vec::new(),
                reconstruction: Reconstruction::RetainedVerbatim,
            },
        );
    }

    #[test]
    fn structured_absent_body_falls_back() {
        let r = record(BodyKind::Structured);
        assert_eq!(
            render_log_body(&r, &TemplateRegistry::new()),
            LogBody::Rendered {
                line: Vec::new(),
                reconstruction: Reconstruction::RetainedVerbatim,
            },
        );
    }

    #[test]
    fn string_in_registry_renders_faithful() {
        let registry: TemplateRegistry = TemplateRegistry::from([(
            (1, 1),
            vec![OwnedToken::Fixed("user".to_owned()), OwnedToken::Wildcard],
        )]);
        let mut r = record(BodyKind::String);
        r.template_id = 1;
        r.template_version = 1;
        r.params = vec![Param {
            type_tag: ourios_core::audit::ParamType::Num,
            value: "42".to_owned(),
        }];
        r.separators = vec![String::new(), " ".to_owned(), String::new()];
        assert_eq!(
            render_log_body(&r, &registry),
            LogBody::Rendered {
                line: b"user 42".to_vec(),
                reconstruction: Reconstruction::Faithful,
            },
        );
    }

    #[test]
    fn string_not_in_registry_returns_retained_body() {
        // No registry entry for (9, 1) → empty tokens → render falls back to
        // the retained body verbatim, never a wrong reconstruction.
        let mut r = record(BodyKind::String);
        r.template_id = 9;
        r.template_version = 1;
        r.body = Some("original line".to_owned());
        assert_eq!(
            render_log_body(&r, &TemplateRegistry::new()),
            LogBody::Rendered {
                line: b"original line".to_vec(),
                reconstruction: Reconstruction::RetainedVerbatim,
            },
        );
    }
}
