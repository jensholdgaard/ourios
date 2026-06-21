//! Query-row body rendering (RFC 0017 §3.3 / §3.4).
//!
//! Turns a stored [`MinedRecord`] into the body a query returns, honouring the
//! three-zone reader-render model (RFC 0001 §6.3 / §6.6):
//!
//! - a **string** body renders from its versioned template tokens (looked up
//!   in the read-time registry) + params + separators — bit-identical when the
//!   row is clean (`CLAUDE.md` §3.3), or the retained `body` verbatim when the
//!   row is lossy / a parse failure / not in the registry;
//! - a **structured** body (`body_kind = Structured`, the OTLP `Body` was a
//!   map/array) is decoded from its canonical JSON and returned **as
//!   structure** — never flattened to a byte line.
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
    /// JSON `body`, returned as structure (map/array), never flattened.
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
        // Corrupt structured row (absent/undecodable body): match `render`'s
        // `BodyKind::Structured` missing-body arm rather than claim structure.
        let (line, reconstruction) = render(record, &[]);
        return LogBody::Rendered {
            line,
            reconstruction,
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
