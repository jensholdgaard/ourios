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
//!   body retention + parse-failure floor per RFC §6.3) lands in
//!   a follow-up PR; today this branch simply creates a new leaf.
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
//!   on [`AuditEventKind`] but no widening path emits it yet
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
//! [`AuditEventKind`]: ourios_core::audit::AuditEventKind

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use ourios_core::audit::{
    AuditEvent, AuditEventKind, AuditSink, NoOpAuditSink, ParamType, SlotExpansion, SlotTypes,
    hash_triggering_line, sample_first_256_bytes,
};
use ourios_core::clock::{Clock, SystemClock};
use ourios_core::confidence::ConfidenceZone;
use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::record::{BodyKind, MinedRecord, NoOpRecordSink, Param, RecordSink};
use ourios_core::tenant::TenantId;

use crate::mask::mask;
use crate::sim_seq::sim_seq_owned;
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
    // Mined-record sink per RFC §6.1. Same shape as `audit_sink`;
    // the Parquet writer (post-`ourios-parquet`) drops in by
    // swapping the impl. Trait-from-day-one — the second
    // consumer is a named planned roadmap item.
    record_sink: Box<dyn RecordSink>,
    // §6.4 counter: structural-widening events increment this;
    // rejection events do not. Today only `TemplateWidened`
    // emits; `TemplateTypeExpanded` will also increment once the
    // type-expansion PR lands ([`AuditEventKind::counts_as_merge`]
    // names that contract). Atomic so the §6.8 Prometheus exposer
    // can read without taking a lock on the cluster.
    merges_total: AtomicU64,
    // §6.8 counter: lines that produced no template. Increments
    // on empty / over-cap input, degenerate-widening rejection,
    // and the §6.3 parse-failure zone (`simSeq < floor`).
    parse_failures_total: AtomicU64,
    // §6.8 counter: lines whose body must be retained in the
    // emitted data record per RFC §6.3. Increments on the
    // lossy *zone* (`floor ≤ simSeq < threshold`) and on the
    // parse-failure zone (`simSeq < floor`); does **not**
    // increment for clean attaches or for the orthogonal
    // §6.6 `lossy_flag = true` (tokenizer-failure) path —
    // see `ConfidenceZone::retains_body`. The numerator of the
    // §3.1 `body_retention_ratio` gauge.
    body_retentions_total: AtomicU64,
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
    // Wall-clock source for audit-event `timestamp` stamping per
    // RFC §6.4. [`SystemClock`] in production; tests substitute
    // a [`ourios_core::clock::TestClock`] via
    // [`Self::with_clock`] for deterministic timestamp
    // assertions (wall-clock comparisons against `now()` flake
    // under NTP step / leap seconds / VM pause).
    clock: Box<dyn Clock>,
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
    /// Build an empty cluster with no-op sinks for both audit
    /// events ([`NoOpAuditSink`]) and mined records
    /// ([`NoOpRecordSink`]) and a [`SystemClock`] (host wall
    /// clock). Production default — when `ourios-wal` and
    /// `ourios-parquet` land they replace the no-ops via
    /// [`Self::with_audit_sink`] / [`Self::with_record_sink`].
    /// Tests that need to inspect emissions opt in via the
    /// matching `Shared*Sink` types from `ourios-core`.
    #[must_use]
    pub fn new(config: MinerConfig) -> Self {
        Self::with_audit_sink(config, Box::new(NoOpAuditSink::new()))
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
            record_sink: Box::new(NoOpRecordSink::new()),
            merges_total: AtomicU64::new(0),
            parse_failures_total: AtomicU64::new(0),
            body_retentions_total: AtomicU64::new(0),
            prefix_depth: DEFAULT_PREFIX_DEPTH,
            clock: Box::new(SystemClock::new()),
        }
    }

    /// Set the mined-record sink. Mirrors [`Self::with_audit_sink`].
    /// Production builds replace the default [`NoOpRecordSink`]
    /// with the WAL/Parquet-backed sink once that crate lands;
    /// tests use a [`ourios_core::record::SharedRecordSink`] for
    /// observable emissions.
    #[must_use]
    pub fn with_record_sink(mut self, sink: Box<dyn RecordSink>) -> Self {
        self.record_sink = sink;
        self
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

    /// Set the wall-clock source used for audit-event `timestamp`
    /// stamping. Production builds use [`SystemClock`] (the
    /// default); tests substitute a
    /// [`ourios_core::clock::TestClock`] for deterministic
    /// timestamps. The clock is consumed by `Box<dyn Clock>` so
    /// alternate implementations (recorded traces, monotonic
    /// counters, future skew-detecting wrappers) drop in without
    /// touching the cluster's API.
    #[must_use]
    pub fn with_clock(mut self, clock: Box<dyn Clock>) -> Self {
        self.clock = clock;
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

    /// Cumulative count of structural-widening events across all
    /// tenants — `TemplateWidened` today, plus
    /// `TemplateTypeExpanded` once that variant has an emitter
    /// (see [`AuditEventKind::counts_as_merge`]). Rejection events
    /// are recorded but do not increment this counter.
    /// Read-side placeholder for the §6.8 Prometheus gauge.
    #[must_use]
    pub fn merges_total(&self) -> u64 {
        self.merges_total.load(Ordering::Relaxed)
    }

    /// Cumulative count of lines that produced no template:
    /// empty / whitespace-only `Body::String`, over-cap lines
    /// (`> u16::MAX` tokens), the §6.4 degenerate-template
    /// rejection branch, and the §6.3 parse-failure zone
    /// (`simSeq < similarity_floor`). Read-side placeholder for
    /// the §6.8 `parse_failures_total` Prometheus gauge.
    #[must_use]
    pub fn parse_failures_total(&self) -> u64 {
        self.parse_failures_total.load(Ordering::Relaxed)
    }

    /// Cumulative count of lines whose body the emitted data
    /// record will retain per RFC §6.3 / §6.4. Bumps on every
    /// path the RFC marks "retain body":
    ///
    /// - §6.3 lossy zone (`floor ≤ sim < threshold`) — line
    ///   attaches to a fresh leaf with body retained.
    /// - §6.3 parse-failure zone (`sim < floor`) — no template,
    ///   body retained.
    /// - §6.2 step 1 parse-failure paths: empty / whitespace-only
    ///   input and over-cap input (line longer than the
    ///   `u16::MAX`-token bound). Both are emitted as parse-
    ///   failure records carrying the original bytes.
    /// - §6.4 degenerate-widening rejection — "treated as a
    ///   parse failure ... retain body" per the RFC.
    ///
    /// Clean attaches don't bump this; nor does the orthogonal
    /// §6.6 `lossy_flag = true` path (tokenizer failure).
    /// Numerator of the §3.1 `body_retention_ratio` gauge.
    #[must_use]
    pub fn body_retentions_total(&self) -> u64 {
        self.body_retentions_total.load(Ordering::Relaxed)
    }

    /// Mark one parse-failure event: increments both
    /// `parse_failures_total` and `body_retentions_total`. RFC
    /// §6.3 says every parse-failure path retains body (the
    /// record is emitted with the original bytes even when no
    /// template was allocated), so the two counters move
    /// together at every parse-failure site — empty input,
    /// over-cap input, the §6.4 degenerate-widening rejection,
    /// and the §6.3 parse-failure zone. Centralised here so a
    /// future contract change touches one site, not four.
    fn record_parse_failure(&self) {
        self.parse_failures_total.fetch_add(1, Ordering::Relaxed);
        self.body_retentions_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Build the OTLP-envelope half of a `MinedRecord` from the
    /// incoming `OtlpLogRecord`. The mining-output fields
    /// (`template_id`, `template_version`, `params`,
    /// `separators`, `body`, `confidence`, `lossy_flag`) are left
    /// at their zero / sentinel defaults; the calling site
    /// customises before calling [`Self::emit_record`].
    fn record_envelope(record: &OtlpLogRecord, body_kind: BodyKind) -> MinedRecord {
        MinedRecord {
            tenant_id: record.tenant_id.clone(),
            template_id: NO_TEMPLATE,
            template_version: 0,
            severity_number: record.severity_number,
            scope_name: record.scope_name.clone(),
            time_unix_nano: record.time_unix_nano,
            body_kind,
            params: Vec::new(),
            separators: Vec::new(),
            body: None,
            confidence: 0.0,
            lossy_flag: false,
        }
    }

    /// Hand one [`MinedRecord`] to the record sink. Centralised
    /// so a future "decorate every record with X" step has one
    /// site to change.
    fn emit_record(&mut self, record: MinedRecord) {
        self.record_sink.emit(record);
    }
}

/// Free helper: clone tokenize's borrowed-from-input separators
/// into the `Vec<String>` shape `MinedRecord::separators`
/// requires. RFC §6.6 "capture, always": the order and length
/// invariants (`separators.len() == tokens.len() + 1` on
/// `BodyKind::String`) are upheld by `tokenize` itself; this
/// just owns the bytes.
fn separators_to_owned(separators: &[&str]) -> Vec<String> {
    separators.iter().map(|s| (*s).to_string()).collect()
}

/// Free helper: lift `mask`'s typed-params output into the
/// `Vec<Param>` shape `MinedRecord::params` carries. PR-A
/// alignment: this is "one entry per masked position, in token
/// order"; PR-B reconciles with §6.1's "one entry per template
/// `<*>` slot" once `reconstruct()` lands and the alignment
/// becomes load-bearing.
fn params_from_mask(typed_params: &[crate::mask::TypedParam<'_>]) -> Vec<Param> {
    typed_params
        .iter()
        .map(|p| Param {
            type_tag: p.type_tag,
            value: p.value.to_string(),
        })
        .collect()
}

/// Look up the `ParamType` at a line position from `mask()`'s
/// classification — the authoritative source.
///
/// `wildcard_positions` is ascending (single forward pass over the
/// input tokens), so a binary search is `O(log n)` per call.
/// Returns `Str` for positions `mask()` did not classify (the
/// original token wasn't a numeric, UUID, or IPv4 literal); per
/// RFC §6.2 step 5 the literal value is captured as
/// `ParamType::Str`.
///
/// **Why not match the masked-token string content.** An input log
/// line that contains the literal token `"<NUM>"` / `"<IP>"` /
/// `"<UUID>"` passes through `mask()` unchanged because the rules
/// (digits / IPv4 / UUID) don't fire on it. The masked-token at
/// that position is therefore the literal string, indistinguishable
/// by content from a mask-emitted tag. String-shape inference
/// would mis-classify the literal as the corresponding `ParamType`
/// and corrupt `slot_types` / suppress `TemplateTypeExpanded`
/// audits. The `wildcard_positions` array does not include the
/// position for the literal case, so this lookup gives the right
/// answer in both.
fn param_type_for_line_position(
    p: usize,
    wildcard_positions: &[usize],
    typed_params: &[crate::mask::TypedParam<'_>],
) -> ParamType {
    debug_assert_eq!(
        wildcard_positions.len(),
        typed_params.len(),
        "mask invariant: typed_params parallel to wildcard_positions",
    );
    match wildcard_positions.binary_search(&p) {
        Ok(k) => typed_params[k].type_tag,
        Err(_) => ParamType::Str,
    }
}

/// Heuristic classification of a leaf's pre-widen `Fixed` token.
/// Used only to seed `slot_types` on widening.
///
/// **Known limitation.** On main (this PR's baseline), `mask()`
/// emits the tag strings `<NUM>` / `<IP>` / `<UUID>` and the
/// fresh-leaf builder stores them as `Fixed("<NUM>")` etc. The
/// leaf carries no metadata distinguishing a `Fixed("<NUM>")` that
/// came from a mask emit (slot type Num) from one that came from a
/// literal user input token spelled "<NUM>" (slot type Str). This
/// heuristic classifies any matching tag string as the
/// corresponding `ParamType`; the mis-classification surface is
/// bounded to the seed step on widening — a future correct
/// observation at the slot only causes a missed
/// `TemplateTypeExpanded` audit, not a silent template merge — and
/// is eliminated entirely by PR-B-1, which switches masked
/// positions to `Wildcard` in the leaf template (with `slot_types`
/// seeded from `typed_params` at fresh-leaf creation, no inference
/// required).
fn type_at_leaf_fixed_token(s: &str) -> ParamType {
    match s {
        "<NUM>" => ParamType::Num,
        "<IP>" => ParamType::Ip,
        "<UUID>" => ParamType::Uuid,
        _ => ParamType::Str,
    }
}

/// On widening, seed [`Leaf::slot_types`] for each newly-introduced
/// `Wildcard` position. The initial type set captures both
/// observations the widening witnessed:
///
/// - the pre-widen token (which was a `Fixed`-token mismatch — the
///   reason widening fired)
/// - the line's token at that position (the value that triggered
///   the widening)
///
/// Neither observation counts as a `TemplateTypeExpanded` — the
/// slot didn't exist before this attach, so there's no "expansion"
/// to audit; the `TemplateWidened` event covers the structural
/// change. Subsequent attaches with a `ParamType` not in this
/// initial set are what trigger `TemplateTypeExpanded` events
/// later in the same attach (or in future attaches).
///
/// Ordinal alignment: positions are inserted into `slot_types` at
/// the post-widen wildcard-ordinal of each newly-widened position,
/// so the post-call invariant `slot_types.len() == count(template
/// Wildcards)` holds. `positions_widened` is ascending (by the
/// [`find_widening_positions`] contract), so a single forward walk
/// over the post-widen template is enough.
fn update_slot_types_on_widening(
    slot_types: &mut Vec<SlotTypes>,
    pre_widen_template: &[OwnedToken],
    post_widen_template: &[OwnedToken],
    line_wildcard_positions: &[usize],
    line_typed_params: &[crate::mask::TypedParam<'_>],
    positions_widened: &[usize],
) {
    debug_assert!(
        positions_widened.windows(2).all(|w| w[0] < w[1]),
        "positions_widened must be sorted ascending",
    );
    debug_assert_eq!(
        pre_widen_template.len(),
        post_widen_template.len(),
        "widening is in-place — length is invariant",
    );

    let mut ordinal = 0usize;
    let mut widen_iter = positions_widened.iter().copied().peekable();
    for (p, tok) in post_widen_template.iter().enumerate() {
        if matches!(tok, OwnedToken::Wildcard) {
            if widen_iter.peek().copied() == Some(p) {
                // The line's type at the triggering position is
                // authoritative — read it from mask's
                // classification rather than inferring from the
                // masked-token string (which collides with literal
                // "<NUM>"/"<IP>"/"<UUID>" tokens, see
                // `param_type_for_line_position`).
                let line_type =
                    param_type_for_line_position(p, line_wildcard_positions, line_typed_params);
                let pre_token = match &pre_widen_template[p] {
                    OwnedToken::Fixed(s) => s.as_str(),
                    OwnedToken::Wildcard => unreachable!(
                        "find_widening_positions only flags Fixed mismatches; \
                         a pre-existing Wildcard cannot appear in positions_widened",
                    ),
                };
                let initial =
                    SlotTypes::singleton(type_at_leaf_fixed_token(pre_token)).insert(line_type);
                slot_types.insert(ordinal, initial);
                widen_iter.next();
            }
            ordinal += 1;
        }
    }
    debug_assert!(
        widen_iter.peek().is_none(),
        "every widened position must have a matching Wildcard in the post-widen template",
    );
}

/// Walk the post-widen template's `Wildcard` slots and collect any
/// `ParamType`s the current line introduces that aren't already in
/// the slot's `slot_types` entry. Each addition becomes one
/// [`SlotExpansion`] in the returned vector; an empty result means
/// the attach is a no-op for the type-expansion path.
///
/// `skip_positions` lists positions that were newly created by the
/// same attach's widening step — their `slot_types` entries were
/// just initialised in [`update_slot_types_on_widening`], so we do
/// **not** treat the initial state as an "expansion" (the
/// `TemplateWidened` event already covers the slot's existence,
/// and the initial type set is its first state, not an addition).
fn collect_type_expansions(
    template: &[OwnedToken],
    line_wildcard_positions: &[usize],
    line_typed_params: &[crate::mask::TypedParam<'_>],
    slot_types: &[SlotTypes],
    skip_positions: &[usize],
) -> Vec<SlotExpansion> {
    debug_assert!(
        skip_positions.windows(2).all(|w| w[0] < w[1]),
        "skip_positions must be sorted ascending",
    );

    let mut out: Vec<SlotExpansion> = Vec::new();
    let mut ordinal: u16 = 0;
    let mut skip_iter = skip_positions.iter().copied().peekable();
    for (p, tok) in template.iter().enumerate() {
        if matches!(tok, OwnedToken::Wildcard) {
            let is_freshly_widened = skip_iter.peek().copied() == Some(p);
            if is_freshly_widened {
                skip_iter.next();
            } else {
                // Line side: authoritative classification from
                // mask, not from the masked-token string.
                let line_type =
                    param_type_for_line_position(p, line_wildcard_positions, line_typed_params);
                let current_set = slot_types[ordinal as usize];
                if !current_set.contains(line_type) {
                    out.push(SlotExpansion {
                        slot_index: ordinal,
                        added_types: vec![line_type],
                    });
                }
            }
            ordinal += 1;
        }
    }
    out
}

/// Apply the expansions returned by [`collect_type_expansions`] to
/// the leaf's `slot_types`. Idempotent under
/// [`SlotTypes::insert`] (re-applying the same expansion is a
/// no-op).
fn apply_type_expansions(slot_types: &mut [SlotTypes], expansions: &[SlotExpansion]) {
    for exp in expansions {
        let s = &mut slot_types[exp.slot_index as usize];
        for &t in &exp.added_types {
            *s = s.insert(t);
        }
    }
}

/// Outcome of the leaf-mutating phase of `attach_and_maybe_widen`.
///
/// Phase 1 borrows the leaf and produces this enum; phase 2 drops
/// the leaf borrow and emits audit events / the data record using
/// the extracted data. The split keeps `&mut self.audit_sink` and
/// `&mut self.tenants` from clashing on the borrow checker — every
/// audit emit happens after the leaf borrow ends.
enum AttachPlan {
    /// No mutation: similarity 1.0 with no new types at any slot.
    /// Reuse `(template_id, template_version)` verbatim.
    CleanReuse {
        template_id: u64,
        template_version: u32,
    },
    /// Degenerate widening rejected per §6.4. Leaf untouched.
    Rejected {
        template_id: u64,
        version: u32,
        current_template: String,
        would_be_template: String,
        would_be_positions: Vec<u16>,
    },
    /// Leaf mutated (widened and/or type-expanded). `events` is the
    /// audit-event payload in emission order: `TemplateWidened`
    /// before `TemplateTypeExpanded` per RFC §6.2's combined-attach
    /// contract.
    Mutated {
        template_id: u64,
        events: Vec<AuditEventKind>,
        final_version: u32,
    },
}

/// RFC §6.2 step 5 — compute the structural mutations and the
/// resulting audit-event payloads for a candidate-attach decision.
/// Mutates `leaf.template`, `leaf.template_version`, and
/// `leaf.slot_types` in place when widening or type-expansion
/// fires; the caller drops the leaf borrow before draining the
/// returned `events`.
//
// This function maps 1:1 onto the RFC §6.2 step 5 algorithm:
// (clean reuse / type-expansion-only / degenerate rejection /
// widening + optional expansion). Each branch reads and mutates
// the same `leaf` state, so factoring branches into helpers would
// require shuttling the leaf back and forth (or returning partial
// `AttachPlan`s and re-entering). The current single-function
// shape keeps the RFC mapping line-for-line and the locking-tests
// in `cluster::tests` against this function direct; the
// too_many_lines lint is silenced here rather than fragmenting
// the algorithm for the lint's sake.
#[allow(clippy::too_many_lines)]
fn plan_attach(
    leaf: &mut Leaf,
    masked_strs: &[&str],
    line_wildcard_positions: &[usize],
    line_typed_params: &[crate::mask::TypedParam<'_>],
) -> AttachPlan {
    let positions_widened = find_widening_positions(masked_strs, &leaf.template);

    if positions_widened.is_empty() {
        // No Fixed mismatch — check for a type-expansion-only
        // attach (a known wildcard slot seeing a new ParamType).
        let expansions = collect_type_expansions(
            &leaf.template,
            line_wildcard_positions,
            line_typed_params,
            &leaf.slot_types,
            &[],
        );
        if expansions.is_empty() {
            return AttachPlan::CleanReuse {
                template_id: leaf.template_id,
                template_version: leaf.template_version,
            };
        }
        apply_type_expansions(&mut leaf.slot_types, &expansions);
        let old_version = leaf.template_version;
        let new_version = leaf
            .template_version
            .checked_add(1)
            .expect("template_version overflow: 2^32 expansions on one leaf is implausible");
        leaf.template_version = new_version;
        let template_str = format_template(&leaf.template);
        return AttachPlan::Mutated {
            template_id: leaf.template_id,
            final_version: new_version,
            events: vec![AuditEventKind::TemplateTypeExpanded {
                old_version,
                new_version,
                // Structure is unchanged by type expansion; both
                // fields carry the same canonical-form string per
                // RFC §6.4 (the expansion lives in `slots_expanded`).
                old_template: template_str.clone(),
                new_template: template_str,
                slots_expanded: expansions,
            }],
        };
    }

    if would_be_degenerate(&leaf.template, &positions_widened) {
        let current_template = format_template(&leaf.template);
        let mut new_template_tokens = leaf.template.clone();
        apply_widening(&mut new_template_tokens, &positions_widened);
        let would_be_template = format_template(&new_template_tokens);
        let would_be_positions = positions_to_u16(&positions_widened);
        return AttachPlan::Rejected {
            template_id: leaf.template_id,
            version: leaf.template_version,
            current_template,
            would_be_template,
            would_be_positions,
        };
    }

    // Widening path. Snapshot pre-widen template for slot seeding
    // and the audit payload's `old_template` string.
    let template_id = leaf.template_id;
    let pre_widen_template = leaf.template.clone();
    let old_version = leaf.template_version;
    let old_template_str = format_template(&leaf.template);
    let positions_u16 = positions_to_u16(&positions_widened);

    apply_widening(&mut leaf.template, &positions_widened);
    update_slot_types_on_widening(
        &mut leaf.slot_types,
        &pre_widen_template,
        &leaf.template,
        line_wildcard_positions,
        line_typed_params,
        &positions_widened,
    );
    let version_after_widen = old_version
        .checked_add(1)
        .expect("template_version overflow: 2^32 widenings on one leaf is implausible");
    leaf.template_version = version_after_widen;
    let template_after_widen = format_template(&leaf.template);

    let mut events: Vec<AuditEventKind> = Vec::with_capacity(2);
    events.push(AuditEventKind::TemplateWidened {
        old_version,
        new_version: version_after_widen,
        old_template: old_template_str,
        new_template: template_after_widen.clone(),
        positions_widened: positions_u16,
    });

    // Pre-existing wildcards may also see a new ParamType from
    // this same line (RFC §6.2: a single attach can trigger both
    // widening and type-expansion; events emit in this order).
    let expansions = collect_type_expansions(
        &leaf.template,
        line_wildcard_positions,
        line_typed_params,
        &leaf.slot_types,
        &positions_widened,
    );
    let final_version = if expansions.is_empty() {
        version_after_widen
    } else {
        apply_type_expansions(&mut leaf.slot_types, &expansions);
        let version_after_expand = version_after_widen
            .checked_add(1)
            .expect("template_version overflow: 2^32 expansions on one leaf is implausible");
        leaf.template_version = version_after_expand;
        events.push(AuditEventKind::TemplateTypeExpanded {
            old_version: version_after_widen,
            new_version: version_after_expand,
            old_template: template_after_widen.clone(),
            new_template: template_after_widen,
            slots_expanded: expansions,
        });
        version_after_expand
    };

    AttachPlan::Mutated {
        template_id,
        events,
        final_version,
    }
}

impl MinerCluster {
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
            None => {
                // The wire delivered no body. Emit a single
                // record with `BodyKind::Absent` and the
                // template-id sentinel; tokenize/mask didn't
                // run, so there's no separator / param info to
                // carry. `lossy_flag = true` because there is no
                // template, so reconstruction is not possible.
                let mut rec = Self::record_envelope(record, BodyKind::Absent);
                rec.lossy_flag = true;
                self.emit_record(rec);
                NO_TEMPLATE
            }
            Some(Body::String(raw)) => self.ingest_string(record, raw),
            Some(Body::Structured(_)) => self.ingest_structured(record),
        }
    }

    /// `Body::String` path — RFC §6.2 steps 1–5 with widening.
    fn ingest_string(&mut self, record: &OtlpLogRecord, raw: &str) -> u64 {
        let tokenized = tokenize(raw);
        let masked = mask(&tokenized.tokens);
        // Pre-compute the owned forms once. Every emit path
        // reads these.
        let separators = separators_to_owned(&tokenized.separators);
        let params = params_from_mask(&masked.typed_params);
        let masked_strs: Vec<&str> = masked.tokens.into_iter().collect();

        if masked_strs.is_empty() {
            // Empty / whitespace-only input is a §6.2 step 1
            // parse failure. Tokenize still produced one
            // separator entry covering the entire input
            // (`tokens.len() + 1 == 1`).
            let mut rec = Self::record_envelope(record, BodyKind::String);
            rec.separators = separators;
            rec.body = Some(raw.to_string());
            rec.lossy_flag = true;
            self.emit_record(rec);
            self.record_parse_failure();
            return NO_TEMPLATE;
        }

        // Cap the line's token count at the audit-event's
        // position width (RFC §6.4 pins `positions_widened:
        // Vec<u16>`). Lines exceeding this can't be widened
        // safely — emitting an audit with a truncated set would
        // be the silent-merge bug `[CLAUDE.md §3.1]` exists to
        // prevent. ≥65 536 tokens in a single log line is
        // pathological; the §6.2 step 1 "line longer than
        // max-line-bytes" parse-failure path. Retain body per
        // §6.3.
        if masked_strs.len() > u16::MAX as usize {
            let mut rec = Self::record_envelope(record, BodyKind::String);
            rec.separators = separators;
            rec.params = params;
            rec.body = Some(raw.to_string());
            rec.lossy_flag = true;
            self.emit_record(rec);
            self.record_parse_failure();
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
        let floor = self.config.similarity_floor;

        match best {
            // No candidate at all → fresh leaf. Treated as clean
            // by definition: there was no weaker match to drop
            // into the lossy zone against, and no template to
            // declare a parse failure against.
            None => {
                let new_id = self.create_new_leaf(record, &masked_strs);
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.template_id = new_id;
                rec.template_version = 1;
                rec.separators = separators;
                rec.params = params;
                rec.confidence = 1.0;
                self.emit_record(rec);
                new_id
            }
            Some(c) => {
                match ConfidenceZone::classify(c.similarity, threshold, floor) {
                    // Clean: attach to candidate, optionally
                    // widening. RFC §6.2 step 5. No body
                    // retention. The helper emits its own
                    // record (one of: clean-reuse, widening, or
                    // degenerate-rejection).
                    ConfidenceZone::Clean => self.attach_and_maybe_widen(
                        record,
                        raw,
                        &masked_strs,
                        &masked.wildcard_positions,
                        &masked.typed_params,
                        c,
                        separators,
                        params,
                    ),
                    // Lossy: new leaf rather than force-merge
                    // into a too-weak candidate (RFC §6.2 step
                    // 5b). Body retained; no audit event
                    // (no widening happened). The retention
                    // counter bumps here; `record_parse_failure`
                    // covers the parse-failure-zone path
                    // separately.
                    ConfidenceZone::Lossy => {
                        self.body_retentions_total.fetch_add(1, Ordering::Relaxed);
                        let new_id = self.create_new_leaf(record, &masked_strs);
                        let mut rec = Self::record_envelope(record, BodyKind::String);
                        rec.template_id = new_id;
                        rec.template_version = 1;
                        rec.separators = separators;
                        rec.params = params;
                        rec.confidence = c.similarity / threshold;
                        rec.body = Some(raw.to_string());
                        // `lossy_flag` stays false — §6.6: the
                        // §6.3 lossy zone is "body retained,
                        // reconstruction expected to match".
                        self.emit_record(rec);
                        new_id
                    }
                    // Parse failure: no template allocated.
                    // Both counters bump via the shared helper.
                    ConfidenceZone::ParseFailure => {
                        let mut rec = Self::record_envelope(record, BodyKind::String);
                        rec.separators = separators;
                        rec.params = params;
                        rec.body = Some(raw.to_string());
                        rec.lossy_flag = true;
                        self.emit_record(rec);
                        self.record_parse_failure();
                        NO_TEMPLATE
                    }
                }
            }
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
            // Length is structurally guaranteed by the tree's
            // length-keyed first level (`Tree::descend` looks up
            // `by_length[masked_strs.len()]`), so a length
            // mismatch here would be a tree-invariant bug rather
            // than a runtime case to handle. Debug-only assert.
            debug_assert_eq!(
                leaf.template.len(),
                masked_strs.len(),
                "tree partitions by length; every leaf under this parent must match",
            );
            // Filter on the non-token half of the §6.1
            // template-key composition tuple. (Severity and
            // scope are *not* part of the tree's keying today —
            // we instead keep one leaf per `(severity, scope)`
            // pair under each `(length, prefix)` bucket and
            // filter on the leaf-list side.)
            if leaf.severity_number != record.severity_number
                || leaf.scope_name.as_deref() != record.scope_name.as_deref()
            {
                continue;
            }
            // Allocation-free over `&[OwnedToken]`. The borrowed
            // `Token` view + `Vec::collect` form would allocate
            // per leaf on every ingest call.
            let similarity = sim_seq_owned(masked_strs, &leaf.template);
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
            // Fresh leaves have no Wildcards yet (masking emits
            // `Fixed("<NUM>")` on main; mask→Wildcard arrives in
            // the PR-B-1 follow-on). The first widening grows
            // `slot_types` in lockstep with the new Wildcard slots.
            slot_types: vec![],
        });
        // Maintain the TenantState::template_count cache invariant —
        // every fresh allocation under `state` is mirrored here so
        // `MinerCluster::template_count` can stay O(1).
        state.template_count += 1;
        new_id
    }

    /// RFC §6.2 step 5 — clean-or-widen-or-type-expand attach to
    /// an existing leaf. The exit paths:
    ///
    /// - No mismatched `Fixed` positions **and** no new `ParamType`
    ///   at any existing `Wildcard` slot → truly clean attach;
    ///   reuse the leaf's `(template_id, template_version)`, no
    ///   audit event.
    /// - No mismatched `Fixed` positions **but** at least one
    ///   wildcard slot sees a `ParamType` not in its observed-type
    ///   set → emit `TemplateTypeExpanded`, bump version by 1.
    /// - One or more mismatched `Fixed` positions:
    ///   - Run the §6.4 degenerate guard. If the proposed widening
    ///     would leave zero `Fixed` tokens, emit
    ///     `TemplateWideningRejectedDegenerate`, increment
    ///     `parse_failures_total`, return [`NO_TEMPLATE`].
    ///   - Else apply the widening (in place on the leaf), seed
    ///     `slot_types` for the new slots from the pre-widen Fixed
    ///     token + the line's token, bump version, emit
    ///     `TemplateWidened`. Then run the type-expansion check on
    ///     the *pre-existing* wildcards; if any slot sees a new
    ///     `ParamType`, bump version again and emit
    ///     `TemplateTypeExpanded` per RFC §6.2's combined-attach
    ///     contract (`template_version` increments twice, two events
    ///     emitted in widening-then-expansion order).
    #[allow(clippy::too_many_arguments)]
    fn attach_and_maybe_widen(
        &mut self,
        record: &OtlpLogRecord,
        raw: &str,
        masked_strs: &[&str],
        line_wildcard_positions: &[usize],
        line_typed_params: &[crate::mask::TypedParam<'_>],
        candidate: Candidate,
        separators: Vec<String>,
        params: Vec<Param>,
    ) -> u64 {
        // Ownership rationale: each exit path emits **one** data
        // record and never reuses `separators` / `params` after
        // that emit. Taking the vectors by value lets each branch
        // move them straight into the record without a `.to_vec()`
        // clone.

        // Phase 1 — mutate the leaf and accumulate the audit-event
        // payloads. Hold the leaf borrow only over the mutation;
        // emitting through `self.audit_sink` happens in phase 2.
        let plan = {
            let state = self
                .tenants
                .get_mut(&record.tenant_id)
                .expect("tenant present: find_best_candidate returned Some(...)");
            let parent = state.tree.descend_mut(masked_strs, self.prefix_depth);
            let leaf = &mut parent.leaves[candidate.leaf_idx];
            plan_attach(
                leaf,
                masked_strs,
                line_wildcard_positions,
                line_typed_params,
            )
        };

        match plan {
            AttachPlan::CleanReuse {
                template_id,
                template_version,
            } => {
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.template_id = template_id;
                rec.template_version = template_version;
                rec.separators = separators;
                rec.params = params;
                rec.confidence = 1.0;
                self.emit_record(rec);
                template_id
            }
            AttachPlan::Rejected {
                template_id,
                version,
                current_template,
                would_be_template,
                would_be_positions,
            } => {
                self.audit_sink.emit(AuditEvent {
                    kind: AuditEventKind::TemplateWideningRejectedDegenerate {
                        version,
                        current_template,
                        would_be_template,
                        would_be_positions,
                    },
                    tenant_id: record.tenant_id.clone(),
                    template_id,
                    triggering_line_hash: hash_triggering_line(raw.as_bytes()),
                    triggering_line_sample: Some(sample_first_256_bytes(raw)),
                    timestamp: self.clock.now(),
                });
                // §6.4 treats degenerate widening as a parse
                // failure that retains body.
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.separators = separators;
                rec.params = params;
                rec.body = Some(raw.to_string());
                rec.lossy_flag = true;
                self.emit_record(rec);
                self.record_parse_failure();
                NO_TEMPLATE
            }
            AttachPlan::Mutated {
                template_id,
                events,
                final_version,
            } => {
                for kind in events {
                    let counts_as_merge = kind.counts_as_merge();
                    self.audit_sink.emit(AuditEvent {
                        kind,
                        tenant_id: record.tenant_id.clone(),
                        template_id,
                        triggering_line_hash: hash_triggering_line(raw.as_bytes()),
                        triggering_line_sample: Some(sample_first_256_bytes(raw)),
                        timestamp: self.clock.now(),
                    });
                    if counts_as_merge {
                        self.merges_total.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.template_id = template_id;
                rec.template_version = final_version;
                rec.separators = separators;
                rec.params = params;
                rec.confidence = 1.0;
                self.emit_record(rec);
                template_id
            }
        }
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
        let template_id = if let Some(&existing_id) = state.structured_templates.get(&key) {
            existing_id
        } else {
            let new_id = self.next_template_id;
            self.next_template_id += 1;
            state.structured_templates.insert(key, new_id);
            // Same cache invariant as create_new_leaf: one fresh
            // allocation, one cache increment.
            state.template_count += 1;
            new_id
        };

        // Emit a data record. Structured records carry no
        // separators or params — reconstruction goes via the
        // canonicalised-JSON `body` field (per §6.2 step 0); the
        // canonicalisation itself is deferred to the §6.6
        // follow-up that adds `reconstruct()`. PR-A emits the
        // record with `body = None` and a placeholder
        // `confidence = 1.0` (sentinel — no Drain comparison
        // happens in the structured branch).
        let mut rec = Self::record_envelope(record, BodyKind::Structured);
        rec.template_id = template_id;
        rec.template_version = 1;
        rec.confidence = 1.0;
        self.emit_record(rec);

        template_id
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

    /// Snapshot of one tenant's `Body::String` leaves. Returns an
    /// empty vec for unseen tenants.
    ///
    /// Order is not guaranteed (`HashMap` iteration). Stored
    /// templates may contain [`OwnedToken::Wildcard`] positions
    /// from §6.2 step 5 widening, so the return type carries
    /// [`OwnedToken`] (not `String`) — a `"<*>"` string sentinel
    /// would lose the wildcard-vs-literal distinction the type
    /// exists to preserve. Structured-body templates (§6.2 step-0
    /// short-circuit) are not returned by this helper — they have
    /// no token shape to surface.
    #[must_use]
    pub fn templates_for(&self, tenant_id: &TenantId) -> Vec<LeafSnapshot> {
        self.tenants.get(tenant_id).map_or_else(Vec::new, |s| {
            s.tree
                .collect_leaves()
                .into_iter()
                .map(|leaf| LeafSnapshot {
                    template: leaf.template.clone(),
                    template_id: leaf.template_id,
                    template_version: leaf.template_version,
                    slot_types: leaf.slot_types.clone(),
                })
                .collect()
        })
    }
}

/// Read-only view of a single leaf surfaced by
/// [`MinerCluster::templates_for`]. Carries the four fields a
/// test or operator typically needs to assert on; deliberately
/// owns its data so callers can drop the cluster borrow before
/// inspecting the snapshot.
#[derive(Debug, Clone)]
pub struct LeafSnapshot {
    pub template: Vec<OwnedToken>,
    pub template_id: u64,
    pub template_version: u32,
    pub slot_types: Vec<SlotTypes>,
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
///
/// Returns `usize` positions; `MinerCluster::ingest_string` has
/// already capped `line.len()` at `u16::MAX`, so a downstream
/// `u16` conversion at audit-construction time is infallible.
/// Using `usize` here avoids the silent-drop hazard a `u16` return
/// type carried (a missed mismatch position would have produced
/// an empty `positions_widened` → clean attach → silent merge).
fn find_widening_positions(line: &[&str], template: &[OwnedToken]) -> Vec<usize> {
    debug_assert_eq!(line.len(), template.len());
    line.iter()
        .zip(template.iter())
        .enumerate()
        .filter_map(|(i, (l, t))| match t {
            OwnedToken::Fixed(s) if s.as_str() != *l => Some(i),
            _ => None,
        })
        .collect()
}

/// RFC §6.4 degenerate-template guard. Returns `true` iff
/// applying `positions_widened` to `template` would leave the
/// template with zero `OwnedToken::Fixed(_)` positions.
///
/// `positions_widened` is sorted ascending by construction
/// ([`find_widening_positions`] walks indices left-to-right), so
/// we lockstep-walk both sequences in `O(N + M)` with no
/// allocation. The previous `.contains()`-inside-`.all()` shape
/// was `O(N · M)`.
fn would_be_degenerate(template: &[OwnedToken], positions_widened: &[usize]) -> bool {
    debug_assert!(
        positions_widened.windows(2).all(|w| w[0] < w[1]),
        "positions_widened must be sorted ascending (find_widening_positions invariant)",
    );
    let mut widen_iter = positions_widened.iter().copied();
    let mut next_widen = widen_iter.next();
    for (i, tok) in template.iter().enumerate() {
        match tok {
            OwnedToken::Wildcard => {} // already wildcard, doesn't contribute
            OwnedToken::Fixed(_) => {
                if next_widen == Some(i) {
                    // About to become wildcard via this widening.
                    next_widen = widen_iter.next();
                } else {
                    // A Fixed token survives this widening →
                    // not degenerate.
                    return false;
                }
            }
        }
    }
    true
}

/// Replace `Fixed` tokens at the given positions with
/// `Wildcard`, in place. Positions that are already `Wildcard`
/// are no-ops; positions not in the list are unchanged.
fn apply_widening(template: &mut [OwnedToken], positions: &[usize]) {
    for &pos in positions {
        if pos < template.len() {
            template[pos] = OwnedToken::Wildcard;
        }
    }
}

/// Convert `usize` positions to the `Vec<u16>` shape RFC §6.4
/// requires for `AuditEvent` payloads. Infallible: callers must
/// have already enforced the line-length cap so every position
/// fits.
fn positions_to_u16(positions: &[usize]) -> Vec<u16> {
    positions
        .iter()
        .map(|&p| {
            u16::try_from(p)
                .expect("line length capped at u16::MAX in ingest_string; positions fit")
        })
        .collect()
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
    use ourios_core::record::SharedRecordSink;

    /// Test helper — a `Body::String` record for `tenant` carrying
    /// `text` and default severity (UNSPECIFIED) / scope (None).
    /// Keeps tests focused on their assertions rather than on
    /// record-construction boilerplate.
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
        assert_eq!(events[0].template_id, id_in);
        let AuditEventKind::TemplateWidened {
            old_version,
            new_version,
            positions_widened,
            old_template,
            new_template,
        } = &events[0].kind
        else {
            panic!("expected TemplateWidened, got {:?}", events[0].kind);
        };
        assert_eq!(*old_version, 1);
        assert_eq!(*new_version, 2);
        assert_eq!(*positions_widened, vec![3]);
        assert_eq!(old_template, "user <NUM> logged in from <IP>");
        assert_eq!(new_template, "user <NUM> logged <*> from <IP>");
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
                .template
                .iter()
                .all(|t| matches!(t, OwnedToken::Fixed(_))),
            "no Wildcard tokens should be present without widening: {:?}",
            templates[0].template,
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
        let AuditEventKind::TemplateWidened {
            old_version,
            new_version,
            ..
        } = &events[0].kind
        else {
            panic!("expected TemplateWidened, got {:?}", events[0].kind);
        };
        assert_eq!(*old_version, 1);
        assert_eq!(*new_version, 2);
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
        let AuditEventKind::TemplateWidened {
            old_version: ov0,
            new_version: nv0,
            positions_widened: p0,
            ..
        } = &events[0].kind
        else {
            panic!(
                "event 0: expected TemplateWidened, got {:?}",
                events[0].kind
            );
        };
        assert_eq!((*ov0, *nv0, p0.clone()), (1, 2, vec![4]));
        let AuditEventKind::TemplateWidened {
            old_version: ov1,
            new_version: nv1,
            positions_widened: p1,
            ..
        } = &events[1].kind
        else {
            panic!(
                "event 1: expected TemplateWidened, got {:?}",
                events[1].kind
            );
        };
        assert_eq!((*ov1, *nv1, p1.clone()), (2, 3, vec![5]));
    }

    // ---------- §6.4 type expansion (PR-B-0) ----------

    #[test]
    fn literal_widening_seeds_slot_types_with_str_for_both_observations() {
        // Pre-widen Fixed token "in" and triggering literal "out"
        // are both classified as Str by `type_at_position`. The
        // newly-introduced wildcard's slot_types should be
        // {Str} (the singleton — Str ∪ Str = Str). No
        // TemplateTypeExpanded event fires; the existence of the
        // slot is covered by TemplateWidened alone.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let _ = cluster.ingest(&string_record(&t, "user 42 logged out"));

        let events = sink.drain();
        assert_eq!(events.len(), 1, "literal widening: one event only");
        assert!(matches!(
            events[0].kind,
            AuditEventKind::TemplateWidened { .. }
        ));

        let templates = cluster.templates_for(&t);
        assert_eq!(templates.len(), 1);
        assert_eq!(
            templates[0].slot_types.len(),
            1,
            "exactly one wildcard slot"
        );
        let types: Vec<_> = templates[0].slot_types[0].iter().collect();
        assert_eq!(types, vec![ParamType::Str]);
    }

    #[test]
    fn fixed_mask_tag_widening_captures_both_param_types_in_slot() {
        // CLAUDE.md §3.1 regression: a leaf with `Fixed("<NUM>")`
        // widened by a `<UUID>` line on main produces:
        //   - one TemplateWidened event (position 1)
        //   - slot_types[0] = {Num, Uuid} (both types observed
        //     during the widening, captured as the slot's initial
        //     state — neither counts as an "expansion" because the
        //     slot didn't exist pre-widen)
        // PR #32 had this case silently merging without any audit
        // signal because masked positions entered the leaf as
        // Wildcard from creation; PR-B-0 keeps mask tags as Fixed
        // so the Fixed-mismatch widening fires, with the slot type
        // information accumulated for future expansion checks.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        // Prefix ["user", "logged"] shared (positions 0–1 inside
        // prefix_depth=2). The mask-tag divergence sits at
        // position 2 so both lines route to the same leaf, where
        // sim_seq sees Fixed("<NUM>") vs "<UUID>" → Fixed
        // mismatch → widening at position 2.
        let _ = cluster.ingest(&string_record(&t, "user logged 42 in"));
        let _ = cluster.ingest(&string_record(
            &t,
            "user logged 550e8400-e29b-41d4-a716-446655440000 in",
        ));

        let events = sink.drain();
        assert_eq!(events.len(), 1, "single TemplateWidened, no expansion");
        let AuditEventKind::TemplateWidened {
            positions_widened, ..
        } = &events[0].kind
        else {
            panic!("expected TemplateWidened, got {:?}", events[0].kind);
        };
        assert_eq!(*positions_widened, vec![2]);

        let templates = cluster.templates_for(&t);
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].slot_types.len(), 1);
        let types: Vec<_> = templates[0].slot_types[0].iter().collect();
        assert_eq!(
            types,
            vec![ParamType::Uuid, ParamType::Num],
            "slot must record both the pre-widen Num and the triggering Uuid",
        );
    }

    #[test]
    fn clean_attach_at_typed_wildcard_with_known_type_emits_no_audit() {
        // After a widening that seeds slot_types[0] = {Num, Uuid}
        // (one Num line, one Uuid line), a third line with `<NUM>`
        // at the same position attaches cleanly with no events.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user logged 42 in"));
        let _ = cluster.ingest(&string_record(
            &t,
            "user logged 550e8400-e29b-41d4-a716-446655440000 in",
        ));
        let _ = sink.drain();

        // L3 — Num at position 2 (the wildcard) is already in the
        // slot's set, so no expansion event fires.
        let _ = cluster.ingest(&string_record(&t, "user logged 99 in"));

        assert!(
            sink.is_empty(),
            "known type at typed wildcard must not emit",
        );
        let templates = cluster.templates_for(&t);
        assert_eq!(
            templates[0].template_version, 2,
            "version stays at 2 — no expansion",
        );
    }

    #[test]
    fn clean_attach_at_typed_wildcard_with_new_type_emits_type_expanded() {
        // Seed slot_types[0] = {Str} via literal widening
        // ("in"/"out"), then ingest a `<NUM>` at the same position
        // — Num is not in {Str}, so TemplateTypeExpanded fires.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        // 5-token lines so the single-position widening keeps
        // sim_seq at 4/5 = 0.8 ≥ default threshold 0.7. Shorter
        // lines drop the similarity below threshold and split
        // into a fresh leaf via the Lossy zone (RFC §6.2 step 5b).
        let _ = cluster.ingest(&string_record(&t, "user logged at hour in"));
        let _ = cluster.ingest(&string_record(&t, "user logged at hour out"));
        let _ = sink.drain();

        // "13" masks to "<NUM>"; it lands at position 4 where the
        // leaf has a Wildcard with slot_types[0] = {Str}.
        let _ = cluster.ingest(&string_record(&t, "user logged at hour 13"));

        let events = sink.drain();
        assert_eq!(events.len(), 1, "exactly one TemplateTypeExpanded");
        let AuditEventKind::TemplateTypeExpanded {
            old_version,
            new_version,
            slots_expanded,
            ..
        } = &events[0].kind
        else {
            panic!("expected TemplateTypeExpanded, got {:?}", events[0].kind);
        };
        assert_eq!((*old_version, *new_version), (2, 3));
        assert_eq!(slots_expanded.len(), 1);
        assert_eq!(slots_expanded[0].slot_index, 0);
        assert_eq!(slots_expanded[0].added_types, vec![ParamType::Num]);

        let templates = cluster.templates_for(&t);
        let types: Vec<_> = templates[0].slot_types[0].iter().collect();
        assert_eq!(types, vec![ParamType::Num, ParamType::Str]);
    }

    #[test]
    fn type_expansion_only_attach_counts_toward_merges_total() {
        // RFC §6.4 — `merges_total` counts both `TemplateWidened`
        // and `TemplateTypeExpanded` (see
        // `AuditEventKind::counts_as_merge`). A pure type-expansion
        // attach must therefore bump the counter.
        let (mut cluster, _sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user logged at hour in"));
        let _ = cluster.ingest(&string_record(&t, "user logged at hour out"));
        let merges_after_widen = cluster.merges_total();

        let _ = cluster.ingest(&string_record(&t, "user logged at hour 13"));
        assert_eq!(
            cluster.merges_total(),
            merges_after_widen + 1,
            "type-expansion-only attach must bump merges_total",
        );
    }

    #[test]
    fn combined_widening_and_type_expansion_emits_two_events_in_order() {
        // RFC §6.2: a single attach can trigger BOTH structural
        // widening (Fixed mismatch) AND type expansion (new
        // ParamType at a pre-existing wildcard). In that case
        // template_version increments twice and two events emit
        // in widening-then-expansion order.
        //
        // Setup: leaf with one pre-existing Wildcard whose
        // slot_types = {Str}, plus a Fixed literal at another
        // position. The triggering line widens the literal
        // (literal-vs-literal Fixed mismatch) AND brings a `<NUM>`
        // to the pre-existing wildcard.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        // L1: fresh 5-token leaf [Fixed("user"), Fixed("logged"),
        //     Fixed("at"), Fixed("hour"), Fixed("NOW")], slots=[].
        //     Prefix ["user", "logged"] is shared.
        let _ = cluster.ingest(&string_record(&t, "user logged at hour NOW"));
        // L2: literal "NOW" → "LATER" widens position 4. sim_seq
        //     = 4/5 = 0.8 ≥ threshold. slot_types = [{Str}], v=2.
        let _ = cluster.ingest(&string_record(&t, "user logged at hour LATER"));
        let _ = sink.drain();

        // L3: Fixed mismatch at position 3 ("hour" → "minute")
        //     AND position 4's pre-existing wildcard sees "<NUM>"
        //     from "13". sim_seq = 4/5 = 0.8 → Clean. After
        //     widening pos 3 the slot ordinals are: position 3 →
        //     ordinal 0 (fresh, slot_types=[{Str}]), position 4
        //     → ordinal 1 (existing, slot_types was [{Str}]).
        //     Expansion fires at ordinal 1, adding Num.
        let _ = cluster.ingest(&string_record(&t, "user logged at minute 13"));

        let events = sink.drain();
        assert_eq!(events.len(), 2, "combined widening + type expansion");
        let AuditEventKind::TemplateWidened {
            old_version: w_old,
            new_version: w_new,
            positions_widened,
            ..
        } = &events[0].kind
        else {
            panic!(
                "event 0 must be TemplateWidened (widening fires before expansion), got {:?}",
                events[0].kind,
            );
        };
        assert_eq!((*w_old, *w_new), (2, 3));
        assert_eq!(*positions_widened, vec![3]);

        let AuditEventKind::TemplateTypeExpanded {
            old_version: e_old,
            new_version: e_new,
            slots_expanded,
            ..
        } = &events[1].kind
        else {
            panic!(
                "event 1 must be TemplateTypeExpanded, got {:?}",
                events[1].kind,
            );
        };
        assert_eq!((*e_old, *e_new), (3, 4));
        // Post-widen template: [Fixed, Fixed, Fixed, Wildcard,
        // Wildcard]. The freshly-widened slot at position 3 is
        // ordinal 0; the pre-existing slot at position 4 is
        // ordinal 1 — that's the one expanding.
        assert_eq!(slots_expanded.len(), 1);
        assert_eq!(slots_expanded[0].slot_index, 1);
        assert_eq!(slots_expanded[0].added_types, vec![ParamType::Num]);

        let templates = cluster.templates_for(&t);
        assert_eq!(
            templates[0].template_version, 4,
            "version is 4 after both bumps",
        );
        assert_eq!(templates[0].slot_types.len(), 2, "two wildcard slots");
    }

    #[test]
    fn slot_types_are_aligned_by_wildcard_ordinal_not_template_position() {
        // The leaf's `slot_types[k]` is the type set for the k-th
        // Wildcard from the left (ordinal), not for template
        // position k. Pin the invariant by widening positions
        // out-of-order across multiple attaches and checking the
        // resulting alignment.
        let (mut cluster, _sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        // L1: 5-token leaf [Fixed("user"), Fixed("logged"),
        //     Fixed("in"), Fixed("fast"), Fixed("NOW")], v=1.
        //     Prefix ["user", "logged"] is shared.
        let _ = cluster.ingest(&string_record(&t, "user logged in fast NOW"));
        // L2: widen position 3 ("fast" → "slow"). sim_seq = 4/5
        //     ≥ threshold. slot_types = [{Str}] (one wildcard at
        //     ordinal 0).
        let _ = cluster.ingest(&string_record(&t, "user logged in slow NOW"));
        // L3: widen position 2 ("in" → "out"). Position 3 is a
        //     pre-existing Wildcard matching "slow". sim_seq =
        //     4/5 ≥ threshold. The new wildcard at template
        //     position 2 is inserted at ordinal 0 (it sits left
        //     of position 3's existing wildcard, now ordinal 1),
        //     so slot_types = [{Str (new at pos 2)}, {Str (old
        //     at pos 3)}].
        let _ = cluster.ingest(&string_record(&t, "user logged out slow NOW"));

        let templates = cluster.templates_for(&t);
        assert_eq!(templates[0].slot_types.len(), 2);
        // Both slots are Str-only (literal widenings).
        for (i, st) in templates[0].slot_types.iter().enumerate() {
            let types: Vec<_> = st.iter().collect();
            assert_eq!(
                types,
                vec![ParamType::Str],
                "slot {i}: literal widening seeds only Str",
            );
        }
    }

    #[test]
    fn silent_merge_across_mask_tag_types_is_audited_not_silent() {
        // Regression for the CLAUDE.md §3.1 violation that closed
        // PR #32. Two lines differing only by mask-tag type at a
        // position beyond `prefix_depth` (default 2) MUST produce
        // an audit signal, not a silent merge.
        //
        // - Line A: "GET /home 42 ok" — masks <NUM> at position 2.
        // - Line B: "GET /home <UUID-string> ok" — masks <UUID>.
        //
        // Pre-PR-B-0: same tree path (length=4, prefix=GET/home);
        // sim_seq with the leaf's Fixed("<NUM>") at position 2 vs
        // the line's "<UUID>" → Fixed mismatch → TemplateWidened
        // fires. slot_types[0] then captures {Num, Uuid}.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "GET /home 42 ok"));
        let _ = cluster.ingest(&string_record(
            &t,
            "GET /home 550e8400-e29b-41d4-a716-446655440000 ok",
        ));

        let events = sink.drain();
        assert!(
            !events.is_empty(),
            "§3.1: mask-tag type change at a tree-routed wildcard slot must audit",
        );
        let AuditEventKind::TemplateWidened {
            positions_widened, ..
        } = &events[0].kind
        else {
            panic!("expected TemplateWidened, got {:?}", events[0].kind);
        };
        assert_eq!(*positions_widened, vec![2]);

        // Confirm the slot's type set captures the divergence so a
        // *third* mask-tag type at the same position would emit
        // TemplateTypeExpanded.
        let templates = cluster.templates_for(&t);
        let types: Vec<_> = templates[0].slot_types[0].iter().collect();
        assert_eq!(types, vec![ParamType::Uuid, ParamType::Num]);
    }

    #[test]
    fn literal_mask_tag_token_in_line_classifies_as_str_not_num() {
        // Regression for PR #33's review feedback. If a log line
        // literally contains the token `"<NUM>"` (e.g., a
        // placeholder a developer wrote into the message),
        // `mask()` does NOT classify it (the digit rule doesn't
        // fire on non-digit strings). The cluster's per-position
        // type classification therefore reports `Str` for that
        // position, NOT `Num`.
        //
        // Setup at position 3: literal widening seeds
        // slot_types[0] = {Str}. Then a third line brings the
        // literal `"<NUM>"` at the same position. The classifier
        // must read mask's `wildcard_positions` (empty for this
        // position, since the rule didn't fire) and conclude
        // `Str`, which is already in the slot's set — no audit
        // event. The pre-fix code would have inferred `Num` from
        // the masked-token string content and incorrectly fired
        // `TemplateTypeExpanded`, corrupting slot_types.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user logged at hour in"));
        let _ = cluster.ingest(&string_record(&t, "user logged at hour out"));
        let _ = sink.drain();

        // The third line's position-4 token is the literal
        // string "<NUM>". mask() leaves it alone (not all-digits).
        let _ = cluster.ingest(&string_record(&t, "user logged at hour <NUM>"));

        assert!(
            sink.is_empty(),
            "literal `<NUM>` must be Str (already in slot's set), not a spurious Num expansion",
        );
        let templates = cluster.templates_for(&t);
        assert_eq!(
            templates[0].template_version, 2,
            "no version bump: the literal `<NUM>` did not introduce a new type",
        );
        let types: Vec<_> = templates[0].slot_types[0].iter().collect();
        assert_eq!(
            types,
            vec![ParamType::Str],
            "slot_types stays at {{Str}} — Num must not leak in from a literal-tag token",
        );
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
        //  - increments body_retentions_total — §6.4 "treated as
        //    a parse failure ... retain body"
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
        assert_eq!(
            cluster.body_retentions_total(),
            1,
            "§6.4 says degenerate-rejected lines retain body",
        );

        let events = sink.drain();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(events[0].kind, AuditEventKind::TemplateWidened { .. }),
            "event 0: expected TemplateWidened, got {:?}",
            events[0].kind,
        );
        // Rejection variant carries no version bump and surfaces
        // the would-be template the operator was protected from.
        let AuditEventKind::TemplateWideningRejectedDegenerate {
            would_be_template, ..
        } = &events[1].kind
        else {
            panic!(
                "event 1: expected TemplateWideningRejectedDegenerate, got {:?}",
                events[1].kind,
            );
        };
        assert_eq!(would_be_template, "<*> <*> <*>");

        // Leaf state was not mutated by the rejection — still has
        // its post-widening template (1 Fixed at position 0).
        let templates = cluster.templates_for(&t);
        assert_eq!(templates.len(), 1);
        let leaf_template = &templates[0].template;
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
            cluster.body_retentions_total(),
            2,
            "empty input is still a parse failure that retains body \
             (RFC §6.3: every parse-failure path bumps both counters)",
        );
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
        // Under the default 0.7 threshold + 0.5 floor:
        //
        //   L1 = "alpha beta gamma delta epsilon"  → leaf A.
        //   L2 = "alpha beta gamma rho sigma"      → sim with A = 3/5
        //                                            = 0.6 ∈ [0.5, 0.7)
        //                                            → lossy zone →
        //                                            new leaf B (same
        //                                            `(length, prefix)`
        //                                            bucket).
        //   L3 = "alpha beta gamma delta zeta"     → sim with A = 4/5
        //                                            = 0.8 (clean), sim
        //                                            with B = 3/5 = 0.6
        //                                            (lossy). Best
        //                                            candidate is A;
        //                                            widens A at
        //                                            position 4.
        //
        // (Pre-three-zone this test had L2 at sim 0.4 — that's
        // now a parse failure rather than a leaf, so L2 was
        // rewritten to land in the lossy zone where the
        // best-candidate-selection question still makes sense.)
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let id_a = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon"));
        let id_b = cluster.ingest(&string_record(&t, "alpha beta gamma rho sigma"));
        assert_ne!(id_a, id_b, "leaves are distinct after L2");
        assert_eq!(cluster.template_count(&t), 2);
        assert!(
            sink.is_empty(),
            "L2 fell into the lossy zone → fresh leaf, no widening",
        );
        assert_eq!(
            cluster.body_retentions_total(),
            1,
            "L2's lossy attach is one body retention",
        );

        let id_c = cluster.ingest(&string_record(&t, "alpha beta gamma delta zeta"));
        // Must widen leaf A (sim 0.8, clean), not B (sim 0.6, lossy).
        assert_eq!(
            id_c, id_a,
            "best-candidate selection must pick the higher-similarity leaf",
        );
        assert_eq!(cluster.merges_total(), 1);
        let events = sink.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].template_id, id_a);
        let AuditEventKind::TemplateWidened {
            positions_widened, ..
        } = &events[0].kind
        else {
            panic!("expected TemplateWidened, got {:?}", events[0].kind);
        };
        assert_eq!(*positions_widened, vec![4]);
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

    #[test]
    fn ingest_string_routes_lines_above_u16_max_tokens_to_parse_failure() {
        // The cap defends `positions_widened: Vec<u16>` (RFC §6.4)
        // from a silent-merge bug: if the helper had to drop
        // out-of-range positions, an attach with no surviving
        // mismatches would have looked like a clean match
        // (no widening, no audit, no `merges_total` bump).
        // Producing a 65 537-token line here is the smallest input
        // that exercises the cap.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        let n: usize = (u16::MAX as usize) + 2;
        let mut text = String::with_capacity(n * 2);
        for i in 0..n {
            if i > 0 {
                text.push(' ');
            }
            text.push('x');
        }

        let id = cluster.ingest(&string_record(&t, &text));

        assert_eq!(id, NO_TEMPLATE);
        assert_eq!(cluster.template_count(&t), 0);
        assert_eq!(cluster.parse_failures_total(), 1);
        assert_eq!(
            cluster.body_retentions_total(),
            1,
            "RFC §6.3: over-cap lines retain body alongside the parse-failure count",
        );
        assert_eq!(cluster.merges_total(), 0);
    }

    // ---------- three-zone confidence (RFC §6.3) ----------

    #[test]
    fn lossy_zone_creates_new_leaf_and_bumps_body_retention() {
        // L1 = "alpha beta gamma delta epsilon"  (length 5,
        //                                         prefix "alpha beta").
        // L2 = "alpha beta gamma rho sigma"      → sim with L1 = 3/5 = 0.6
        //                                         → lossy zone
        //                                         (0.4 ≤ 0.6 < 0.7 under
        //                                         the RFC §6.3 defaults).
        //
        // Lossy attach: new leaf in the same parent (not widening),
        // body_retentions_total bumps by one, no audit event, no
        // merges_total bump.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let id1 = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon"));
        let id2 = cluster.ingest(&string_record(&t, "alpha beta gamma rho sigma"));

        assert_ne!(id1, id2, "lossy attach creates a distinct leaf");
        assert_eq!(cluster.template_count(&t), 2);
        assert_eq!(cluster.body_retentions_total(), 1);
        assert_eq!(cluster.merges_total(), 0);
        assert_eq!(cluster.parse_failures_total(), 0);
        assert!(sink.is_empty(), "lossy attach emits no audit event");
    }

    #[test]
    fn parse_failure_zone_returns_no_template_and_bumps_counters() {
        // L1 = "alpha beta gamma delta epsilon zeta"  (length 6).
        // L2 = "alpha beta phi rho sigma omega"        → sim with L1 = 2/6
        //                                              ≈ 0.333 →
        //                                              parse-failure zone
        //                                              (< 0.4 RFC §6.3 floor).
        //
        // Pre-§6.3 PR draft used length-5 lines with sim 0.4 and
        // a 0.5 floor — that boundary collapsed once the floor
        // was corrected to the RFC-pinned 0.4. Lengthening L2 by
        // one token (sim 2/6 instead of 2/5) lands the line
        // unambiguously below the floor without re-introducing a
        // boundary-dependent assertion.
        //
        // Parse failure: no leaf created, NO_TEMPLATE returned,
        // parse_failures_total AND body_retentions_total both
        // bump (RFC §6.3 says parse failure also retains body).
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let id1 = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon zeta"));
        let id2 = cluster.ingest(&string_record(&t, "alpha beta phi rho sigma omega"));

        assert_ne!(id1, NO_TEMPLATE, "L1 created the only leaf");
        assert_eq!(
            id2, NO_TEMPLATE,
            "below-floor similarity → parse failure, not new leaf",
        );
        assert_eq!(
            cluster.template_count(&t),
            1,
            "parse failure must not allocate a leaf",
        );
        assert_eq!(cluster.parse_failures_total(), 1);
        assert_eq!(
            cluster.body_retentions_total(),
            1,
            "RFC §6.3: parse failure retains body too",
        );
        assert_eq!(cluster.merges_total(), 0);
        assert!(sink.is_empty(), "parse failure emits no audit event");
    }

    #[test]
    fn clean_attach_does_not_bump_body_retentions() {
        // sim ≥ threshold → ConfidenceZone::Clean →
        // retains_body() == false → counter unchanged.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Two structurally identical (post-mask) lines: sim == 1.0,
        // clean attach to the existing leaf.
        let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let _ = cluster.ingest(&string_record(&t, "user 17 logged in"));

        assert_eq!(cluster.body_retentions_total(), 0);
        assert_eq!(cluster.parse_failures_total(), 0);
        assert_eq!(cluster.template_count(&t), 1);
    }

    #[test]
    fn floor_at_threshold_collapses_lossy_zone() {
        // With floor == threshold, the lossy zone is empty. Every
        // below-threshold attach goes straight to parse failure.
        // Pin the corner case so a future config refactor that
        // accidentally re-introduces the lossy zone is caught.
        let config = MinerConfig::try_new_full(0.7, 0.7, 256).expect("valid config");
        let mut cluster = MinerCluster::new(config);
        let t = TenantId::new("tenant-x");

        // L1 establishes the leaf; L2 has sim 3/5 = 0.6 — under
        // the collapsed-zone config this is < floor, so parse
        // failure (not lossy, since lossy zone is empty).
        let _id1 = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon"));
        let id2 = cluster.ingest(&string_record(&t, "alpha beta gamma rho sigma"));

        assert_eq!(id2, NO_TEMPLATE);
        assert_eq!(cluster.template_count(&t), 1);
        assert_eq!(cluster.parse_failures_total(), 1);
        assert_eq!(cluster.body_retentions_total(), 1);
    }

    // ---------- record emission (RFC §6.1 / §6.6 scaffolding) ----------

    /// Test helper — a cluster whose audit and record sinks are
    /// both `SharedAuditSink`/`SharedRecordSink` clones so tests
    /// can inspect what was emitted on both streams.
    fn cluster_with_observable_sinks() -> (MinerCluster, SharedAuditSink, SharedRecordSink) {
        let audit = SharedAuditSink::new();
        let records = SharedRecordSink::new();
        let cluster =
            MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()))
                .with_record_sink(Box::new(records.clone()));
        (cluster, audit, records)
    }

    #[test]
    fn body_none_emits_absent_record_with_no_template() {
        let records = SharedRecordSink::new();
        let mut cluster =
            MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
        let t = TenantId::new("tenant-x");

        let r = OtlpLogRecord {
            tenant_id: t.clone(),
            body: None,
            ..Default::default()
        };
        let id = cluster.ingest(&r);

        assert_eq!(id, NO_TEMPLATE);
        let emitted = records.drain();
        assert_eq!(emitted.len(), 1);
        let rec = &emitted[0];
        assert_eq!(rec.tenant_id, t);
        assert_eq!(rec.template_id, NO_TEMPLATE);
        assert_eq!(rec.body_kind, BodyKind::Absent);
        assert!(rec.lossy_flag, "Body::None records are lossy (no template)");
        assert!(rec.separators.is_empty());
        assert!(rec.params.is_empty());
        assert!(rec.body.is_none());
    }

    #[test]
    fn clean_fresh_leaf_emits_record_with_separators_and_no_body() {
        let (mut cluster, _audit, records) = cluster_with_observable_sinks();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));

        let emitted = records.drain();
        assert_eq!(emitted.len(), 1);
        let rec = &emitted[0];
        assert_eq!(rec.body_kind, BodyKind::String);
        assert_ne!(rec.template_id, NO_TEMPLATE);
        assert_eq!(rec.template_version, 1);
        // tokenize("user 42 logged in") yields 4 tokens → 5
        // separators per the §6.6 capture invariant.
        assert_eq!(rec.separators.len(), 5);
        // Clean attaches do not retain body and are not lossy.
        assert!(rec.body.is_none());
        assert!(!rec.lossy_flag);
        // sim_seq against a fresh leaf is 1.0 by definition;
        // confidence = sim / threshold = 1.0 / 0.7 ≈ 1.428, but
        // the cluster reports the sentinel 1.0 for clean attaches.
        assert!((rec.confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn clean_reuse_emits_record_at_same_template_id_and_version() {
        let (mut cluster, _audit, records) = cluster_with_observable_sinks();
        let t = TenantId::new("tenant-x");

        let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let id2 = cluster.ingest(&string_record(&t, "user 17 logged in"));

        assert_eq!(id1, id2, "reuse same template");
        let emitted = records.drain();
        assert_eq!(emitted.len(), 2);
        assert_eq!(emitted[0].template_id, id1);
        assert_eq!(emitted[1].template_id, id1);
        assert_eq!(emitted[0].template_version, 1);
        assert_eq!(
            emitted[1].template_version, 1,
            "clean reuse must not bump the version",
        );
    }

    #[test]
    fn widening_emits_record_with_bumped_version() {
        let (mut cluster, _audit, records) = cluster_with_observable_sinks();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));
        let _ = cluster.ingest(&string_record(&t, "user 42 logged out from 10.0.0.1"));

        let emitted = records.drain();
        assert_eq!(emitted.len(), 2);
        assert_eq!(emitted[0].template_version, 1, "L1 at fresh-leaf version");
        assert_eq!(
            emitted[1].template_version, 2,
            "L2's widening bumps version on the same template_id",
        );
        assert_eq!(
            emitted[0].template_id, emitted[1].template_id,
            "widening attaches to the same template_id",
        );
    }

    #[test]
    fn lossy_attach_emits_record_with_retained_body_and_lossy_flag_false() {
        let (mut cluster, _audit, records) = cluster_with_observable_sinks();
        let t = TenantId::new("tenant-x");

        // L2 = sim 3/5 = 0.6 ∈ [0.4, 0.7) → lossy zone.
        let _ = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon"));
        let l2_raw = "alpha beta gamma rho sigma";
        let _ = cluster.ingest(&string_record(&t, l2_raw));

        let emitted = records.drain();
        assert_eq!(emitted.len(), 2);
        let lossy = &emitted[1];
        assert_eq!(lossy.body.as_deref(), Some(l2_raw));
        // §6.6: the lossy zone retains body but `lossy_flag`
        // stays false — reconstruction is expected to match.
        assert!(!lossy.lossy_flag);
        // confidence = sim / threshold = 0.6 / 0.7.
        let expected_conf = 0.6_f32 / 0.7_f32;
        assert!(
            (lossy.confidence - expected_conf).abs() < 1e-4,
            "expected confidence ≈ {expected_conf}, got {}",
            lossy.confidence,
        );
        // Lossy attach creates a fresh leaf, so version is 1.
        assert_eq!(lossy.template_version, 1);
        assert_ne!(lossy.template_id, NO_TEMPLATE);
    }

    #[test]
    fn parse_failure_zone_emits_record_with_lossy_flag_and_no_template() {
        let (mut cluster, _audit, records) = cluster_with_observable_sinks();
        let t = TenantId::new("tenant-x");

        // sim 2/6 ≈ 0.333 < 0.4 floor → parse-failure zone.
        let _ = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon zeta"));
        let l2_raw = "alpha beta phi rho sigma omega";
        let _ = cluster.ingest(&string_record(&t, l2_raw));

        let emitted = records.drain();
        assert_eq!(emitted.len(), 2);
        let pf = &emitted[1];
        assert_eq!(pf.template_id, NO_TEMPLATE);
        assert_eq!(pf.template_version, 0);
        assert!(pf.lossy_flag, "parse-failure records are lossy");
        assert_eq!(pf.body.as_deref(), Some(l2_raw));
        assert!(
            pf.confidence.abs() < f32::EPSILON,
            "parse-failure confidence is the 0.0 sentinel",
        );
    }

    #[test]
    fn empty_input_emits_parse_failure_record() {
        let records = SharedRecordSink::new();
        let mut cluster =
            MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, ""));

        let emitted = records.drain();
        assert_eq!(emitted.len(), 1);
        let rec = &emitted[0];
        assert_eq!(rec.template_id, NO_TEMPLATE);
        assert!(rec.lossy_flag);
        assert_eq!(rec.body.as_deref(), Some(""));
        // §6.6 capture invariant on the degenerate case: empty
        // input still has separators.len() == tokens.len() + 1.
        assert_eq!(rec.separators.len(), 1);
    }

    #[test]
    fn structured_body_emits_record_with_structured_kind() {
        let records = SharedRecordSink::new();
        let mut cluster =
            MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));

        let emitted = records.drain();
        assert_eq!(emitted.len(), 1);
        let rec = &emitted[0];
        assert_eq!(rec.body_kind, BodyKind::Structured);
        assert_ne!(rec.template_id, NO_TEMPLATE);
        assert_eq!(rec.template_version, 1);
        // Structured records have no token-shape to carry; PR-A
        // leaves body=None (canonical-JSON encoding is the §6.6
        // follow-up's job).
        assert!(rec.separators.is_empty());
        assert!(rec.params.is_empty());
        assert!(rec.body.is_none());
        assert!(!rec.lossy_flag);
    }

    #[test]
    fn default_sink_drops_records_silently() {
        // `MinerCluster::new` defaults to `NoOpRecordSink`; tests
        // that don't opt into `with_record_sink` simply see no
        // records (the cluster doesn't crash, doesn't allocate,
        // doesn't expose state). Pins the production-safe default.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");
        let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
        // No assertion beyond "the call succeeded" — the contract
        // is no public observable side effect.
        assert_eq!(cluster.template_count(&t), 1);
    }
}
