//! The §3.1 / §3.3 oracles the miner target asserts, shared as a source file
//! (each target pulls it in with `#[path]`, like `telemetry.rs`) so the fuzz
//! package keeps no library API (RFC 0015 §3.1).
//!
//! Ported from the RFC 0024 property suite's `assert_no_silent_merge`
//! (`crates/ourios-miner/tests/it/rfc0024_properties.rs`), with one change:
//! templates are looked up by `(template_id, template_version)`, the read
//! path's registry key (RFC 0017 §3.5). A property batch rarely widens a
//! template, so rendering every row against the leaf's *latest* tokens
//! passes there; a fuzzer widens within its first few inputs, and a row
//! stamped version N must render against the N-version tokens.

#![deny(unsafe_code)]

use std::collections::HashMap;

use ourios_core::audit::ParamType;
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};
use ourios_miner::reconstruct::{Reconstruction, render};
use ourios_miner::tokenize::tokenize;
use ourios_miner::tree::OwnedToken;

/// `(template_id, template_version) → tokens`, snapshotted from the live
/// tree right after the ingest that stamped the version (the in-memory
/// truth, with no detour through the audit stream's template strings).
pub type Templates = HashMap<(u64, u32), Vec<OwnedToken>>;

/// Record the tokens of the leaf `record` was stamped with, unless that
/// `(template_id, template_version)` is already known. Must run right after
/// the ingest that emitted `record`: a later widening bumps the leaf past
/// the record's version, and the older tokens are gone from the tree.
///
/// # Panics
///
/// When the tenant exposes no leaf at the record's key — the miner stamped a
/// `(template_id, template_version)` it never exposed, so no reader could
/// ever render the row.
pub fn snapshot_leaf(
    miner: &MinerCluster,
    tenant: &TenantId,
    record: &MinedRecord,
    templates: &mut Templates,
) {
    let key = (record.template_id, record.template_version);
    if templates.contains_key(&key) {
        return;
    }
    let leaf = miner
        .templates_for(tenant)
        .into_iter()
        .find(|leaf| (leaf.template_id, leaf.template_version) == key)
        .unwrap_or_else(|| {
            panic!("RFC0015.1 / RFC 0017 §3.5: record stamped {key:?} but no leaf has that id + version")
        });
    templates.insert(key, leaf.template);
}

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

/// RFC0015.10 — `CLAUDE.md` §3.1: a line attached to a template agrees with
/// that template's literal tokens position by position; only wildcard slots
/// vary. Two lines on one template therefore agree everywhere outside the
/// wildcards, which is what a merge across a semantic boundary breaks.
///
/// # Panics
///
/// On any violation, with a message naming the invariant.
pub fn assert_no_silent_merge(
    index: usize,
    line: &str,
    record: &MinedRecord,
    templates: &Templates,
) {
    let key = (record.template_id, record.template_version);
    let template = template_of(index, record, templates);
    let tokens = match tokenize(line) {
        Ok(tokenized) => tokenized.tokens,
        Err(e) => panic!(
            "RFC0015.10 / CLAUDE.md §3.1: record {index}: a line the tokenizer rejects ({e}) \
             was attached to template {key:?}"
        ),
    };
    assert_eq!(
        tokens.len(),
        template.len(),
        "RFC0015.10 / CLAUDE.md §3.1: record {index}: {} tokens attached to the {}-token template {key:?}",
        tokens.len(),
        template.len(),
    );
    for (position, (token, slot)) in tokens.iter().zip(template).enumerate() {
        if let OwnedToken::Fixed(literal) = slot {
            assert_eq!(
                token, literal,
                "RFC0015.10 / CLAUDE.md §3.1: record {index}: token {position} differs from the literal \
                 of template {key:?} — a line attached to a template whose literals it does not share",
            );
        }
    }
}

/// RFC0015.10 — `CLAUDE.md` §3.1 across versions: a widening only ever turns
/// a literal into a wildcard. A literal that changes into a different literal,
/// a wildcard that narrows back, or a template that changes length between
/// versions would silently re-key every row already stamped with the older
/// version.
///
/// # Panics
///
/// On any violation, with a message naming the invariant.
pub fn assert_widening_is_monotone(templates: &Templates) {
    let mut by_id: HashMap<u64, Vec<(u32, &[OwnedToken])>> = HashMap::new();
    for (&(id, version), tokens) in templates {
        by_id.entry(id).or_default().push((version, tokens));
    }
    for (id, mut versions) in by_id {
        versions.sort_by_key(|&(version, _)| version);
        for (&(old_version, older), &(new_version, newer)) in
            versions.iter().zip(versions.iter().skip(1))
        {
            assert_eq!(
                older.len(),
                newer.len(),
                "RFC0015.10 / CLAUDE.md §3.1: template {id} changed length from v{old_version} to v{new_version}",
            );
            for (position, (before, after)) in older.iter().zip(newer).enumerate() {
                match (before, after) {
                    (OwnedToken::Fixed(a), OwnedToken::Fixed(b)) if a != b => panic!(
                        "RFC0015.10 / CLAUDE.md §3.1: template {id} v{old_version}→v{new_version} replaced \
                         literal {a:?} with {b:?} at token {position} — a merge that rewrites a literal"
                    ),
                    (OwnedToken::Wildcard, OwnedToken::Fixed(b)) => panic!(
                        "RFC0015.10 / CLAUDE.md §3.1: template {id} v{old_version}→v{new_version} narrowed \
                         the wildcard at token {position} back to literal {b:?}"
                    ),
                    _ => {}
                }
            }
        }
    }
}

fn template_of<'a>(
    index: usize,
    record: &MinedRecord,
    templates: &'a Templates,
) -> &'a [OwnedToken] {
    let key = (record.template_id, record.template_version);
    templates.get(&key).map_or_else(
        || panic!("RFC0015.1: record {index}: no snapshot for {key:?} — snapshot_leaf must run after every ingest"),
        Vec::as_slice,
    )
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
