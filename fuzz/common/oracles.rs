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

use ourios_core::audit::{AuditEvent, AuditPayload, ParamType, TemplateChange};
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};
use ourios_miner::reconstruct::{Reconstruction, render};
use ourios_miner::tokenize::tokenize;
use ourios_miner::tree::{OwnedToken, format_template};

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

/// `template_id → (template_version, tokens)` for every leaf of one tenant.
pub type Leaves = HashMap<u64, (u32, Vec<OwnedToken>)>;

/// The tenant's leaves as they stand now, for [`AuditCtx`].
pub fn leaves(miner: &MinerCluster, tenant: &TenantId) -> Leaves {
    miner
        .templates_for(tenant)
        .into_iter()
        .map(|leaf| (leaf.template_id, (leaf.template_version, leaf.template)))
        .collect()
}

/// What one ingest did to the tenant's tree: its line number, the leaves
/// before and after it. [`AuditCtx::assert_changes_audited`] checks the audit
/// events the ingest emitted against it.
pub struct AuditCtx<'a> {
    pub index: usize,
    pub before: &'a Leaves,
    pub after: &'a Leaves,
}

impl AuditCtx<'_> {
    /// RFC0015.1 — `CLAUDE.md` §3.1 "every merge emits an audit event with
    /// old + new templates" (RFC 0001 H1.3), checked against what one ingest
    /// actually did to the tree, given everything the audit sink received
    /// during it:
    ///
    /// - a new leaf has exactly one `Created` event carrying its tokens;
    /// - a leaf whose version rose from `v` to `w` has exactly the chain of
    ///   `Widened` / `TypeExpanded` events `v → v+1 → … → w`, each link's
    ///   templates continuing the previous one, starting at the tokens before
    ///   the ingest and ending at the tokens after it;
    /// - every link of that chain is a legal step on its own (RFC0015.10,
    ///   [`LeafAudit::assert_step_shape`]), so an intermediate version that
    ///   no row was stamped with is checked too;
    /// - no other leaf has any template event (a degenerate-widening
    ///   rejection, which changes nothing, is the exception).
    ///
    /// # Panics
    ///
    /// On any violation, with a message naming the invariant.
    pub fn assert_changes_audited(&self, events: Vec<AuditEvent>) {
        let mut by_id = template_changes_by_id(events);
        for (&id, (version, tokens)) in self.after {
            let changes = by_id.remove(&id).unwrap_or_default();
            let leaf = LeafAudit {
                index: self.index,
                id,
                after: (*version, tokens),
                changes: &changes,
            };
            leaf.assert_audited(self.before.get(&id));
        }
        let orphans: Vec<u64> = by_id.into_keys().collect();
        assert!(
            orphans.is_empty(),
            "RFC0015.1 / CLAUDE.md §3.1: line {}: template events for ids {orphans:?}, which have no leaf",
            self.index,
        );
    }
}

/// The template events of one ingest, per template id, in emission order.
/// Degenerate-widening rejections change no template and are dropped.
fn template_changes_by_id(events: Vec<AuditEvent>) -> HashMap<u64, Vec<TemplateChange>> {
    let mut by_id: HashMap<u64, Vec<TemplateChange>> = HashMap::new();
    for (id, change) in events.into_iter().filter_map(template_change) {
        by_id.entry(id).or_default().push(change);
    }
    by_id
}

fn template_change(event: AuditEvent) -> Option<(u64, TemplateChange)> {
    match event.payload {
        AuditPayload::Template {
            change: TemplateChange::RejectedDegenerate { .. },
            ..
        } => None,
        AuditPayload::Template {
            template_id,
            change,
            ..
        } => Some((template_id, change)),
        _ => None,
    }
}

/// One leaf after one ingest, with the template events the ingest emitted
/// for it.
struct LeafAudit<'a> {
    index: usize,
    id: u64,
    after: Version<'a>,
    changes: &'a [TemplateChange],
}

impl LeafAudit<'_> {
    fn assert_audited(&self, before: Option<&(u32, Vec<OwnedToken>)>) {
        match before {
            None => self.assert_created(),
            Some((version, tokens)) => self.assert_steps((*version, tokens)),
        }
    }

    fn assert_created(&self) {
        let (version, tokens) = self.after;
        let expected = format_template(tokens);
        let audited = matches!(
            self.changes,
            [TemplateChange::Created { new_template }] if *new_template == expected
        );
        assert!(
            version == 1 && audited,
            "RFC0015.1 / CLAUDE.md §3.1: line {}: new template {} (v{version}, {expected:?}) must be \
             audited by exactly one `Created` event with its tokens; got {:?}",
            self.index,
            self.id,
            self.changes,
        );
    }

    fn assert_steps(&self, (old_version, old_tokens): Version<'_>) {
        let mut version = old_version;
        let mut template = format_template(old_tokens);
        let mut typed = old_tokens.to_vec();
        for change in self.changes {
            let step = version_step(change).unwrap_or_else(|| {
                panic!(
                    "RFC0015.1 / CLAUDE.md §3.1: line {}: template {} already existed but got {change:?}",
                    self.index, self.id,
                )
            });
            assert!(
                step.0 == version && step.1 == version + 1 && step.2 == template,
                "RFC0015.1 / CLAUDE.md §3.1: line {}: template {} event {change:?} does not continue \
                 from v{version} {template:?}",
                self.index,
                self.id,
            );
            self.assert_step_shape(change);
            self.replay_typed_step(&mut typed, change);
            version = step.1;
            step.3.clone_into(&mut template);
        }
        let (new_version, new_tokens) = self.after;
        assert!(
            typed == new_tokens,
            "RFC0015.10 / CLAUDE.md §3.1: line {}: template {}: replaying the declared widenings on the \
             leaf before the ingest gives {typed:?}, but the leaf after it is {new_tokens:?} — a token \
             changed that no audit event declares",
            self.index,
            self.id,
        );
        let expected = format_template(new_tokens);
        assert!(
            version == new_version && template == expected,
            "RFC0015.1 / CLAUDE.md §3.1: line {}: template {} moved v{old_version}→v{new_version} \
             ({expected:?}) but its audit events only account for v{old_version}→v{version} \
             ({template:?}) — a merge without its audit event",
            self.index,
            self.id,
        );
    }

    /// RFC0015.10 on typed tokens. The events carry templates only as
    /// canonical strings, where a literal `<*>` token and a wildcard print
    /// the same, but the chain's ends are typed leaves. So the typed template
    /// is carried through the chain: from the leaf before the ingest, each
    /// `Widened` step turns exactly its declared positions (strictly
    /// increasing, in range, each a literal until then) into wildcards, each
    /// `TypeExpanded` step changes nothing, and every intermediate must print
    /// as the event says. [`Self::assert_steps`] then requires the result to
    /// equal the typed leaf after the ingest, so a position widened without
    /// being declared, a literal `<*>` included, cannot pass.
    fn replay_typed_step(&self, typed: &mut [OwnedToken], change: &TemplateChange) {
        let (positions, new_template): (&[u16], &str) = match change {
            TemplateChange::Widened {
                positions_widened,
                new_template,
                ..
            } => (positions_widened, new_template),
            TemplateChange::TypeExpanded { new_template, .. } => (&[], new_template),
            _ => return,
        };
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "RFC0015.10 / CLAUDE.md §3.1: line {}: template {} declares positions_widened {positions:?}, \
             which is not strictly increasing",
            self.index,
            self.id,
        );
        for &position in positions {
            self.widen_typed(typed, usize::from(position));
        }
        let printed = format_template(typed);
        assert_eq!(
            printed, new_template,
            "RFC0015.10 / CLAUDE.md §3.1: line {}: template {} event {change:?} does not match its declared \
             positions applied to the typed template",
            self.index, self.id,
        );
    }

    fn widen_typed(&self, typed: &mut [OwnedToken], position: usize) {
        let token = typed.get_mut(position).unwrap_or_else(|| {
            panic!(
                "RFC0015.10 / CLAUDE.md §3.1: line {}: template {} declares widened position {position} \
                 past its end",
                self.index, self.id,
            )
        });
        assert!(
            matches!(token, OwnedToken::Fixed(_)),
            "RFC0015.10 / CLAUDE.md §3.1: line {}: template {} declares position {position} widened, \
             but it was already a wildcard",
            self.index,
            self.id,
        );
        *token = OwnedToken::Wildcard;
    }

    /// RFC0015.10 — `CLAUDE.md` §3.1 for every link of the chain, including
    /// versions no emitted row was stamped with: a widening turns exactly its
    /// declared positions into wildcards and changes nothing else; a type
    /// expansion changes no token at all.
    fn assert_step_shape(&self, change: &TemplateChange) {
        match change {
            TemplateChange::Widened {
                old_template,
                new_template,
                positions_widened,
                ..
            } => self.assert_widened_shape(old_template, new_template, positions_widened),
            TemplateChange::TypeExpanded {
                old_template,
                new_template,
                ..
            } => assert_eq!(
                old_template, new_template,
                "RFC0015.10 / CLAUDE.md §3.1: line {}: template {} type expansion changed its tokens",
                self.index, self.id,
            ),
            _ => {}
        }
    }

    /// Compared on the canonical strings token by token, not on
    /// `parse_template`: a literal `<*>` token is legal input and would parse
    /// as a wildcard.
    fn assert_widened_shape(&self, old: &str, new: &str, positions: &[u16]) {
        let old_tokens: Vec<&str> = old.split(' ').collect();
        let new_tokens: Vec<&str> = new.split(' ').collect();
        assert_eq!(
            old_tokens.len(),
            new_tokens.len(),
            "RFC0015.10 / CLAUDE.md §3.1: line {}: template {} widening {old:?} → {new:?} changed length",
            self.index,
            self.id,
        );
        let declared: Vec<usize> = positions.iter().map(|&p| usize::from(p)).collect();
        assert!(
            !declared.is_empty() && declared.iter().all(|&p| new_tokens.get(p) == Some(&"<*>")),
            "RFC0015.10 / CLAUDE.md §3.1: line {}: template {} widening {old:?} → {new:?} declares \
             positions {positions:?} that are not all wildcards afterwards",
            self.index,
            self.id,
        );
        let changed: Vec<usize> = old_tokens
            .iter()
            .zip(&new_tokens)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(position, _)| position)
            .collect();
        assert!(
            changed.iter().all(|p| declared.contains(p)),
            "RFC0015.10 / CLAUDE.md §3.1: line {}: template {} widening {old:?} → {new:?} changed \
             positions {changed:?}, outside its declared {positions:?} — a merge that rewrites a literal",
            self.index,
            self.id,
        );
    }
}

/// `(old_version, new_version, old_template, new_template)` of a change that
/// moves an existing template forward one version.
fn version_step(change: &TemplateChange) -> Option<(u32, u32, &str, &str)> {
    match change {
        TemplateChange::Widened {
            old_version,
            new_version,
            old_template,
            new_template,
            ..
        }
        | TemplateChange::TypeExpanded {
            old_version,
            new_version,
            old_template,
            new_template,
            ..
        } => Some((*old_version, *new_version, old_template, new_template)),
        _ => None,
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
