//! Per-tenant template cluster.
//!
//! Holds one [`TenantState`] per [`TenantId`] (`[CLAUDE.md §3.7]`):
//! every ingested record is keyed on its tenant, and per-tenant
//! template *stores* are isolated — no template ever crosses
//! tenants. The `template_id` allocator, by contrast, is
//! **cluster-wide** so the same `u64` value never refers to two
//! different leaves (RFC 0001 §6.1, §5 §3.7.2); each tenant
//! sees a monotonic *subsequence* of the shared id space.
//!
//! # The widen step
//!
//! As of this PR the cluster implements RFC 0001 §6.2 step 4
//! (best-candidate selection) and step 5 (widen). The decision
//! tree on `Body::String` records:
//!
//! - **No candidate** in the `(severity, scope, length, prefix)`
//!   bucket → fresh leaf, no audit event (RFC0001.1).
//! - **Best candidate has `sim_seq == 1.0`** → clean attach to the
//!   existing leaf, no widening, no audit.
//! - **Best candidate has `threshold ≤ sim_seq < 1.0`** → compute
//!   the set of mismatched Fixed positions. If the proposed
//!   widening would leave zero Fixed tokens (`RFC0001.2`
//!   degenerate guard), emit `TemplateWideningRejectedDegenerate`,
//!   increment `parse_failures_total`, return [`NO_TEMPLATE`].
//!   Otherwise apply the widening, bump the leaf's
//!   `template_version`, emit `TemplateWidened`, increment
//!   `merges_total`, return the leaf's `template_id`.
//! - **Best candidate has `sim_seq < threshold`** → fresh leaf in
//!   the same bucket. The three-zone confidence model (lossy-zone
//!   body retention + parse-failure floor per RFC §6.3) is the
//!   next PR; today this branch behaves exactly as the pre-widen
//!   one did.
//!
//! Audit events flow to the [`AuditSink`] the cluster was
//! constructed with — [`MinerCluster::new`] defaults to an
//! [`ourios_core::audit::InMemoryAuditSink`] (events accumulate
//! and are unobservable from outside the cluster), and tests use
//! [`MinerCluster::with_audit_sink`] with a
//! [`ourios_core::audit::SharedAuditSink`] to inspect emissions.
//! The eventual WAL-backed sink replaces the in-memory placeholder
//! with the RFC §6.4 *ordering-plus-durability-barrier* contract.
//!
//! # Not yet emitted
//!
//! - Three-zone confidence + lossy-zone body retention (RFC §6.3,
//!   `H1.2`).
//! - Type-expansion (`TemplateTypeExpanded`); the variant exists
//!   on [`AuditEventType`] but no widening path emits it yet
//!   (`H5.2`).
//! - `reconstruct()` / `lossy_flag` semantics (RFC §6.6).
//! - Per-parameter 256 B overflow + `OVERFLOW` marker (RFC §6.5).
//! - Prometheus exposition for `merges_total` /
//!   `parse_failures_total` / `template_count` (RFC §6.8).
//! - Parquet records for the data records themselves (those go
//!   through `ourios-parquet`).
//!
//! [`Tree`]: crate::tree::Tree
//! [`AuditSink`]: ourios_core::audit::AuditSink
//! [`AuditEventType`]: ourios_core::audit::AuditEventType

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use ourios_core::audit::{
    AuditEvent, AuditEventType, AuditSink, InMemoryAuditSink, hash_triggering_line,
    sample_first_256_bytes,
};
use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::tenant::TenantId;

use crate::mask::mask;
use crate::sim_seq::sim_seq;
use crate::tokenize::tokenize;
use crate::tree::{DEFAULT_PREFIX_DEPTH, Leaf, OwnedToken, Tree};
// `DEFAULT_PREFIX_DEPTH` is used as the prefix-depth field's
// default — see `MinerCluster::with_audit_sink`.

/// Sentinel `template_id` returned by [`MinerCluster::ingest`] when
/// no template was allocated for the input. Three paths reach this
/// today:
///
/// - `Body::None` — the wire delivered no body.
/// - `Body::String("")` (or whitespace-only) — `tokenize` yields
///   zero tokens; placeholder for `parse_failures_total` once the
///   §6.8 telemetry surface lands.
/// - A widening was rejected by the §6.4 degenerate-template
///   guard. The audit-event stream records the rejection and the
///   line is treated as a parse failure
///   (`parse_failures_total` increments).
///
/// Real templates always have id `>= 1` (see `next_template_id`
/// initialisation).
pub const NO_TEMPLATE: u64 = 0;

/// A multi-tenant in-memory miner.
///
/// Holds one [`TenantState`] per [`TenantId`]; per-tenant state
/// is allocated lazily on the first `ingest` call for that
/// tenant. Tenant deprovisioning (`TenantPaused`,
/// `TenantDeleted`) is RFC 0001 §9 territory and not in this
/// type's API yet.
pub struct MinerCluster {
    config: MinerConfig,
    tenants: HashMap<TenantId, TenantState>,
    // Cluster-wide template_id allocator. RFC 0001 §6.1 calls
    // template_id "per-tenant monotonic" but also requires that
    // "two tenants emitting the structurally identical template
    // will have different template_ids" (and §5 §3.7.2: "no
    // template_id is shared across tenants"). A truly per-tenant
    // allocator gives both tenants id=1 for their first template
    // and silently violates §3.7.2. The reconciliation: the id
    // *space* is cluster-wide, but each tenant's slice of that
    // space is monotonic with respect to that tenant's allocation
    // order — both invariants hold.
    next_template_id: u64,
    // Audit-event sink per RFC §6.4. Boxed trait object so the
    // WAL sink (post-`ourios-wal`) drops in by swapping the impl;
    // the trait is `Send` so the cluster stays moveable across
    // threads.
    audit_sink: Box<dyn AuditSink>,
    // §6.4 counter: every emitted `TemplateWidened` or
    // `TemplateTypeExpanded` event increments this; rejection
    // events do not. Atomic so the §6.8 Prometheus exposer (a
    // future PR) can read without taking a lock on the cluster.
    merges_total: AtomicU64,
    // Placeholder for the §6.8 `parse_failures_total` counter.
    // Currently only the degenerate-rejection branch and the
    // empty-input branch increment it; the three-zone parse-
    // failure floor (RFC §6.3) is the next PR.
    parse_failures_total: AtomicU64,
    // Drain prefix-tree depth (RFC §6.2 step 3 / `tree.rs`'s
    // *Depth convention*). The default
    // [`crate::tree::DEFAULT_PREFIX_DEPTH`] (= 2) matches Drain3's
    // `depth = 4` semantics. Exposed as a builder knob via
    // [`Self::with_prefix_depth`] because: (a) the value is a
    // real tuning question once corpus measurements arrive and
    // (b) some §5 scenarios that the cluster must defend — chief
    // among them RFC0001.2's degenerate-template guard — are
    // only reachable when leaves can accumulate under a shared
    // parent, which on the default tree shape requires sharing a
    // 2-token prefix. Tests that don't want that constraint set
    // depth to 0.
    prefix_depth: usize,
}

/// Per-tenant template store.
///
/// Private: the cross-tenant API surface lives on
/// [`MinerCluster`]; per-tenant access goes through the cluster
/// helpers below. `tree` is the Drain prefix tree for the
/// `Body::String` branch; leaves carry both literal tokens and
/// (post-widening) [`OwnedToken::Wildcard`] positions.
///
/// `structured_templates` is the §6.2 step-0 short-circuit map
/// for `Body::Structured` records: each
/// `(severity_number, scope_name)` tuple shares one `template_id`
/// per RFC 0001 §6.1 *Template-key composition* (the
/// `BodyKind::Structured` discriminator is implicit from the map
/// itself). The map's value is the `template_id` allocated on
/// first observation of that tuple.
///
/// `template_id` allocation lives on [`MinerCluster`], not here
/// — see the `next_template_id` comment there for why.
///
/// `template_count` is a cache of the number of templates this
/// tenant holds (tree leaves + structured-template entries),
/// incremented on every fresh allocation in
/// [`MinerCluster::ingest`]. The cache invariant is: every fresh
/// `template_id` allocation on this tenant (whether tree leaf or
/// structured map insert) increments the cache by exactly one.
/// Widening reuses an existing leaf's id, so it does not bump the
/// cache.
struct TenantState {
    tree: Tree,
    structured_templates: HashMap<(u8, Option<String>), u64>,
    template_count: usize,
}

impl TenantState {
    fn new() -> Self {
        Self {
            tree: Tree::new(),
            structured_templates: HashMap::new(),
            template_count: 0,
        }
    }
}

impl MinerCluster {
    /// Build an empty cluster with an unobservable in-memory
    /// audit sink — events accumulate but cannot be drained from
    /// outside the cluster. Suitable for production until the WAL
    /// sink lands; tests that need to inspect audit emissions use
    /// [`Self::with_audit_sink`] with a
    /// [`ourios_core::audit::SharedAuditSink`].
    #[must_use]
    pub fn new(config: MinerConfig) -> Self {
        Self::with_audit_sink(config, Box::new(InMemoryAuditSink::new()))
    }

    /// Build an empty cluster whose audit events flow to `sink`.
    /// The cluster takes ownership; observers that need to read
    /// the sink afterwards should clone a
    /// [`ourios_core::audit::SharedAuditSink`] before handing it
    /// in (the `Arc<Mutex<_>>` shape on that type is exactly the
    /// observer-friendly handle).
    #[must_use]
    pub fn with_audit_sink(config: MinerConfig, sink: Box<dyn AuditSink>) -> Self {
        Self {
            config,
            tenants: HashMap::new(),
            // Start at 1 so 0 stays available as the [`NO_TEMPLATE`]
            // sentinel.
            next_template_id: 1,
            audit_sink: sink,
            merges_total: AtomicU64::new(0),
            parse_failures_total: AtomicU64::new(0),
            prefix_depth: DEFAULT_PREFIX_DEPTH,
        }
    }

    /// Set the Drain prefix-tree depth used by this cluster.
    /// Default [`crate::tree::DEFAULT_PREFIX_DEPTH`] (= 2). Tests
    /// or tuning experiments that want every length-N line in the
    /// same leaf list pass `0`; production should leave the
    /// default until corpus measurements justify otherwise.
    #[must_use]
    pub fn with_prefix_depth(mut self, depth: usize) -> Self {
        self.prefix_depth = depth;
        self
    }

    /// Borrow the cluster's [`MinerConfig`].
    ///
    /// All tenants currently share one config; per-tenant
    /// overrides are a future PR.
    #[must_use]
    pub fn config(&self) -> &MinerConfig {
        &self.config
    }

    /// Cumulative count of widening + type-expansion events
    /// emitted across all tenants. RFC §6.4: rejected-degenerate
    /// events do not increment this counter. Placeholder for the
    /// §6.8 Prometheus gauge.
    #[must_use]
    pub fn merges_total(&self) -> u64 {
        self.merges_total.load(Ordering::Relaxed)
    }

    /// Cumulative count of lines that produced no template
    /// because they tripped the degenerate-template guard (RFC
    /// §6.4) or yielded zero tokens (empty / whitespace-only
    /// `Body::String`). Placeholder for the §6.8 Prometheus
    /// gauge; the full three-zone parse-failure floor lands in
    /// the next PR.
    #[must_use]
    pub fn parse_failures_total(&self) -> u64 {
        self.parse_failures_total.load(Ordering::Relaxed)
    }

    /// Ingest a structured OTLP log record. Returns the
    /// `template_id` allocated (or reused) for the record's
    /// §6.1 *Template-key composition* tuple, or [`NO_TEMPLATE`]
    /// (`0`) for the parse-failure paths described in
    /// [`NO_TEMPLATE`].
    ///
    /// The body fork follows RFC 0001 §6.2 step 0:
    ///
    /// - `Body::String(s)` — tokenize/mask/descend the prefix tree,
    ///   then run §6.2 steps 4–5 (best-candidate selection +
    ///   widen). Widening emits a `TemplateWidened` audit event
    ///   and bumps the leaf's `template_version`; the
    ///   degenerate-template guard (§6.4) rejects fully-wildcard
    ///   widenings and routes the line to the parse-failure
    ///   path. Clean attaches (sim == 1.0) emit no audit event.
    /// - `Body::Structured(_)` — short-circuit. The `AnyValue`
    ///   tree is **not** walked; the template id is keyed on
    ///   `(severity_number, scope_name, BodyKind::Structured)`
    ///   per §6.1, and the same tuple reuses the same id on
    ///   subsequent records. Structured records never widen and
    ///   never emit audit events.
    /// - `None` — the wire delivered no body. Returns
    ///   [`NO_TEMPLATE`]; no allocation, no audit.
    ///
    /// On first sight of `record.tenant_id`, allocates a fresh
    /// per-tenant store.
    pub fn ingest(&mut self, record: &OtlpLogRecord) -> u64 {
        match &record.body {
            None => NO_TEMPLATE,
            Some(Body::String(raw)) => self.ingest_string(record, raw),
            Some(Body::Structured(_)) => self.ingest_structured(record),
        }
    }

    /// `Body::String` path — RFC §6.2 steps 1–5 with widening.
    fn ingest_string(&mut self, record: &OtlpLogRecord, raw: &str) -> u64 {
        let tokenized = tokenize(raw);
        let masked = mask(&tokenized.tokens);
        let masked_strs: Vec<&str> = masked.tokens.into_iter().collect();

        if masked_strs.is_empty() {
            // Empty input is the §6.3 parse-failure floor's
            // simplest case; counting it here matches the future
            // §6.3 branch's contract (parse_failures_total
            // counts every line that produces no template).
            self.parse_failures_total.fetch_add(1, Ordering::Relaxed);
            return NO_TEMPLATE;
        }

        // Phase 1 — read-only candidate selection. RFC §6.2 step
        // 4: among leaves in the same `(severity, scope, length,
        // prefix)` bucket, pick `argmax sim_seq`. The walk is
        // immutable so we can early-return (or fall through to
        // fresh-leaf creation) without committing a
        // `template_id` allocation.
        let best = self.find_best_candidate(record, &masked_strs);

        let threshold = self.config.similarity_threshold;

        match best {
            // No candidate at all → fresh leaf.
            None => self.create_new_leaf(record, &masked_strs),
            // Best candidate is below threshold → fresh leaf in
            // the same bucket. The three-zone lossy-floor logic
            // (RFC §6.3) is the next PR; for now this branch
            // behaves exactly as the pre-widen one did.
            Some(c) if c.similarity < threshold => self.create_new_leaf(record, &masked_strs),
            // Above threshold → attach (maybe widen).
            Some(c) => self.attach_and_maybe_widen(record, raw, &masked_strs, c),
        }
    }

    /// RFC §6.2 step 4 — find the best-matching leaf in the
    /// `(severity, scope, length, prefix)` bucket, or `None` if
    /// the tenant is unseen, the prefix path doesn't exist, or
    /// the leaf list is empty (after filtering).
    fn find_best_candidate(
        &self,
        record: &OtlpLogRecord,
        masked_strs: &[&str],
    ) -> Option<Candidate> {
        let state = self.tenants.get(&record.tenant_id)?;
        let parent = state.tree.descend(masked_strs, self.prefix_depth)?;

        let mut best: Option<Candidate> = None;
        for (leaf_idx, leaf) in parent.leaves.iter().enumerate() {
            // Filter by the non-token half of the template key
            // (RFC §6.1 *Template-key composition*). Length is a
            // structural property of the prefix bucket; we still
            // check it because `descend` walks at most
            // `self.prefix_depth` levels, so two lines of
            // different length CAN end up under the same parent
            // when both are shorter than the prefix depth — in
            // which case `sim_seq` would panic on mismatched
            // lengths.
            if leaf.severity_number != record.severity_number
                || leaf.scope_name.as_deref() != record.scope_name.as_deref()
                || leaf.template.len() != masked_strs.len()
            {
                continue;
            }
            let view: Vec<crate::sim_seq::Token<'_>> =
                leaf.template.iter().map(OwnedToken::as_borrowed).collect();
            let similarity = sim_seq(masked_strs, &view);
            let candidate = Candidate {
                leaf_idx,
                similarity,
            };
            best = match best {
                None => Some(candidate),
                Some(prev) if similarity > prev.similarity => Some(candidate),
                Some(prev) => Some(prev),
            };
        }
        best
    }

    /// RFC §6.2 step 4 (fresh-leaf branch). Allocates a new
    /// `template_id`, materialises the prefix path, pushes a
    /// fixed-only leaf. RFC0001.1: this path **does not** emit an
    /// audit event — `template_count` already reflects the
    /// allocation and `merges_total` is reserved for widening.
    fn create_new_leaf(&mut self, record: &OtlpLogRecord, masked_strs: &[&str]) -> u64 {
        let new_id = self.next_template_id;
        self.next_template_id += 1;

        let state = self
            .tenants
            .entry(record.tenant_id.clone())
            .or_insert_with(TenantState::new);
        let parent = state.tree.descend_mut(masked_strs, self.prefix_depth);
        let new_template: Vec<OwnedToken> = masked_strs
            .iter()
            .map(|s| OwnedToken::Fixed((*s).to_string()))
            .collect();
        parent.leaves.push(Leaf {
            template: new_template,
            template_id: new_id,
            template_version: 1,
            severity_number: record.severity_number,
            scope_name: record.scope_name.clone(),
        });
        // Maintain the TenantState::template_count cache invariant —
        // every fresh allocation under `state` is mirrored here so
        // `MinerCluster::template_count` can stay O(1).
        state.template_count += 1;
        new_id
    }

    /// RFC §6.2 step 5 — clean-or-widen attach to an existing
    /// leaf. The split:
    ///
    /// - No mismatched Fixed positions → clean attach, no
    ///   widening, no audit. Reuse the leaf's existing
    ///   `template_id` and `template_version`.
    /// - One or more mismatched Fixed positions:
    ///   - Run the §6.4 degenerate guard. If the proposed
    ///     widening would leave zero Fixed tokens, emit
    ///     `TemplateWideningRejectedDegenerate`, increment
    ///     `parse_failures_total`, return [`NO_TEMPLATE`].
    ///   - Else apply the widening (in place on the leaf), bump
    ///     `template_version`, emit `TemplateWidened`, increment
    ///     `merges_total`, return the leaf's existing
    ///     `template_id`.
    fn attach_and_maybe_widen(
        &mut self,
        record: &OtlpLogRecord,
        raw: &str,
        masked_strs: &[&str],
        candidate: Candidate,
    ) -> u64 {
        // Phase 2 — re-descend mutably to the chosen leaf.
        let state = self
            .tenants
            .get_mut(&record.tenant_id)
            .expect("tenant present: find_best_candidate returned Some(...)");
        let parent = state.tree.descend_mut(masked_strs, self.prefix_depth);
        let leaf = &mut parent.leaves[candidate.leaf_idx];

        let positions_widened = find_widening_positions(masked_strs, &leaf.template);

        if positions_widened.is_empty() {
            // Clean attach — no Fixed position mismatched. Reuse
            // the leaf as-is; no version bump, no audit event.
            return leaf.template_id;
        }

        // Degenerate guard (§6.4). Check *before* mutating: if the
        // proposed widening would leave zero Fixed tokens, reject.
        if would_be_degenerate(&leaf.template, &positions_widened) {
            let template_id = leaf.template_id;
            let template_version = leaf.template_version;
            let old_template = format_template(&leaf.template);
            // The would-be new template, computed without
            // mutating the leaf — needed for the audit payload so
            // an operator inspecting the rejection sees what was
            // proposed.
            let mut new_template_tokens = leaf.template.clone();
            apply_widening(&mut new_template_tokens, &positions_widened);
            let new_template = format_template(&new_template_tokens);

            self.audit_sink.emit(AuditEvent {
                event_type: AuditEventType::TemplateWideningRejectedDegenerate,
                tenant_id: record.tenant_id.clone(),
                template_id,
                // Version is unchanged on rejection; the audit
                // event records the *would-be* widening, so both
                // versions point at the same number.
                old_version: template_version,
                new_version: template_version,
                old_template,
                new_template,
                triggering_line_hash: hash_triggering_line(raw.as_bytes()),
                triggering_line_sample: Some(sample_first_256_bytes(raw)),
                positions_widened: positions_widened.clone(),
                slots_expanded: Vec::new(),
                timestamp: SystemTime::now(),
            });
            self.parse_failures_total.fetch_add(1, Ordering::Relaxed);
            return NO_TEMPLATE;
        }

        // Apply the widening. Snapshot the pre-widen state for
        // the audit payload first, then mutate, then emit.
        let template_id = leaf.template_id;
        let old_version = leaf.template_version;
        let old_template = format_template(&leaf.template);
        apply_widening(&mut leaf.template, &positions_widened);
        leaf.template_version = leaf
            .template_version
            .checked_add(1)
            .expect("template_version overflow: 2^32 widenings on one leaf is implausible");
        let new_version = leaf.template_version;
        let new_template = format_template(&leaf.template);

        self.audit_sink.emit(AuditEvent {
            event_type: AuditEventType::TemplateWidened,
            tenant_id: record.tenant_id.clone(),
            template_id,
            old_version,
            new_version,
            old_template,
            new_template,
            triggering_line_hash: hash_triggering_line(raw.as_bytes()),
            triggering_line_sample: Some(sample_first_256_bytes(raw)),
            positions_widened,
            slots_expanded: Vec::new(),
            timestamp: SystemTime::now(),
        });
        self.merges_total.fetch_add(1, Ordering::Relaxed);
        template_id
    }

    /// `Body::Structured` short-circuit per RFC 0001 §6.2 step 0.
    /// The tree is not walked; the per-tenant
    /// `(severity_number, scope_name) → template_id` map is the
    /// entire lookup. First observation of a tuple allocates;
    /// subsequent records with the same tuple reuse. Structured
    /// records never widen and never emit audit events.
    fn ingest_structured(&mut self, record: &OtlpLogRecord) -> u64 {
        let key = (record.severity_number, record.scope_name.clone());
        let state = self
            .tenants
            .entry(record.tenant_id.clone())
            .or_insert_with(TenantState::new);
        if let Some(&existing_id) = state.structured_templates.get(&key) {
            return existing_id;
        }
        let new_id = self.next_template_id;
        self.next_template_id += 1;
        state.structured_templates.insert(key, new_id);
        // Same cache invariant as create_new_leaf: one fresh
        // allocation, one cache increment.
        state.template_count += 1;
        new_id
    }

    /// Number of distinct templates this tenant has accumulated
    /// (tree leaves + structured-template entries). Returns 0 for
    /// a tenant the cluster has never seen.
    ///
    /// O(1): served from the [`TenantState::template_count`]
    /// cache rather than walking the tree.
    #[must_use]
    pub fn template_count(&self, tenant_id: &TenantId) -> usize {
        self.tenants.get(tenant_id).map_or(0, |s| s.template_count)
    }

    /// Snapshot of `(template_tokens, template_id)` pairs for one
    /// tenant's `Body::String` templates. Returns an empty vec
    /// for unseen tenants.
    ///
    /// Order is not guaranteed (`HashMap` iteration). Templates
    /// now contain [`OwnedToken::Wildcard`] positions post-widen
    /// — the return type is `Vec<(Vec<OwnedToken>, u64)>` (was
    /// `Vec<(Vec<String>, u64)>` pre-widen) so the wildcard
    /// vs. literal distinction stays typed; a `"<*>"` string
    /// sentinel would lose the round-trip guarantee.
    /// Structured-body templates (§6.2 step-0 short-circuit) are
    /// not returned by this helper — they have no token shape to
    /// surface.
    #[must_use]
    pub fn templates_for(&self, tenant_id: &TenantId) -> Vec<(Vec<OwnedToken>, u64)> {
        self.tenants.get(tenant_id).map_or_else(Vec::new, |s| {
            s.tree
                .collect_leaves()
                .into_iter()
                .map(|leaf| (leaf.template.clone(), leaf.template_id))
                .collect()
        })
    }
}

/// One leaf considered as the best match in RFC §6.2 step 4.
/// `leaf_idx` is the index into `parent.leaves`; the
/// `template_id` is read off the leaf in Phase 2 (no need to
/// duplicate it on the candidate).
#[derive(Debug, Clone, Copy)]
struct Candidate {
    leaf_idx: usize,
    similarity: f32,
}

/// Token positions where the leaf's template is `Fixed(_)` and
/// the candidate line has a different value. Per RFC §6.2 step
/// 5, these are exactly the positions that would become `<*>` if
/// the line attached to the leaf.
fn find_widening_positions(line: &[&str], template: &[OwnedToken]) -> Vec<u16> {
    debug_assert_eq!(line.len(), template.len());
    line.iter()
        .zip(template.iter())
        .enumerate()
        .filter_map(|(i, (l, t))| match t {
            OwnedToken::Fixed(s) if s.as_str() != *l => {
                u16::try_from(i).ok().or_else(|| {
                    // > u16::MAX tokens in a line is well past
                    // any realistic mask output. Drop the
                    // position rather than panic — the widening
                    // simply won't apply at that index, which
                    // is safe-but-conservative (we won't merge
                    // when we maybe could have, and the line
                    // creates a new leaf instead).
                    debug_assert!(false, "more than u16::MAX tokens in a line");
                    None
                })
            }
            _ => None,
        })
        .collect()
}

/// RFC §6.4 degenerate-template guard. Returns `true` iff
/// applying `positions_widened` to `template` would leave the
/// template with zero `OwnedToken::Fixed(_)` positions.
fn would_be_degenerate(template: &[OwnedToken], positions_widened: &[u16]) -> bool {
    template.iter().enumerate().all(|(i, tok)| match tok {
        OwnedToken::Wildcard => true,
        OwnedToken::Fixed(_) => {
            // Already-Fixed positions count toward degeneracy
            // only if this widening would replace them.
            u16::try_from(i).is_ok_and(|i_u16| positions_widened.contains(&i_u16))
        }
    })
}

/// Replace `Fixed` tokens at the given positions with
/// `Wildcard`, in place. Positions that are already `Wildcard`
/// are no-ops; positions not in the list are unchanged.
fn apply_widening(template: &mut [OwnedToken], positions: &[u16]) {
    for &pos in positions {
        let idx = pos as usize;
        if idx < template.len() {
            template[idx] = OwnedToken::Wildcard;
        }
    }
}

/// Canonical string form of a template: literal tokens for
/// `Fixed`, `<*>` for `Wildcard`, space-joined. RFC §6.4 calls
/// for this form in `old_template` / `new_template` audit
/// fields.
fn format_template(template: &[OwnedToken]) -> String {
    template
        .iter()
        .map(|t| match t {
            OwnedToken::Fixed(s) => s.as_str(),
            OwnedToken::Wildcard => "<*>",
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ourios_core::audit::SharedAuditSink;
    use ourios_core::otlp::{AnyValue, any_value::Value as AvValue};

    /// Test helper — a `Body::String` record for `tenant` carrying
    /// `text` and default severity (UNSPECIFIED) / scope (None).
    /// Mirrors the pre-amendment `cluster.ingest(&tenant, "text")`
    /// ergonomics so existing tests' assertions stay focused on
    /// what they're testing rather than on record-construction
    /// boilerplate.
    fn string_record(tenant: &TenantId, text: &str) -> OtlpLogRecord {
        OtlpLogRecord {
            tenant_id: tenant.clone(),
            body: Some(Body::String(text.to_string())),
            ..Default::default()
        }
    }

    /// Test helper — a `Body::Structured` record for `tenant` with
    /// the given severity and scope.
    fn structured_record(tenant: &TenantId, severity: u8, scope: Option<&str>) -> OtlpLogRecord {
        OtlpLogRecord {
            tenant_id: tenant.clone(),
            severity_number: severity,
            scope_name: scope.map(str::to_string),
            body: Some(Body::Structured(AnyValue {
                value: Some(AvValue::IntValue(0)),
            })),
            ..Default::default()
        }
    }

    /// Test helper — build a cluster wired to a [`SharedAuditSink`]
    /// and return both so the test can inspect emissions.
    fn cluster_with_observable_sink() -> (MinerCluster, SharedAuditSink) {
        let sink = SharedAuditSink::new();
        let cluster = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(sink.clone()));
        (cluster, sink)
    }

    // ---------- existing String-body behaviour preserved ----------

    #[test]
    fn ingest_returns_same_template_id_for_repeat_shape() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let id2 = cluster.ingest(&string_record(&t, "user 17 logged in"));

        // Both lines mask to "user <NUM> logged in" → exact
        // sim_seq match on the existing leaf, no widening, no
        // audit, same template_id.
        assert_eq!(id1, id2);
        assert_eq!(cluster.template_count(&t), 1);
        assert_eq!(cluster.merges_total(), 0);
    }

    #[test]
    fn ingest_returns_distinct_template_ids_for_distinct_shapes() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let id2 = cluster.ingest(&string_record(&t, "GET /home 200"));

        // Distinct masked shapes land in different `(length,
        // prefix)` buckets — no candidate selection happens at
        // all, both create fresh leaves.
        assert_ne!(id1, id2);
        assert_eq!(cluster.template_count(&t), 2);
        assert_eq!(cluster.merges_total(), 0);
    }

    #[test]
    fn template_count_is_zero_for_unseen_tenant() {
        let cluster = MinerCluster::new(MinerConfig::default());
        let unseen = TenantId::new("never-ingested");

        assert_eq!(cluster.template_count(&unseen), 0);
        assert!(cluster.templates_for(&unseen).is_empty());
    }

    #[test]
    fn ingest_lazily_allocates_per_tenant_state() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");
        assert_eq!(cluster.template_count(&t), 0);

        let _ = cluster.ingest(&string_record(&t, "hello world"));

        assert_eq!(cluster.template_count(&t), 1);
    }

    // ---------- widen behaviour (this PR's main story) ----------

    /// Replaces the pre-widen `ingest_creates_separate_leaves_for_
    /// near_match_under_same_parent` test, which locked in the
    /// no-widening contract. With `sim_seq >= threshold` widening
    /// now active, the two lines that previously created two
    /// distinct leaves collapse to a single template with a
    /// `<*>` at position 3.
    ///
    /// Per `CLAUDE.md` §6.2 ("Tests are specifications") this
    /// contract change is explicit, not silent: the old test
    /// asserted *distinct ids*, the new one asserts *same id +
    /// audit event*. PR review must acknowledge the swap.
    #[test]
    fn near_match_under_same_parent_widens_into_single_template() {
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        // Both lines mask to length-6 templates differing at
        // position 3 ("in" vs "out"). sim_seq = 5/6 ≈ 0.833 ≥
        // the default 0.7 threshold, so the second line widens
        // the first leaf rather than creating a new one.
        let id_in = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));
        let id_out = cluster.ingest(&string_record(&t, "user 42 logged out from 10.0.0.1"));

        // Same template, one widening event, count stays at 1.
        assert_eq!(id_in, id_out);
        assert_eq!(cluster.template_count(&t), 1);
        assert_eq!(cluster.merges_total(), 1);

        let events = sink.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, AuditEventType::TemplateWidened);
        assert_eq!(events[0].template_id, id_in);
        assert_eq!(events[0].old_version, 1);
        assert_eq!(events[0].new_version, 2);
        assert_eq!(events[0].positions_widened, vec![3]);
        assert_eq!(events[0].old_template, "user <NUM> logged in from <IP>");
        assert_eq!(events[0].new_template, "user <NUM> logged <*> from <IP>");
    }

    #[test]
    fn fresh_leaf_does_not_emit_audit_event() {
        // RFC0001.1 — leaf allocation is reflected in
        // `template_count`, but the audit stream is reserved for
        // widening events. Verifies both:
        //  - `merges_total` stays 0
        //  - the sink stays empty
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let _ = cluster.ingest(&string_record(&t, "GET /home 200"));

        assert_eq!(cluster.template_count(&t), 2);
        assert_eq!(cluster.merges_total(), 0);
        assert!(sink.is_empty());
    }

    #[test]
    fn exact_sim_seq_match_attaches_without_widening_or_audit() {
        // A line whose mask matches an existing leaf exactly (no
        // mismatched Fixed positions) reuses the leaf with no
        // widening, no version bump, no audit event.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let id2 = cluster.ingest(&string_record(&t, "user 17 logged in"));

        assert_eq!(id1, id2);
        assert_eq!(cluster.merges_total(), 0);
        assert!(sink.is_empty());

        // Template stays fixed-only (no `<*>` from widening).
        let templates = cluster.templates_for(&t);
        assert_eq!(templates.len(), 1);
        assert!(
            templates[0]
                .0
                .iter()
                .all(|t| matches!(t, OwnedToken::Fixed(_))),
            "no Wildcard tokens should be present without widening: {:?}",
            templates[0].0,
        );
    }

    #[test]
    fn widening_increments_template_version() {
        // H5.1 — the version stamp on the leaf bumps from 1 to 2.
        // The first ingest creates the leaf at version 1; the
        // second triggers a widening that bumps to version 2.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));
        let _ = cluster.ingest(&string_record(&t, "user 42 logged out from 10.0.0.1"));

        let events = sink.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, AuditEventType::TemplateWidened);
        assert_eq!(events[0].old_version, 1);
        assert_eq!(events[0].new_version, 2);
    }

    #[test]
    fn second_widening_at_different_position_increments_version_again() {
        // Three lines, two widening events. After the second
        // widening, version is 3.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        // L1: "user 42 alpha logged in from 10.0.0.1" — fresh leaf, v=1.
        // L2: "user 42 alpha logged out from 10.0.0.1" — widens
        //     position 4 ("in" → "out") to <*>, v=2.
        // L3: "user 42 alpha logged out to 10.0.0.1" — widens
        //     position 5 ("from" → "to") to <*>, v=3. The
        //     position-4 wildcard already matches "out", so only
        //     position 5 widens this round.
        let _ = cluster.ingest(&string_record(&t, "user 42 alpha logged in from 10.0.0.1"));
        let _ = cluster.ingest(&string_record(&t, "user 42 alpha logged out from 10.0.0.1"));
        let _ = cluster.ingest(&string_record(&t, "user 42 alpha logged out to 10.0.0.1"));

        assert_eq!(cluster.template_count(&t), 1);
        assert_eq!(cluster.merges_total(), 2);

        let events = sink.drain();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].old_version, 1);
        assert_eq!(events[0].new_version, 2);
        assert_eq!(events[0].positions_widened, vec![4]);
        assert_eq!(events[1].old_version, 2);
        assert_eq!(events[1].new_version, 3);
        assert_eq!(events[1].positions_widened, vec![5]);
    }

    #[test]
    fn audit_event_carries_triggering_line_hash_and_sample() {
        // RFC §6.4 fields: triggering_line_hash (truncated blake3)
        // and triggering_line_sample (first 256 B at char
        // boundary) must reflect the line that triggered the
        // widening — i.e., L2, not L1.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));
        let l2 = "user 42 logged out from 10.0.0.1";
        let _ = cluster.ingest(&string_record(&t, l2));

        let events = sink.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].triggering_line_hash,
            hash_triggering_line(l2.as_bytes()),
        );
        assert_eq!(events[0].triggering_line_sample.as_deref(), Some(l2));
    }

    #[test]
    fn below_threshold_creates_separate_leaf_no_widening() {
        // The H1.1 invariant: lines with `sim_seq < threshold`
        // remain distinct templates. "user logged in" vs
        // "user logged out" mask to two length-3 templates
        // differing at position 2; sim_seq = 2/3 ≈ 0.667 <
        // default 0.7, so no widening.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let id1 = cluster.ingest(&string_record(&t, "user logged in"));
        let id2 = cluster.ingest(&string_record(&t, "user logged out"));

        assert_ne!(id1, id2);
        assert_eq!(cluster.template_count(&t), 2);
        assert_eq!(cluster.merges_total(), 0);
        assert!(sink.is_empty());
    }

    #[test]
    fn degenerate_widening_is_rejected_and_emits_rejection_event() {
        // RFC0001.2 — a widening that would leave zero Fixed
        // tokens is rejected:
        //  - returns NO_TEMPLATE
        //  - emits TemplateWideningRejectedDegenerate
        //  - increments parse_failures_total (not merges_total)
        //  - does NOT bump template_version or modify the leaf
        //
        // Construction notes:
        //
        // - We use a `with_prefix_depth(0)` cluster so all length-3
        //   lines share one leaf list. The default prefix-tree
        //   shape partitions on the first two tokens, which makes
        //   the degenerate path structurally unreachable (the
        //   prefix-path tokens are always Fixed in any reachable
        //   leaf).
        //
        // - Threshold of 0.3 so a 1/3-similar attach still
        //   triggers widening instead of creating a fresh leaf.
        //
        //   L1 = ["alpha", "beta", "gamma"] — fresh leaf v=1.
        //   L2 = ["alpha", "xxx", "yyy"] — sim with L1 = 1/3 ≥ 0.3
        //        → widens positions 1, 2 → template
        //        ["alpha", <*>, <*>], v=2. 1 Fixed left, NOT
        //        degenerate.
        //   L3 = ["zzz", "qqq", "rrr"] — sim with the widened
        //        template = 2/3 (the two wildcards match) ≥ 0.3
        //        → would widen position 0 (the last Fixed)
        //        → fully degenerate → rejected.
        let config = MinerConfig::try_new(0.3, 256).expect("valid config");
        let sink = SharedAuditSink::new();
        let mut cluster =
            MinerCluster::with_audit_sink(config, Box::new(sink.clone())).with_prefix_depth(0);
        let t = TenantId::new("tenant-x");

        // Construct records bypassing masking by using single-letter
        // tokens that no mask rule fires on.
        let l1 = cluster.ingest(&string_record(&t, "alpha beta gamma"));
        let _l2 = cluster.ingest(&string_record(&t, "alpha xxx yyy"));
        let l3 = cluster.ingest(&string_record(&t, "zzz qqq rrr"));

        // L1 created the leaf, L2 widened it, L3 was rejected.
        assert_ne!(l1, NO_TEMPLATE);
        assert_eq!(l3, NO_TEMPLATE);
        assert_eq!(cluster.merges_total(), 1, "only L2's widening counts");
        assert_eq!(cluster.parse_failures_total(), 1, "L3 was rejected");

        let events = sink.drain();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, AuditEventType::TemplateWidened);
        assert_eq!(
            events[1].event_type,
            AuditEventType::TemplateWideningRejectedDegenerate,
        );
        // Rejection event: versions point at the same number
        // (no version bump on rejection).
        assert_eq!(events[1].old_version, events[1].new_version);
        // The audit payload records the would-be new template.
        assert_eq!(events[1].new_template, "<*> <*> <*>");

        // Leaf state was not mutated by the rejection — still has
        // its post-widening template (1 Fixed at position 0).
        let templates = cluster.templates_for(&t);
        assert_eq!(templates.len(), 1);
        let leaf_template = &templates[0].0;
        assert_eq!(leaf_template.len(), 3);
        assert!(matches!(leaf_template[0], OwnedToken::Fixed(ref s) if s == "alpha"));
        assert!(matches!(leaf_template[1], OwnedToken::Wildcard));
        assert!(matches!(leaf_template[2], OwnedToken::Wildcard));
    }

    #[test]
    fn ingest_returns_no_template_sentinel_for_empty_string_body() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let id_empty = cluster.ingest(&string_record(&t, ""));
        let id_blank = cluster.ingest(&string_record(&t, "   \t\n"));

        assert_eq!(id_empty, NO_TEMPLATE);
        assert_eq!(id_blank, NO_TEMPLATE);
        assert_eq!(cluster.template_count(&t), 0);
        assert_eq!(
            cluster.parse_failures_total(),
            2,
            "empty input is the parse-failure floor's simplest case",
        );
    }

    #[test]
    fn template_count_grows_with_each_distinct_template() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let _ = cluster.ingest(&string_record(&t, "user 17 logged in"));
        let _ = cluster.ingest(&string_record(&t, "GET /home 200"));

        let cached = cluster.template_count(&t);
        assert_eq!(cached, 2);
    }

    #[test]
    fn best_candidate_selection_picks_highest_similarity_in_parent_leaf_list() {
        // Two leaves under the same parent, one matches the line
        // better than the other. The miner must pick the higher-
        // similarity leaf for the widen target — not the first or
        // the one it happens to encounter.
        //
        // L1 = "alpha beta gamma delta epsilon" (length 5)
        // L2 = "alpha beta gamma zeta epsilon" (length 5, also
        //       under the same length-5/prefix-"alpha beta" bucket
        //       — but it must be a distinct leaf, so we force
        //       distinctness by ingesting under a tweaked
        //       threshold-disabling config)
        //
        // Forcing two leaves under one parent requires that at
        // least the second ingest land below threshold. Use a
        // threshold of 1.0 so any mismatch makes a new leaf, then
        // drop to a threshold-allowing scenario for the third.
        //
        // Simpler: build via a two-stage config swap is hard since
        // MinerCluster owns the config. Instead, push leaves
        // directly through `templates_for` is read-only.
        //
        // Real-world simpler: under a threshold of 1.0, every
        // distinct mask creates its own leaf. Then we can't widen
        // anything (no merges happen). That doesn't test the
        // best-candidate logic.
        //
        // Better: under the default 0.7 threshold, ingest two
        // lines whose mismatch is at *one* position (sim 4/5 =
        // 0.8) — those collapse to a single leaf via widening.
        // To get two leaves under one parent we need at least
        // one mismatch that falls below 0.7. So:
        //
        //   L1 = "alpha beta gamma delta epsilon"  → leaf A
        //   L2 = "alpha beta phi rho sigma"        → sim 2/5 = 0.4
        //                                            < 0.7 → leaf B
        //                                            (same prefix
        //                                            "alpha beta",
        //                                            length 5)
        //   L3 = "alpha beta gamma delta zeta"     → sim with
        //                                            leaf A = 4/5
        //                                            = 0.8, sim
        //                                            with leaf B
        //                                            = 2/5 = 0.4.
        //                                            Best is A.
        //                                            Widens A at
        //                                            position 4.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let id_a = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon"));
        let id_b = cluster.ingest(&string_record(&t, "alpha beta phi rho sigma"));
        assert_ne!(id_a, id_b, "leaves are distinct after L2");
        assert_eq!(cluster.template_count(&t), 2);
        assert!(
            sink.is_empty(),
            "L2 fell below threshold → fresh leaf, no widening",
        );

        let id_c = cluster.ingest(&string_record(&t, "alpha beta gamma delta zeta"));
        // Must widen leaf A (sim 0.8), not B (sim 0.4).
        assert_eq!(
            id_c, id_a,
            "best-candidate selection must pick the higher-similarity leaf",
        );
        assert_eq!(cluster.merges_total(), 1);
        let events = sink.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].template_id, id_a);
        assert_eq!(events[0].positions_widened, vec![4]);
    }

    // ---------- new behaviour from PR #28: body fork + structured short-circuit ----------

    #[test]
    fn ingest_returns_no_template_for_absent_body() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");
        let r = OtlpLogRecord {
            tenant_id: t.clone(),
            body: None,
            ..Default::default()
        };

        let id = cluster.ingest(&r);

        assert_eq!(id, NO_TEMPLATE);
        assert_eq!(cluster.template_count(&t), 0);
    }

    #[test]
    fn structured_body_short_circuit_allocates_one_template_per_severity_scope_tuple() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let id1 = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id2 = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id3 = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));

        assert_eq!(id1, id2);
        assert_eq!(id2, id3);
        assert_eq!(cluster.template_count(&t), 1);
    }

    #[test]
    fn structured_body_distinguishes_severity_within_one_scope() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let id_info = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id_error = cluster.ingest(&structured_record(&t, 17, Some("lib.auth")));

        assert_ne!(id_info, id_error);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn structured_body_distinguishes_scope_within_one_severity() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let id_a = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id_b = cluster.ingest(&structured_record(&t, 9, Some("lib.payments")));

        assert_ne!(id_a, id_b);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn structured_body_with_scope_none_is_its_own_bucket() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let id_none = cluster.ingest(&structured_record(&t, 9, None));
        let id_some = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));

        assert_ne!(id_none, id_some);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn structured_body_isolates_template_ids_across_tenants() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let a = TenantId::new("tenant-a");
        let b = TenantId::new("tenant-b");

        let id_a = cluster.ingest(&structured_record(&a, 9, Some("lib.auth")));
        let id_b = cluster.ingest(&structured_record(&b, 9, Some("lib.auth")));

        assert_ne!(
            id_a, id_b,
            "structured records with identical key tuple must get distinct template_ids across tenants",
        );
        assert_eq!(cluster.template_count(&a), 1);
        assert_eq!(cluster.template_count(&b), 1);
    }

    #[test]
    fn structured_and_string_share_no_template_ids_at_same_severity_scope() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let id_struct = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id_string = cluster.ingest(&OtlpLogRecord {
            tenant_id: t.clone(),
            severity_number: 9,
            scope_name: Some("lib.auth".to_string()),
            body: Some(Body::String("hello".to_string())),
            ..Default::default()
        });

        assert_ne!(id_struct, id_string);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn string_body_distinguishes_severity_within_one_scope() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");
        let info = OtlpLogRecord {
            tenant_id: t.clone(),
            severity_number: 9,
            body: Some(Body::String("user 42 logged in".to_string())),
            ..Default::default()
        };
        let error = OtlpLogRecord {
            severity_number: 17,
            ..info.clone()
        };

        let id_info = cluster.ingest(&info);
        let id_error = cluster.ingest(&error);

        assert_ne!(id_info, id_error);
        assert_eq!(cluster.template_count(&t), 2);
    }

    // ---------- helper-function unit tests ----------

    #[test]
    fn find_widening_positions_returns_only_mismatched_fixed_positions() {
        let template = vec![
            OwnedToken::Fixed("user".to_string()),
            OwnedToken::Fixed("42".to_string()),
            OwnedToken::Wildcard,
            OwnedToken::Fixed("in".to_string()),
        ];
        let line = ["user", "17", "anything", "out"];
        let positions = find_widening_positions(&line, &template);
        // Position 0: Fixed "user" == "user" → no widening.
        // Position 1: Fixed "42" != "17" → widen.
        // Position 2: Wildcard → never in the widening set.
        // Position 3: Fixed "in" != "out" → widen.
        assert_eq!(positions, vec![1, 3]);
    }

    #[test]
    fn would_be_degenerate_only_when_no_fixed_remain() {
        let template = vec![
            OwnedToken::Fixed("a".to_string()),
            OwnedToken::Wildcard,
            OwnedToken::Fixed("c".to_string()),
        ];
        // Widening only position 0 leaves position 2 Fixed → not degenerate.
        assert!(!would_be_degenerate(&template, &[0]));
        // Widening position 2 leaves position 0 Fixed → not degenerate.
        assert!(!would_be_degenerate(&template, &[2]));
        // Widening positions 0 AND 2 leaves nothing Fixed → degenerate.
        assert!(would_be_degenerate(&template, &[0, 2]));
        // Widening no positions on a template with Fixed left → not degenerate.
        assert!(!would_be_degenerate(&template, &[]));
    }

    #[test]
    fn format_template_renders_canonical_form() {
        let template = vec![
            OwnedToken::Fixed("user".to_string()),
            OwnedToken::Wildcard,
            OwnedToken::Fixed("logged".to_string()),
            OwnedToken::Wildcard,
        ];
        assert_eq!(format_template(&template), "user <*> logged <*>");
    }

    #[test]
    fn apply_widening_replaces_only_listed_positions() {
        let mut template = vec![
            OwnedToken::Fixed("a".to_string()),
            OwnedToken::Fixed("b".to_string()),
            OwnedToken::Fixed("c".to_string()),
        ];
        apply_widening(&mut template, &[1]);
        assert!(matches!(template[0], OwnedToken::Fixed(ref s) if s == "a"));
        assert!(matches!(template[1], OwnedToken::Wildcard));
        assert!(matches!(template[2], OwnedToken::Fixed(ref s) if s == "c"));
    }
}
