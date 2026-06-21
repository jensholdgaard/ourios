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

use ourios_core::otlp::AnyValue;
use ourios_core::otlp::canonical::decode_any_value;
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_miner::reconstruct::{Reconstruction, render};

use crate::TemplateRegistry;

/// The body of a returned query row (RFC 0017 §3.4). A sum type so invalid
/// states are unrepresentable: a string body always carries its
/// [`Reconstruction`] marker, and a structured body is faithful by
/// construction (the canonical JSON round-trips, no template walk).
#[derive(Debug, Clone, PartialEq)]
pub enum LogBody {
    /// `body_kind = String` (or absent) — the §3.3 three-zone result: the
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
}

/// Render `record`'s body for a query row against the read-time `registry`
/// (RFC 0017 §3.3).
///
/// A structured row returns [`LogBody::Structured`] with its decoded
/// `AnyValue`; a structured row whose `body` is absent or undecodable (a
/// corrupt row — there is no structure to return) falls back to the
/// render-contract empty / [`Reconstruction::RetainedVerbatim`], never
/// `Structured` over nothing. A string / absent row renders via its versioned
/// tokens; when its `(template_id, template_version)` is not in the registry
/// the empty token slice makes `render` fall back to the retained `body`
/// verbatim (§3.3), never a wrong reconstruction.
#[must_use]
pub fn render_log_body(record: &MinedRecord, registry: &TemplateRegistry) -> LogBody {
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
