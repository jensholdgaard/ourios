//! The per-row §3.1 no-silent-merge oracles for the miner target, ported
//! from the RFC 0024 property suite's `assert_no_silent_merge`
//! (`crates/ourios-miner/tests/it/rfc0024_properties.rs`) and keyed by
//! template version (`snapshots.rs`). Shared as a source file (each target
//! pulls it in with `#[path]`, like `telemetry.rs`) so the fuzz package keeps
//! no library API (RFC 0015 §3.1).

#![deny(unsafe_code)]

use std::collections::HashMap;

use ourios_core::record::MinedRecord;
use ourios_miner::tokenize::tokenize;
use ourios_miner::tree::OwnedToken;

use crate::snapshots::{Templates, template_of};

/// RFC0015.10 — `AGENTS.md` §3.1: a line attached to a template agrees with
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
            "RFC0015.10 / AGENTS.md §3.1: record {index}: a line the tokenizer rejects ({e}) \
             was attached to template {key:?}"
        ),
    };
    assert_eq!(
        tokens.len(),
        template.len(),
        "RFC0015.10 / AGENTS.md §3.1: record {index}: {} tokens attached to the {}-token template {key:?}",
        tokens.len(),
        template.len(),
    );
    for (position, (token, slot)) in tokens.iter().zip(template).enumerate() {
        if let OwnedToken::Fixed(literal) = slot {
            assert_eq!(
                token, literal,
                "RFC0015.10 / AGENTS.md §3.1: record {index}: token {position} differs from the literal \
                 of template {key:?} — a line attached to a template whose literals it does not share",
            );
        }
    }
}

/// RFC0015.10 — `AGENTS.md` §3.1 across versions: a widening only ever turns
/// a literal into a wildcard. A literal that changes into a different literal,
/// a wildcard that narrows back, or a template that changes length between
/// versions would silently re-key every row already stamped with the older
/// version.
///
/// # Panics
///
/// On any violation, with a message naming the invariant.
pub fn assert_widening_is_monotone(templates: &Templates) {
    let mut by_id: HashMap<u64, Vec<Version<'_>>> = HashMap::new();
    for (&(id, version), tokens) in templates {
        by_id.entry(id).or_default().push((version, tokens));
    }
    for (id, mut versions) in by_id {
        versions.sort_by_key(|&(version, _)| version);
        for pair in versions.windows(2) {
            assert_version_step(id, pair[0], pair[1]);
        }
    }
}

/// One version of a template: `(template_version, tokens)`.
type Version<'a> = (u32, &'a [OwnedToken]);

fn assert_version_step(
    id: u64,
    (old_version, older): Version<'_>,
    (new_version, newer): Version<'_>,
) {
    assert_eq!(
        older.len(),
        newer.len(),
        "RFC0015.10 / AGENTS.md §3.1: template {id} changed length from v{old_version} to v{new_version}",
    );
    for (position, (before, after)) in older.iter().zip(newer).enumerate() {
        match (before, after) {
            (OwnedToken::Fixed(a), OwnedToken::Fixed(b)) if a != b => panic!(
                "RFC0015.10 / AGENTS.md §3.1: template {id} v{old_version}→v{new_version} replaced \
                 literal {a:?} with {b:?} at token {position} — a merge that rewrites a literal"
            ),
            (OwnedToken::Wildcard, OwnedToken::Fixed(b)) => panic!(
                "RFC0015.10 / AGENTS.md §3.1: template {id} v{old_version}→v{new_version} narrowed \
                 the wildcard at token {position} back to literal {b:?}"
            ),
            _ => {}
        }
    }
}
