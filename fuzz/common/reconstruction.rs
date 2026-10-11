//! The §3.3 bit-identical-reconstruction oracle for the miner target;
//! shared as a source file (each target pulls it in with `#[path]`, like
//! `telemetry.rs`) so the fuzz package keeps no library API (RFC 0015 §3.1).

#![deny(unsafe_code)]

use ourios_core::audit::ParamType;
use ourios_core::record::MinedRecord;
use ourios_miner::cluster::NO_TEMPLATE;
use ourios_miner::reconstruct::{Reconstruction, render};

use crate::snapshots::{Templates, template_of};

/// RFC0015.1 — `CLAUDE.md` §3.3: the record renders back to its own line
/// byte for byte, or it was diverted (`NO_TEMPLATE`) with the line retained
/// verbatim and flagged lossy. A clean row (not lossy, no §6.5 overflow)
/// must additionally have been rebuilt from its template: `render` reports
/// `RetainedVerbatim` for such a row only on a template-shape mismatch,
/// which is a producer bug that a stored body would otherwise paper over.
///
/// # Panics
///
/// On any violation, with a message naming the invariant.
pub fn assert_reconstructs(index: usize, line: &str, record: &MinedRecord, templates: &Templates) {
    if record.template_id == NO_TEMPLATE {
        assert!(
            record.lossy_flag,
            "RFC0015.1 / CLAUDE.md §3.3: record {index}: a diverted string row must be lossy-flagged",
        );
        assert_eq!(
            record.body.as_deref(),
            Some(line),
            "RFC0015.1 / CLAUDE.md §3.3: record {index}: a diverted string row must retain its line verbatim",
        );
        let (rendered, marker) = render(record, &[]);
        assert_bytes_eq(
            index,
            "diverted row renders its retained body",
            &rendered,
            line,
        );
        assert_eq!(
            marker,
            Reconstruction::RetainedVerbatim,
            "RFC0015.1 / CLAUDE.md §3.3: record {index}: a diverted row has no template to be faithful to",
        );
        return;
    }

    let template = template_of(index, record, templates);
    let (rendered, marker) = render(record, template);
    assert_bytes_eq(index, "string-body reconstruction", &rendered, line);

    let clean = !record.lossy_flag
        && !record
            .params
            .iter()
            .any(|p| p.type_tag == ParamType::Overflow);
    if clean {
        assert_eq!(
            marker,
            Reconstruction::Faithful,
            "RFC0015.1 / CLAUDE.md §3.3: record {index}: a clean row must be rebuilt from its template, \
             not surfaced from a retained body (template shape does not match the row)",
        );
    }
}

/// Byte comparison that reports both sides as (lossy) text, so a crash
/// artefact reads like the log line it is rather than two byte arrays.
fn assert_bytes_eq(index: usize, what: &str, got: &[u8], want: &str) {
    assert!(
        got == want.as_bytes(),
        "RFC0015.1 / CLAUDE.md §3.3: record {index}: {what} must be byte-identical\n  rendered: {:?}\n  line:     {want:?}",
        String::from_utf8_lossy(got),
    );
}
