//! RFC 0001 §6.6 body reconstruction.
//!
//! Bit-identical recovery of a line's original bytes from
//! `(template, params, separators)`. The function lives in its own
//! module because §6.6 is a self-contained algorithm: it reads
//! the record schema ([`ourios_core::record::MinedRecord`]) and
//! the leaf template shape ([`crate::tree::OwnedToken`]), and
//! returns owned bytes. Reader code is RFC 0002's concern;
//! reconstruct is the contract.

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord};

use crate::tree::OwnedToken;

/// Reconstruct a record's original line bytes per RFC §6.6.
///
/// Behaviour by [`BodyKind`]:
///
/// - [`BodyKind::Absent`]: no body on the wire; returns empty.
/// - [`BodyKind::Structured`]: §6.2 step 0 short-circuits to the
///   §6.1 Ourios canonical body encoding of the `AnyValue`, which
///   the producer (`MinerCluster::ingest_structured`) stores in
///   `record.body` and which this function returns verbatim. RFC
///   §6.1 pins `lossy_flag = false` unconditionally for these
///   records — the body field is the contract. A structured record
///   whose `body` is absent (e.g. a hand-built record, or a row
///   that predates the encoder) degrades to empty bytes rather than
///   panicking.
/// - [`BodyKind::String`] with `lossy_flag = true` or any
///   [`ParamType::Overflow`] entry in `params`: reconstruction is
///   not guaranteed to equal ingest, so the function returns the
///   retained `body`. Per §6.6 a reader should display the body
///   verbatim with an explicit warning for `lossy_flag = true`
///   rather than calling reconstruct, but reconstruct still has a
///   defined output here for callers that don't dispatch on the
///   flag.
/// - [`BodyKind::String`] otherwise: walks `template`,
///   alternating `separators` with literal text (for
///   [`OwnedToken::Fixed`]) or `params[ordinal].value` (for
///   [`OwnedToken::Wildcard`]) in ordinal order.
///
/// The `template` slice must be the leaf's template at
/// `(record.template_id, record.template_version)` for the
/// String/clean path. For Structured / Absent / lossy / overflow
/// records the template is unused — passing an empty slice is
/// fine.
///
/// # Panics
///
/// Never. The function is total over its input shape — a record
/// whose `separators` / `params` lengths don't agree with
/// `template` (a bug in the producer, or a corrupted Parquet row
/// the reader path is asked to reconstruct) falls back to
/// returning the retained `body` (or empty, if `body` is also
/// absent) rather than panicking. `debug_assert`s still catch
/// producer-side bugs in dev builds; releases degrade gracefully
/// because reader-side reconstruction runs over persisted data
/// the function can't validate upstream.
#[must_use]
pub fn reconstruct(record: &MinedRecord, template: &[OwnedToken]) -> Vec<u8> {
    match record.body_kind {
        BodyKind::Absent => Vec::new(),
        BodyKind::Structured => body_bytes_or_empty(record),
        BodyKind::String => {
            if record.lossy_flag
                || record
                    .params
                    .iter()
                    .any(|p| p.type_tag == ParamType::Overflow)
                || !template_shape_matches_record(record, template)
            {
                // §6.6 capture invariants violated — producer bug
                // or persisted-data corruption. Fall back to the
                // retained body instead of panicking on indexing.
                body_bytes_or_empty(record)
            } else {
                reconstruct_from_template(record, template)
            }
        }
    }
}

/// Cheap pre-flight check on the §6.6 capture invariants.
/// `separators.len() == template.len() + 1` and the `params`
/// length equals the count of `Wildcard` slots in the template.
/// A mismatch indicates either a producer bug or a corrupted
/// persisted row.
fn template_shape_matches_record(record: &MinedRecord, template: &[OwnedToken]) -> bool {
    if record.separators.len() != template.len() + 1 {
        return false;
    }
    let wildcard_count = template
        .iter()
        .filter(|t| matches!(t, OwnedToken::Wildcard))
        .count();
    record.params.len() == wildcard_count
}

fn body_bytes_or_empty(record: &MinedRecord) -> Vec<u8> {
    record
        .body
        .as_deref()
        .map_or_else(Vec::new, |s| s.as_bytes().to_vec())
}

fn reconstruct_from_template(record: &MinedRecord, template: &[OwnedToken]) -> Vec<u8> {
    debug_assert_eq!(
        record.separators.len(),
        template.len() + 1,
        "RFC §6.6: separators.len() == template.len() + 1",
    );
    let wildcard_count = template
        .iter()
        .filter(|t| matches!(t, OwnedToken::Wildcard))
        .count();
    debug_assert_eq!(
        record.params.len(),
        wildcard_count,
        "params must align ordinal-by-ordinal with template Wildcards",
    );

    let separator_bytes: usize = record.separators.iter().map(String::len).sum();
    let fixed_bytes: usize = template
        .iter()
        .map(|t| match t {
            OwnedToken::Fixed(s) => s.len(),
            OwnedToken::Wildcard => 0,
        })
        .sum();
    let param_bytes: usize = record.params.iter().map(|p| p.value.len()).sum();
    let mut out = Vec::with_capacity(separator_bytes + fixed_bytes + param_bytes);

    out.extend_from_slice(record.separators[0].as_bytes());
    let mut params_iter = record.params.iter();
    for (i, tok) in template.iter().enumerate() {
        match tok {
            OwnedToken::Fixed(s) => out.extend_from_slice(s.as_bytes()),
            OwnedToken::Wildcard => {
                let p = params_iter
                    .next()
                    .expect("params length == wildcard count (checked by debug_assert above)");
                out.extend_from_slice(p.value.as_bytes());
            }
        }
        out.extend_from_slice(record.separators[i + 1].as_bytes());
    }
    out
}

/// Per-row reconstruction signal for the §6.6 *Reader render
/// contract*. It is the structured, out-of-band warning marker H7.3
/// references: metadata attached *beside* the rendered row, never a
/// mutation of the body bytes. A `RetainedVerbatim` row is rendered
/// from the retained `body` bytes (when present) without
/// reconstruction — for a lossy/overflow row that is the ingested line
/// verbatim `[§3.3]`; the marker tells a consumer (RFC 0007's DSL
/// output layer; a UI) to flag the row as "rendered from the retained
/// `body`, not reconstructed."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reconstruction {
    /// Bytes were rebuilt from `template + params + separators`. With
    /// the row's correct template they equal the ingested line; `render`
    /// cannot guarantee equality if a caller supplies a wrong but
    /// shape-compatible template.
    Faithful,
    /// Bytes are the retained `body` column, returned verbatim
    /// without invoking `reconstruct`.
    RetainedVerbatim,
}

/// Render a stored row per the §6.6 *Reader render contract*,
/// returning the bytes a reader should display alongside the per-row
/// [`Reconstruction`] signal.
///
/// Behaviour by [`BodyKind`]:
///
/// - [`BodyKind::Structured`]: the row carries the §6.1 Ourios
///   canonical body encoding of its `AnyValue` in `body`. That
///   encoding is byte-deterministic and bidirectional
///   (`stored_bytes ↔ AnyValue`), and §6.1 pins `lossy_flag = false`
///   unconditionally for these rows: the stored body *is* the
///   faithful representation of what was logged, so `render` returns
///   it with [`Reconstruction::Faithful`]. No template is walked
///   (structured rows have none); the canonical encoding is the
///   §3.3 round-trip guarantee for the structured branch.
/// - [`BodyKind::String`] with `lossy_flag = true`, or any
///   [`ParamType::Overflow`] param (§6.5): the row's reconstruction
///   is not guaranteed to equal ingest, so the retained `body` is
///   returned **verbatim** with [`Reconstruction::RetainedVerbatim`].
///   `reconstruct` is **not** invoked — no template lookup, no token
///   walk.
/// - [`BodyKind::String`] otherwise, when the `template` shape matches
///   the record: rebuilt from the template with
///   [`Reconstruction::Faithful`]. A shape mismatch (a corrupt row or
///   a wrong template lookup) would make `reconstruct` fall back to
///   the body — which is *not* a faithful reconstruction — so that
///   case returns the body verbatim with
///   [`Reconstruction::RetainedVerbatim`].
/// - [`BodyKind::Absent`]: no body was on the wire and no template was
///   allocated, so there is nothing to reconstruct. `render` returns
///   empty bytes with [`Reconstruction::RetainedVerbatim`] — the
///   honest signal that the (empty) `body` column was surfaced
///   verbatim, not rebuilt from a template.
///
/// `template` is the leaf's tokens for the `String` clean-path walk,
/// taken exactly as [`reconstruct`] takes them today; the read-time
/// `(template_id, template_version) → tokens` registry the clean path
/// needs is out of scope of the §6.6 amendment (RFC 0007). For
/// `Structured` / `Absent` rows the template is unused — passing an
/// empty slice is fine.
#[must_use]
pub fn render(record: &MinedRecord, template: &[OwnedToken]) -> (Vec<u8>, Reconstruction) {
    match record.body_kind {
        // The canonical body encoding is bidirectional and
        // byte-deterministic (§6.1) and `lossy_flag` is always false
        // for structured rows: the stored body faithfully represents
        // the logged `AnyValue`, so it renders Faithful without any
        // template walk.
        BodyKind::Structured => (body_bytes_or_empty(record), Reconstruction::Faithful),
        // No wire body, no template: nothing was reconstructed, so the
        // (empty) body is surfaced verbatim.
        BodyKind::Absent => (
            body_bytes_or_empty(record),
            Reconstruction::RetainedVerbatim,
        ),
        BodyKind::String => {
            // `Faithful` must mean the bytes were actually rebuilt from
            // the template. `reconstruct` falls back to the retained
            // body on a lossy/overflow row *or* a template-shape
            // mismatch (a corrupt row or a wrong template lookup); none
            // of those are faithful reconstructions, so they return the
            // body verbatim with `RetainedVerbatim` and never walk the
            // template.
            let faithful = !record.lossy_flag
                && !record
                    .params
                    .iter()
                    .any(|p| p.type_tag == ParamType::Overflow)
                && template_shape_matches_record(record, template);
            if faithful {
                // The faithful guard already established `reconstruct`'s
                // clean-path preconditions, so call the inner walk
                // directly and skip re-checking lossy / overflow / shape
                // (a second template scan).
                (
                    reconstruct_from_template(record, template),
                    Reconstruction::Faithful,
                )
            } else {
                (
                    body_bytes_or_empty(record),
                    Reconstruction::RetainedVerbatim,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ourios_core::record::Param;
    use ourios_core::tenant::TenantId;

    fn record_envelope(body_kind: BodyKind) -> MinedRecord {
        MinedRecord {
            tenant_id: TenantId::new("t"),
            template_id: 0,
            template_version: 0,
            severity_number: 0,
            severity_text: None,
            scope_name: None,
            scope_version: None,
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
            params: vec![],
            separators: vec![],
            body: None,
            confidence: 0.0,
            lossy_flag: false,
        }
    }

    #[test]
    fn reconstruct_absent_returns_empty() {
        let r = record_envelope(BodyKind::Absent);
        assert_eq!(reconstruct(&r, &[]), Vec::<u8>::new());
    }

    #[test]
    fn reconstruct_structured_returns_body_verbatim() {
        // §6.2 step 0: structured records carry the canonicalised
        // JSON in `body`; reconstruct surfaces it unchanged. This
        // is the post-canonicalisation behaviour the function is
        // already contractually prepared for; see the producer-
        // side `ingest_structured` for the interim state today.
        let mut r = record_envelope(BodyKind::Structured);
        r.body = Some(r#"{"k":"v"}"#.to_string());
        assert_eq!(reconstruct(&r, &[]), br#"{"k":"v"}"#.to_vec());
    }

    #[test]
    fn reconstruct_structured_with_no_body_returns_empty_until_canonicalisation() {
        // Pins the documented producer-side gap: today's
        // `MinerCluster::ingest_structured` emits records with
        // `body = None` and `lossy_flag = false` (per RFC §6.1).
        // `reconstruct` returns the (empty) body verbatim. The
        // function is correct for both the pre- and post-
        // canonicalisation shape; when the canonicalisation PR
        // populates `body`, the existing
        // `reconstruct_structured_returns_body_verbatim` test
        // already pins the post-canonicalisation endpoint.
        let mut r = record_envelope(BodyKind::Structured);
        r.lossy_flag = false;
        r.body = None;
        assert_eq!(reconstruct(&r, &[]), Vec::<u8>::new());
    }

    #[test]
    fn reconstruct_string_with_lossy_flag_returns_body_verbatim() {
        // §6.6: lossy_flag = true (tokenizer / preprocessing
        // failure) means reconstruct must NOT try to rebuild from
        // template; the retained body is the source of truth.
        let mut r = record_envelope(BodyKind::String);
        r.lossy_flag = true;
        r.body = Some("user 42\u{0000}secret".to_string());
        assert_eq!(reconstruct(&r, &[]), b"user 42\0secret".to_vec());
    }

    #[test]
    fn reconstruct_string_with_overflow_param_returns_body_verbatim() {
        // RFC §6.5: an Overflow param means the original value
        // spilled to body; reconstruct returns body rather than a
        // truncated rebuild.
        let mut r = record_envelope(BodyKind::String);
        r.params = vec![Param {
            type_tag: ParamType::Overflow,
            value: "{length:4096,sha256_prefix:...}".to_string(),
        }];
        r.body = Some("user <very-long-blob> ok".to_string());
        assert_eq!(reconstruct(&r, &[]), b"user <very-long-blob> ok".to_vec());
    }

    #[test]
    fn reconstruct_string_clean_rebuilds_from_template_and_separators() {
        // RFC §3.5 worked example: "user 42 logged in from 10.0.0.1"
        // mined to template [Fixed("user"), Wildcard, Fixed("logged"),
        // Fixed("in"), Fixed("from"), Wildcard] with params
        // [{Num,"42"}, {Ip,"10.0.0.1"}] and separators ["", " ",
        // " ", " ", " ", " ", ""].
        let template = vec![
            OwnedToken::Fixed("user".to_string()),
            OwnedToken::Wildcard,
            OwnedToken::Fixed("logged".to_string()),
            OwnedToken::Fixed("in".to_string()),
            OwnedToken::Fixed("from".to_string()),
            OwnedToken::Wildcard,
        ];
        let mut r = record_envelope(BodyKind::String);
        r.params = vec![
            Param {
                type_tag: ParamType::Num,
                value: "42".to_string(),
            },
            Param {
                type_tag: ParamType::Ip,
                value: "10.0.0.1".to_string(),
            },
        ];
        r.separators = vec![
            String::new(),
            " ".to_string(),
            " ".to_string(),
            " ".to_string(),
            " ".to_string(),
            " ".to_string(),
            String::new(),
        ];
        assert_eq!(
            reconstruct(&r, &template),
            b"user 42 logged in from 10.0.0.1".to_vec(),
        );
    }

    #[test]
    fn reconstruct_string_preserves_multibyte_separators() {
        // §3.3 invariant — separators capture the *bytes* between
        // tokens, including Unicode whitespace beyond ASCII. A
        // U+00A0 (NBSP, 2 bytes in UTF-8) between two tokens must
        // round-trip byte-for-byte.
        let template = vec![
            OwnedToken::Fixed("alpha".to_string()),
            OwnedToken::Fixed("beta".to_string()),
        ];
        let mut r = record_envelope(BodyKind::String);
        r.separators = vec![String::new(), "\u{00A0}".to_string(), String::new()];
        let expected = "alpha\u{00A0}beta".as_bytes().to_vec();
        assert_eq!(reconstruct(&r, &template), expected);
    }

    #[test]
    fn reconstruct_shape_mismatch_falls_back_to_body_in_release() {
        // Producer bug or corrupted persisted row: separators /
        // params don't agree with template. Release builds must
        // NOT panic on the indexing inside
        // `reconstruct_from_template`; they must return the
        // retained body (or empty if absent) as a graceful
        // degradation path for reader-side reconstruction over
        // data the caller can't validate upstream.
        //
        // Note: dev builds also avoid the panic — the pre-flight
        // `template_shape_matches_record` check short-circuits to
        // the body-fallback branch before the indexing code
        // runs. The function is total over its input shape.
        let template = vec![
            OwnedToken::Fixed("alpha".to_string()),
            OwnedToken::Wildcard,
            OwnedToken::Fixed("omega".to_string()),
        ];
        let mut r = record_envelope(BodyKind::String);
        // separators.len() should be template.len() + 1 == 4, but
        // we pass only 2 — shape mismatch.
        r.separators = vec![String::new(), String::new()];
        r.params = vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_string(),
        }];
        r.body = Some("alpha 42 omega".to_string());

        // Must not panic; must return the body bytes.
        assert_eq!(reconstruct(&r, &template), b"alpha 42 omega".to_vec());
    }

    #[test]
    fn reconstruct_shape_mismatch_with_no_body_returns_empty() {
        // Same scenario as above but `body` is `None`. The
        // function must still not panic; it returns empty bytes
        // as the safest fallback (the reader can detect "no
        // bytes" and surface a "this record is unreadable"
        // marker instead).
        let template = vec![OwnedToken::Fixed("alpha".to_string()), OwnedToken::Wildcard];
        let mut r = record_envelope(BodyKind::String);
        // params.len() == 0 but template has 1 Wildcard.
        r.separators = vec![String::new(), " ".to_string(), String::new()];
        r.params = vec![];

        assert_eq!(reconstruct(&r, &template), Vec::<u8>::new());
    }

    #[test]
    fn render_lossy_string_returns_body_verbatim_with_marker() {
        // §6.6 reader render contract: a lossy String row returns its
        // retained body verbatim and the RetainedVerbatim marker.
        let mut r = record_envelope(BodyKind::String);
        r.lossy_flag = true;
        r.body = Some("user 42\u{0000}secret".to_string());
        let (bytes, marker) = render(&r, &[]);
        assert_eq!(bytes, b"user 42\0secret".to_vec());
        assert_eq!(marker, Reconstruction::RetainedVerbatim);
    }

    #[test]
    fn render_overflow_string_returns_body_verbatim_with_marker() {
        // §6.5 overflow: the value spilled to body, so render returns
        // body verbatim with RetainedVerbatim rather than rebuilding.
        let mut r = record_envelope(BodyKind::String);
        r.params = vec![Param {
            type_tag: ParamType::Overflow,
            value: "{length:4096,sha256_prefix:...}".to_string(),
        }];
        r.body = Some("user <very-long-blob> ok".to_string());
        let (bytes, marker) = render(&r, &[]);
        assert_eq!(bytes, b"user <very-long-blob> ok".to_vec());
        assert_eq!(marker, Reconstruction::RetainedVerbatim);
    }

    #[test]
    fn render_clean_string_reconstructs_with_faithful_marker() {
        let template = vec![
            OwnedToken::Fixed("user".to_string()),
            OwnedToken::Wildcard,
            OwnedToken::Fixed("logged".to_string()),
            OwnedToken::Fixed("in".to_string()),
            OwnedToken::Fixed("from".to_string()),
            OwnedToken::Wildcard,
        ];
        let mut r = record_envelope(BodyKind::String);
        r.params = vec![
            Param {
                type_tag: ParamType::Num,
                value: "42".to_string(),
            },
            Param {
                type_tag: ParamType::Ip,
                value: "10.0.0.1".to_string(),
            },
        ];
        r.separators = vec![
            String::new(),
            " ".to_string(),
            " ".to_string(),
            " ".to_string(),
            " ".to_string(),
            " ".to_string(),
            String::new(),
        ];
        let (bytes, marker) = render(&r, &template);
        assert_eq!(bytes, b"user 42 logged in from 10.0.0.1".to_vec());
        assert_eq!(marker, Reconstruction::Faithful);
    }

    #[test]
    fn render_clean_string_with_template_shape_mismatch_is_retained_not_faithful() {
        // A clean (non-lossy, non-overflow) String row whose template
        // shape does not match the record — here two params against one
        // wildcard — would make `reconstruct` fall back to the retained
        // body. `render` must report that as RetainedVerbatim, never
        // Faithful: labelling fallback bytes "Faithful" would lie about
        // whether the template was walked.
        let template = vec![OwnedToken::Fixed("user".to_string()), OwnedToken::Wildcard];
        let mut r = record_envelope(BodyKind::String);
        // Valid separators (len == template.len() + 1) so the ONLY shape
        // mismatch is the wildcard/param count (1 wildcard vs 2 params).
        r.separators = vec![String::new(), " ".to_string(), String::new()];
        r.params = vec![
            Param {
                type_tag: ParamType::Num,
                value: "1".to_string(),
            },
            Param {
                type_tag: ParamType::Num,
                value: "2".to_string(),
            },
        ];
        r.body = Some("user 1 and 2".to_string());
        let (bytes, marker) = render(&r, &template);
        assert_eq!(bytes, b"user 1 and 2".to_vec());
        assert_eq!(marker, Reconstruction::RetainedVerbatim);
    }

    #[test]
    fn render_structured_returns_canonical_body_with_faithful_marker() {
        // §6.6 + §6.1: a structured row carries the Ourios canonical
        // body encoding in `body`. That encoding is byte-deterministic
        // and round-trips the `AnyValue`, and `lossy_flag` is always
        // false for structured rows, so `render` surfaces the stored
        // bytes with `Faithful` — no template walk, the canonical body
        // is itself the §3.3 round-trip.
        let mut r = record_envelope(BodyKind::Structured);
        r.confidence = 1.0;
        r.body = Some(
            r#"{"kvlistValue":{"values":[{"key":"k","value":{"intValue":"42"}}]}}"#.to_string(),
        );
        let (bytes, marker) = render(&r, &[]);
        assert_eq!(
            bytes,
            br#"{"kvlistValue":{"values":[{"key":"k","value":{"intValue":"42"}}]}}"#.to_vec(),
        );
        assert_eq!(marker, Reconstruction::Faithful);
    }

    #[test]
    fn render_absent_returns_empty_with_retained_marker() {
        // §6.6: an Absent row had no wire body and no template, so
        // nothing is reconstructed. `render` returns empty bytes with
        // `RetainedVerbatim` — the honest "not rebuilt from a template"
        // signal.
        let r = record_envelope(BodyKind::Absent);
        let (bytes, marker) = render(&r, &[]);
        assert_eq!(bytes, Vec::<u8>::new());
        assert_eq!(marker, Reconstruction::RetainedVerbatim);
    }
}
