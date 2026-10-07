//! Per-tenant template cluster.
//!
//! Holds one `TenantState` per [`TenantId`] (`[CLAUDE.md §3.7]`):
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
//!   bucket → fresh leaf, emitting a `TemplateChange::Created` audit
//!   event (`event_type` `template_created`, RFC 0017 §3.1; not a merge).
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
//! # Out of this crate / not yet wired
//!
//! - Parquet records for the mined records themselves (those go
//!   through `ourios-parquet`).
//! - A WAL-backed audit/record sink (RFC §6.4); today an in-memory
//!   placeholder stands in (see above).
//! - The §6.9 recovery driver (snapshot load + WAL-tail replay)
//!   lives in the ingester; the cluster's halves of that contract
//!   are [`MinerCluster::snapshot_state`] and
//!   [`MinerCluster::restore_tenant`].
//!
//! [`Tree`]: crate::tree::Tree
//! [`AuditSink`]: ourios_core::audit::AuditSink
//! [`TemplateChange`]: ourios_core::audit::TemplateChange

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use ourios_config::MinerConfig;
use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, ParamType, Provenance, ProvenanceSet, SlotTypes,
    TemplateChange, hash_triggering_line, sample_first_256_bytes,
};
use ourios_core::clock::Clock;
use ourios_core::confidence::ConfidenceZone;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::record::{BodyKind, MinedRecord, Param, RecordSink};
use ourios_core::tenant::TenantId;

use ourios_config::UpstreamTemplates;

use crate::mask::{TypedParam, mask};
use crate::metrics::{MinerMetrics, service_of};
use crate::sim_seq::sim_seq_owned;
use crate::snapshot::WalHighWater;
use crate::tokenize::tokenize;
use crate::tree::{Leaf, OwnedToken, Tree, UpstreamAssociations, format_template};
use crate::upstream;

use plan::{
    AttachPlan, Candidate, adopted_row_parts, leaf_template, params_from_mask, plan_attach,
    separators_to_owned, upstream_template_of,
};

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
/// Real templates always have id `>= 1` (see `IdRange::new`).
pub const NO_TEMPLATE: u64 = 0;

/// A multi-tenant in-memory miner.
///
/// Holds one `TenantState` per [`TenantId`]; per-tenant state
/// is allocated lazily on the first `ingest` call for that
/// tenant. Tenant deprovisioning (`TenantPaused`,
/// `TenantDeleted`) is RFC 0001 §9 territory and not in this
/// type's API yet.
pub struct MinerCluster {
    /// Cluster-default [`MinerConfig`] per RFC 0004 §3.4. Used
    /// for any tenant without a per-tenant override and as the
    /// fallback when [`Self::effective_config`] doesn't find a
    /// match.
    config: MinerConfig,
    /// Per-tenant overrides seeded via [`Self::with_tenant_config`]
    /// before first observation. RFC 0004 §3.4: the override is
    /// captured by [`TenantState`] at lazy allocation; entries
    /// here cease to be load-bearing once their tenant is seen
    /// (the cached `state.config` is the read path thereafter).
    tenant_overrides: HashMap<TenantId, MinerConfig>,
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
    // order — both invariants hold. Ids come only from durably
    // reserved blocks (`id_alloc`), so none is ever issued twice.
    ids: IdRange,
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
    // type-expansion PR lands ([`TemplateChange::counts_as_merge`]
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
    // §6.5 / §3.2 counter: per-parameter byte-limit overflow
    // events. Increments by the count of `Overflow`-tagged
    // [`Param`] entries on each emitted record. Read-side
    // placeholder for the §6.8 `ourios.miner.params.overflow`
    // *counter* metric — the §3.2
    // `ourios.miner.params.overflow.utilization` *gauge* is the
    // derived rolling ratio that lands alongside it once total
    // emitted params is tracked.
    params_overflow_total: AtomicU64,
    // Wall-clock source for audit-event `timestamp` stamping per
    // RFC §6.4. [`SystemClock`] in production; tests substitute
    // a [`ourios_core::clock::TestClock`] via
    // [`Self::with_clock`] for deterministic timestamp
    // assertions (wall-clock comparisons against `now()` flake
    // under NTP step / leap seconds / VM pause).
    clock: Box<dyn Clock>,
    // RFC §6.8 OTel instrument set, resolved through the
    // process-global `ourios.miner` meter. The atomic counters
    // above remain the in-process read path for tests / accessors;
    // these instruments are the exported telemetry surface (a
    // no-op when no meter provider is installed). The two are kept
    // in lockstep at the same emission sites.
    metrics: MinerMetrics,
    // Capture slot for [`Self::ingest_mined`] (RFC 0035 §3.1). The
    // ordered phase must run *exactly* `ingest`'s id assignment, audit
    // emission, counters, and metrics — forking that path would let the
    // two halves drift apart and silently break §3.5.3 determinism — so
    // instead of a parallel implementation, an `Armed` slot diverts the
    // single record `emit_record` would hand the record sink into
    // `Captured` for the caller to encode off the gate.
    mined_capture: MinedCapture,
    // Records salvaged out of the capture slot on a panic inside
    // [`Self::ingest_mined`] (forwarded to the record sink instead of
    // being dropped with the unwind). Expected to stay 0; a non-zero
    // value means a panic fired between capture and return.
    mined_capture_salvages: AtomicU64,
}

/// State of the one-record capture slot backing
/// [`MinerCluster::ingest_mined`]. Boxed so the enum stays small on the
/// `Off`/`Armed` paths.
enum MinedCapture {
    Off,
    Armed,
    Captured(Box<MinedRecord>),
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
/// `(severity_number, scope_name, event_name)` tuple shares one
/// `template_id` per RFC 0001 §6.1 *Template-key composition* as
/// extended by RFC 0037 §3.1 (the `BodyKind::Structured`
/// discriminator is implicit from the map itself). Including
/// `event_name` in the key means distinct event types — e.g. a
/// `gen_ai.client.inference.operation.details` inference event
/// versus a tool-call event in the same scope — get distinct
/// `template_id`s instead of collapsing to one. The map's value is
/// the `template_id` allocated on first observation of that tuple.
///
/// `template_id` allocation lives on [`MinerCluster`], not here
/// — see the `ids` comment there for why.
///
/// `template_count` is a cache of the number of templates this
/// tenant holds (tree leaves + structured-template entries),
/// incremented on every fresh allocation in
/// [`MinerCluster::ingest`]. The cache invariant is: every fresh
/// `template_id` allocation on this tenant (whether tree leaf or
/// structured map insert) increments the cache by exactly one.
/// Widening reuses an existing leaf's id, so it does not bump the
/// cache.
/// One entry of the RFC 0050 §3.3 adopted-template map, keyed on
/// `(canonical template, severity_number, scope_name)`.
#[derive(Debug)]
enum AdoptedEntry {
    /// The canonical shape already lives in the Drain tree (mined
    /// first, adopted second): adoption rides that leaf's id at the
    /// version whose tokens matched. Provenance and associations
    /// live on the leaf.
    TreeBacked {
        template_id: u64,
        template_version: u32,
    },
    /// Interned by adoption itself — the Drain tree gains no leaf
    /// (RFC0050.2). Always version 1; adopted templates never
    /// widen.
    Owned(OwnedAdopted),
}

#[derive(Debug)]
struct OwnedAdopted {
    template_id: u64,
    provenance: ProvenanceSet,
    associations: UpstreamAssociations,
}

/// Outcome of [`MinerCluster::resolve_adoption`] — resolution runs
/// under one tenant borrow; audit emission belongs to the caller.
enum AdoptResolution {
    /// The canonical was already resolved (either entry kind).
    Existing(u64, u32),
    /// First adoption converging onto an existing mined leaf —
    /// a provenance transition to audit.
    FirstOnLeaf(u64, u32),
    /// Freshly interned owned entry — the caller consumes the id
    /// allocation and audits.
    Interned(u64),
    /// The RFC 0023/0050 ceiling refused the intern.
    Ceiling,
    /// Interning needs a fresh id and none is reservable: the line is
    /// mined instead, which attaches or counts the failure.
    NoId,
}

/// One adopted canonical to resolve: what [`MinerCluster::resolve_adoption`]
/// reads.
struct AdoptQuery<'a> {
    upstream: &'a str,
    owned_tokens: &'a [OwnedToken],
    key: (String, u8, Option<String>),
    config: &'a MinerConfig,
}

/// A fresh leaf to mint for a string line: its masked form, the owned
/// parts its record carries, the RFC 0050 `observe` string, and its
/// §6.3 zone.
struct FreshLeaf<'a> {
    masked: MaskedLine<'a>,
    parts: LineParts,
    observed: Option<&'a str>,
    zone: FreshLeafZone,
}

struct TenantState {
    tree: Tree,
    structured_templates: HashMap<(u8, Option<String>, Option<String>), u64>,
    /// RFC 0050 §3.3 — canonical-shape identity for adopted
    /// templates, and the convergence cache that makes the
    /// adopt-time tree scan a once-per-canonical cost.
    adopted_templates: HashMap<(String, u8, Option<String>), AdoptedEntry>,
    /// [`AdoptedEntry::Owned`] entries only — the half of the
    /// RFC 0023 ceiling basis that is not `leaf_count` (RFC0050.5:
    /// adopted templates count against `max_templates`;
    /// tree-backed entries are already counted as leaves).
    owned_adopted_count: usize,
    /// The canonical shapes the tenant's **mined** leaves currently
    /// carry — the O(1) guard in front of the RFC0050.6 adopt-time
    /// convergence scan, so a stream of unique upstream templates
    /// (including one already at the ceiling) costs a hash miss per
    /// canonical instead of a tree walk. Maintained at leaf
    /// creation, on template-changing widenings, and on restore.
    mined_canonicals: canonicals::MinedCanonicals,
    template_count: usize,
    /// Drain-tree leaves only (excludes structured-template
    /// entries) — the quantity RFC 0023 §3.1's `max_templates`
    /// ceiling bounds. Incremented exactly where a leaf is pushed;
    /// rebuilt from `leaves.len()` on snapshot restore.
    leaf_count: usize,
    /// Effective [`MinerConfig`] for this tenant, captured at
    /// lazy allocation time. Resolves to the per-tenant override
    /// (set via [`MinerCluster::with_tenant_config`]) if one
    /// exists; otherwise the cluster default. RFC 0004 §3.4 pins
    /// this as the per-tenant tunable surface; further mutation
    /// after allocation is out of scope (the RFC names dynamic
    /// reconfiguration as an open question; today's contract is
    /// startup-only).
    config: MinerConfig,
    /// RFC 0052 §3.1's folded horizon: the WAL offset of this
    /// tenant's own last frame folded into the state above. Opaque
    /// here — the cluster never compares it — and `None` until a
    /// caller records one.
    folded: Option<WalHighWater>,
}

impl TenantState {
    fn new(config: MinerConfig) -> Self {
        Self {
            tree: Tree::new(),
            structured_templates: HashMap::new(),
            adopted_templates: HashMap::new(),
            owned_adopted_count: 0,
            mined_canonicals: canonicals::MinedCanonicals::default(),
            template_count: 0,
            leaf_count: 0,
            config,
            folded: None,
        }
    }

    /// RFC0050.6 "adopted first, mined second": take the owned
    /// adopted entry for this canonical, if any, flipping the map
    /// entry to tree-backed. The counts stay balanced from the
    /// caller's side: +1 leaf, −1 owned entry, net zero templates.
    fn take_converged_adoption(
        &mut self,
        key: &(String, u8, Option<String>),
    ) -> Option<OwnedAdopted> {
        if !matches!(
            self.adopted_templates.get(key),
            Some(AdoptedEntry::Owned(_))
        ) {
            return None;
        }
        let Some(AdoptedEntry::Owned(owned)) = self.adopted_templates.remove(key) else {
            unreachable!("guarded by the matches! above");
        };
        self.adopted_templates.insert(
            key.clone(),
            AdoptedEntry::TreeBacked {
                template_id: owned.template_id,
                template_version: 1,
            },
        );
        self.owned_adopted_count -= 1;
        Some(owned)
    }
}

/// A `Body::String` line in flight through the mining path: the
/// record it arrived on, its resolved `service.name`, and the raw
/// body bytes every §6.3 / §6.5 retention path keeps verbatim.
#[derive(Clone, Copy)]
struct StringLine<'a> {
    record: &'a OtlpLogRecord,
    service: Option<&'a str>,
    raw: &'a str,
}

/// A clean-zone attach (RFC 0001 §6.2 step 5): the chosen leaf, the
/// tenant's §6.5 param byte cap, and the record's RFC 0050 `observe`
/// association, if it carried one.
#[derive(Clone, Copy)]
struct Attach<'a> {
    candidate: Candidate,
    byte_limit: u32,
    observed: Option<&'a str>,
}

impl<'a> Attach<'a> {
    fn new(candidate: Candidate, config: &MinerConfig, observed: Option<&'a str>) -> Self {
        Self {
            candidate,
            byte_limit: config.param_byte_limit,
            observed,
        }
    }
}

/// What a string line contributes to its data record whichever way it
/// is mined: its separators and its line-ordered, byte-capped params.
/// Owned, because each exit path moves them into exactly one record.
struct LineParts {
    separators: Vec<String>,
    params: Vec<Param>,
}

/// A `Body::Structured` line in flight: the record, its resolved
/// `service.name`, and the body the §6.2 step-0 short-circuit keys on.
#[derive(Clone, Copy)]
struct StructuredLine<'a> {
    record: &'a OtlpLogRecord,
    service: Option<&'a str>,
    body: &'a ourios_core::otlp::AnyValue,
}

/// [`mask`]'s view of a line. `wildcard_positions` and
/// `typed_params` are parallel: one entry per mask-emitted slot.
#[derive(Clone, Copy)]
struct MaskedLine<'a> {
    strs: &'a [&'a str],
    wildcard_positions: &'a [usize],
    typed_params: &'a [TypedParam<'a>],
}

/// Which §6.3 zone a fresh-leaf mint came from — the axis
/// [`MinerCluster::emit_fresh_leaf_record`] varies on.
#[derive(Clone, Copy)]
enum FreshLeafZone {
    /// No candidate existed; clean by definition.
    Clean,
    /// The best candidate fell in the lossy zone; the mint retains
    /// the body and carries `similarity / threshold`.
    Lossy { confidence: f32 },
}

impl MinerCluster {
    /// Resolve the effective [`MinerConfig`] for `tenant_id`.
    /// Order of preference:
    ///
    /// 1. If `TenantState` already exists, its captured config
    ///    (set at lazy allocation, never mutated thereafter).
    /// 2. Else, the per-tenant override registered via
    ///    [`Self::with_tenant_config`].
    /// 3. Else, the cluster-default config.
    ///
    /// Returns by value (`MinerConfig` is `Copy`); no borrow.
    /// Callers that need to keep the tenants map borrowed during
    /// algorithm work should call this *before* descending into
    /// the per-tenant store.
    fn effective_config(&self, tenant_id: &TenantId) -> MinerConfig {
        self.tenants.get(tenant_id).map_or_else(
            || {
                self.tenant_overrides
                    .get(tenant_id)
                    .copied()
                    .unwrap_or(self.config)
            },
            |s| s.config,
        )
    }

    /// Apply RFC §6.5's "overflow forces body retention" rule to
    /// a record about to be emitted: if any `Param` carries
    /// `type_tag = Overflow`, set `body = Some(raw)` (overriding
    /// any previous setting) and bump `params_overflow_total` by
    /// the overflow count. No-op when the record has no overflow
    /// params.
    ///
    /// The override of `body` is intentional: even on paths that
    /// already retain body (lossy zone, parse-failure zone), the
    /// caller's `body` may be `None` or partial; an overflow
    /// record's body must always carry the original line bytes
    /// so `reconstruct()`'s `Overflow` branch (RFC §6.6) has
    /// something to fall back to.
    fn apply_overflow_retention(&self, line: StringLine<'_>, rec: &mut MinedRecord) {
        let StringLine {
            record,
            service,
            raw,
        } = line;
        let overflow_count = rec
            .params
            .iter()
            .filter(|p| p.type_tag == ParamType::Overflow)
            .count();
        if overflow_count > 0 {
            rec.body = Some(raw.to_string());
            #[allow(clippy::cast_possible_truncation)]
            let count = overflow_count as u64;
            self.params_overflow_total
                .fetch_add(count, Ordering::Relaxed);
            self.metrics
                .record_overflow(&record.tenant_id, service, count);
        }
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
    ///
    /// The §6.6 `lossy_flag = true` tokenizer-failure path is
    /// **not** routed through this helper — its body retention is
    /// the orthogonal "reconstruction impossible" case the
    /// `body_retentions_total` metric doc explicitly excludes,
    /// not the §6.3 lossy-zone retention the gauge is meant to
    /// surface. That path uses [`Self::record_tokenizer_failure`].
    fn record_parse_failure(
        &self,
        record: &OtlpLogRecord,
        service: Option<&str>,
        reason: &'static str,
    ) {
        self.parse_failures_total.fetch_add(1, Ordering::Relaxed);
        self.body_retentions_total.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .record_parse_failure(&record.tenant_id, service, reason);
        self.metrics.record_body_retention(&record.tenant_id);
    }

    /// Mark one tokenizer-failure event per RFC §6.6: increments
    /// `parse_failures_total` only. The orthogonal `lossy_flag =
    /// true` semantics mean the body IS retained on the emitted
    /// record (so a reader can surface it verbatim), but this
    /// retention is *not* the §3.1 `body_retention_ratio`
    /// numerator — that gauge counts the §6.3 retention paths,
    /// not the §6.6 reconstruction-impossible paths. Counting
    /// tokenizer failures here would inflate the ratio with
    /// events that aren't body-retention events in the
    /// gauge-contract sense.
    fn record_tokenizer_failure(&self, record: &OtlpLogRecord, service: Option<&str>) {
        self.parse_failures_total.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .record_parse_failure(&record.tenant_id, service, "tokenizer_failure");
    }

    /// Build the OTLP-envelope half of a `MinedRecord` from the
    /// incoming `OtlpLogRecord`. The mining-output fields
    /// (`template_id`, `template_version`, `params`,
    /// `separators`, `body`, `confidence`, `lossy_flag`) are left
    /// at their zero / sentinel defaults; the calling site
    /// customises before calling [`Self::emit_record`].
    ///
    /// **Per-record clone cost (deferred optimisation).** The
    /// `attributes` and `resource_attributes` vectors are
    /// `.clone()`-d once per emitted record. For corpus / bench
    /// inputs today the vectors are empty (`Vec::clone` on an
    /// empty `Vec` is essentially free), so there's no measured
    /// cost. Once the RFC 0003 receiver populates them — and
    /// especially `resource_attributes`, which is typically
    /// identical across every record in a `ResourceLogs` group —
    /// the deep clone becomes a hot-path concern worth measuring.
    /// The shape options at that point (per-record-borrowed,
    /// `Arc<[KeyValue]>` interning, take-ownership-from-receiver)
    /// are RFC 0003 / `ourios-ingester` territory; pinning a
    /// shape here would optimise without data. The
    /// [`MinedRecord`] field type stays plain `Vec<KeyValue>` for
    /// now so it mirrors `OtlpLogRecord`'s shape exactly.
    fn record_envelope(record: &OtlpLogRecord, body_kind: BodyKind) -> MinedRecord {
        MinedRecord {
            tenant_id: record.tenant_id.clone(),
            template_id: NO_TEMPLATE,
            template_version: 0,
            severity_number: record.severity_number,
            severity_text: record.severity_text.clone(),
            scope_name: record.scope_name.clone(),
            scope_version: record.scope_version.clone(),
            scope_attributes: record.scope_attributes.clone(),
            resource_schema_url: record.resource_schema_url.clone(),
            scope_schema_url: record.scope_schema_url.clone(),
            time_unix_nano: record.time_unix_nano,
            observed_time_unix_nano: record.observed_time_unix_nano,
            attributes: record.attributes.clone(),
            dropped_attributes_count: record.dropped_attributes_count,
            resource_attributes: record.resource_attributes.clone(),
            trace_id: record.trace_id,
            span_id: record.span_id,
            flags: record.flags,
            event_name: record.event_name.clone(),
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
    ///
    /// One emitted record is one ingested line, so this is also the
    /// single site that observes the §6.8 per-line `confidence`
    /// histogram + p50/p01 reservoir. The ratio-gauge denominators
    /// are bumped earlier, at the top of [`Self::ingest`], so they
    /// lead any numerator for the same line.
    fn emit_record(&mut self, record: MinedRecord, service: Option<&str>) {
        self.metrics
            .record_line(&record.tenant_id, service, f64::from(record.confidence));
        // An armed capture slot diverts the record to `ingest_mined`'s
        // caller instead of the sink (RFC 0035 §3.1) — after the metrics
        // above, so the two ingest entry points observe identically.
        if matches!(self.mined_capture, MinedCapture::Armed) {
            self.mined_capture = MinedCapture::Captured(Box::new(record));
            return;
        }
        self.record_sink.emit(record);
    }
}

mod build;
mod canonicals;
mod id_alloc;
mod persist;
mod plan;
mod structured;

use id_alloc::IdRange;
pub use id_alloc::{
    ID_RESERVATION_FAILED, IdBlock, IdReservationError, IdReserver, IdSpaceExhausted,
    MAX_TEMPLATE_ID,
};
pub use persist::{AdoptedSnapshot, LeafSnapshot, RestoreError};

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
    ///   `(severity_number, scope_name, event_name)` per §6.1 as
    ///   extended by RFC 0037 §3.1 (`BodyKind::Structured` is
    ///   implicit), and the same tuple reuses the same id on
    ///   subsequent records. Structured records never widen and
    ///   never emit audit events.
    /// - `None` — the wire delivered no body. Returns
    ///   [`NO_TEMPLATE`]; no allocation, no audit.
    ///
    /// On first sight of `record.tenant_id`, allocates a fresh
    /// per-tenant store.
    pub fn ingest(&mut self, record: &OtlpLogRecord) -> u64 {
        let started = std::time::Instant::now();
        // Resolve the source service once per ingest. It is
        // constant for the record and feeds every §6.8 per-service
        // instrument below; the hot-path helpers take the borrowed
        // `Option<&str>` rather than re-scanning + re-allocating it.
        // `None` when the source set no `service.name` — `ourios.service`
        // is then omitted (recommended, not synthesized).
        let service = service_of(&record.resource_attributes);
        let service = service.as_deref();
        // Bump the per-line denominators before any per-line
        // numerator (overflow / body-retention) for this line. The
        // ratio-gauge callbacks may collect concurrently; denominator
        // first keeps utilization ∈ [0, 1] at every collection point.
        // Exactly one record is emitted per ingest, so this matches
        // the `record_line` confidence observation one-to-one.
        self.metrics
            .record_line_denominator(&record.tenant_id, service);
        let template_id = match &record.body {
            None => {
                // The wire delivered no body. Emit a single
                // record with `BodyKind::Absent` and the
                // template-id sentinel; tokenize/mask didn't
                // run, so there's no separator / param info to
                // carry. `lossy_flag = false` per RFC 0025 §3.1:
                // absence is not loss — reconstruction is defined
                // and total (it renders nothing).
                let rec = Self::record_envelope(record, BodyKind::Absent);
                self.emit_record(rec, service);
                NO_TEMPLATE
            }
            Some(Body::String(raw)) => self.ingest_string(StringLine {
                record,
                service,
                raw,
            }),
            Some(Body::Structured(body)) => self.ingest_structured(StructuredLine {
                record,
                service,
                body,
            }),
        };
        // §6.8 `ourios.miner.duration` histogram (hot-path budget
        // D1) and the `ourios.miner.template.count` observable-gauge
        // mirror. Both read the post-ingest state, so they sit after
        // the body fork.
        self.metrics
            .record_duration(&record.tenant_id, started.elapsed().as_secs_f64());
        // `usize` ≤ `u64` on every supported target; saturate
        // rather than panic on the impossible overflow.
        let count = u64::try_from(self.template_count(&record.tenant_id)).unwrap_or(u64::MAX);
        self.metrics.set_template_count(&record.tenant_id, count);
        template_id
    }

    /// The ordered-phase half of [`Self::ingest`] (RFC 0035 §3.1). Runs
    /// the identical Drain match, template-id assignment, audit emission
    /// (template created / widened / type-expanded stay in this ordered
    /// phase — they are id-assignment events, RFC 0001 §6.4), counters,
    /// and metrics, but the one [`MinedRecord`] `ingest` would hand the
    /// record sink is **returned to the caller** instead — so the
    /// order-insensitive Parquet encode can run on a concurrent pool off
    /// the global gate.
    ///
    /// `None` for the mined record is unreachable in practice (every
    /// `ingest` emits exactly one record); it is surfaced rather than
    /// `expect`ed so a panic path that left the slot armed cannot take
    /// the miner down a second time.
    ///
    /// # Panics
    ///
    /// Re-raises any panic from the underlying [`Self::ingest`] — but
    /// only after settling the capture slot (reset, and any captured
    /// record forwarded to the record sink), so an unwind can neither
    /// leave a stale `Captured` to be discarded by the next call nor
    /// silently drop an already-acknowledged record.
    pub fn ingest_mined(&mut self, record: &OtlpLogRecord) -> (u64, Option<MinedRecord>) {
        self.mined_capture = MinedCapture::Armed;
        // An RAII guard cannot settle the slot (it would need `&mut self`
        // concurrently with the `ingest` borrow), so catch-unwind
        // provides the same drop-time guarantee explicitly.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.ingest(record))) {
            Ok(template_id) => (template_id, self.take_mined_capture()),
            Err(panic) => {
                self.salvage_mined_capture();
                std::panic::resume_unwind(panic);
            }
        }
    }

    /// Take + reset the capture slot.
    fn take_mined_capture(&mut self) -> Option<MinedRecord> {
        match std::mem::replace(&mut self.mined_capture, MinedCapture::Off) {
            MinedCapture::Captured(rec) => Some(*rec),
            MinedCapture::Off | MinedCapture::Armed => None,
        }
    }

    /// The unwind half of [`Self::ingest_mined`]: reset the slot and
    /// forward a record captured before the panic to the record sink.
    /// The caller's batch is already WAL-durable (acked or about to
    /// be), so dropping a captured record with the unwind would lose it
    /// until a restart's replay — forward it now, and count the salvage
    /// so the (expected-never) path is countable in-process via
    /// [`Self::mined_capture_salvages_total`] — an assertion aid for
    /// tests and debugging, not an exported telemetry instrument.
    fn salvage_mined_capture(&mut self) {
        if let Some(rec) = self.take_mined_capture() {
            self.mined_capture_salvages.fetch_add(1, Ordering::Relaxed);
            self.record_sink.emit(rec);
        }
    }

    /// Records forwarded to the sink by the panic-unwind salvage in
    /// [`Self::ingest_mined`]. Expected 0 in a healthy process.
    #[must_use]
    pub fn mined_capture_salvages_total(&self) -> u64 {
        self.mined_capture_salvages.load(Ordering::Relaxed)
    }

    /// `Body::String` path — RFC §6.2 steps 1–5 with widening.
    //
    // Like `plan_attach`, this function maps 1:1 onto its RFC
    // section's algorithm steps (tokenize → mask → §6.2 step-1
    // empty / over-cap guards → §6.2 step 4 candidate selection
    // → §6.3 three-zone classification → §6.2 step 5 attach).
    // Each branch sets up its own record envelope, so factoring
    // out the branches would mostly shuffle local-variable
    // arguments without simplifying the algorithm; the
    // too_many_lines lint is silenced here rather than
    // fragmenting the per-step structure.
    #[allow(clippy::too_many_lines)]
    fn ingest_string(&mut self, line: StringLine<'_>) -> u64 {
        let StringLine {
            record,
            service,
            raw,
        } = line;
        // RFC §6.2 step 1 (H7.2): a tokenizer failure (today:
        // embedded NUL byte) routes the line to the parse-failure
        // path with `lossy_flag = true` and the original body
        // retained verbatim. Reconstruction is not possible — the
        // line bytes are non-text — so the reader will surface the
        // body column instead. This is orthogonal to the §6.3
        // body-retention paths: `record_tokenizer_failure` bumps
        // only `parse_failures_total`, not `body_retentions_total`
        // (the gauge contract excludes §6.6 retentions).
        let tokenized = match tokenize(raw) {
            Ok(t) => t,
            Err(_err) => {
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.body = Some(raw.to_string());
                rec.lossy_flag = true;
                self.emit_record(rec, service);
                self.record_tokenizer_failure(record, service);
                return NO_TEMPLATE;
            }
        };
        let masked = mask(&tokenized.tokens);
        // Resolve the tenant's effective tunables once for this
        // ingest (RFC 0004 §3.4): per-tenant override if seeded
        // before allocation and the tenant is allocated, else the
        // cluster default.
        let effective_config = self.effective_config(&record.tenant_id);
        // Pre-compute the owned forms once. Every emit path
        // reads these. The §6.5 byte-limit check is applied here
        // (one shared `params` vector across the fresh-leaf and
        // empty/over-cap paths); the attach paths rebuild via
        // `build_record_params` so they apply the same check on
        // the aligned per-Wildcard-slot params.
        let parts = LineParts {
            separators: separators_to_owned(&tokenized.separators),
            params: params_from_mask(&masked.typed_params, effective_config.param_byte_limit),
        };
        let masked_strs: Vec<&str> = masked.tokens.into_iter().collect();
        let masked_line = MaskedLine {
            strs: &masked_strs,
            wildcard_positions: &masked.wildcard_positions,
            typed_params: &masked.typed_params,
        };

        if masked_strs.is_empty() {
            // Empty / whitespace-only input is a §6.2 step 1
            // parse failure. Tokenize still produced one
            // separator entry covering the entire input
            // (`tokens.len() + 1 == 1`).
            let mut rec = Self::record_envelope(record, BodyKind::String);
            rec.separators = parts.separators;
            rec.body = Some(raw.to_string());
            rec.lossy_flag = true;
            self.emit_record(rec, service);
            self.record_parse_failure(record, service, "empty_line");
            return NO_TEMPLATE;
        }

        // RFC 0023 §3.1 bound 3: lines tokenizing past
        // `max_line_tokens` take the parse-failure path with the
        // body retained, bounding stored-template token width. The
        // config type (`u16`) also keeps every accepted line inside
        // the RFC §6.4 audit position width (`positions_widened:
        // Vec<u16>`), whose violation would otherwise be the
        // silent-merge bug `[CLAUDE.md §3.1]` exists to prevent.
        if masked_strs.len() > usize::from(effective_config.max_line_tokens) {
            return self.emit_string_parse_failure(line, parts, "line_too_long");
        }

        // RFC 0050 §3.2 — the upstream-template dial. `ignore`
        // (default) touches nothing; a byte limit of 0 disables all
        // handling. Under `adopt`, a usable `log.record.template`
        // short-circuits the Drain walk entirely; every rejection
        // is counted inside `try_adopt` and falls through to
        // ordinary mining. Under `observe`, the string is validated
        // here (cap before grammar, §3.2) and threaded to the
        // attach/create sites, which associate it with the mined
        // leaf; the clustering itself is untouched (RFC0050.9).
        let upstream_raw = match effective_config.upstream_templates {
            UpstreamTemplates::Ignore => None,
            UpstreamTemplates::Observe | UpstreamTemplates::Adopt
                if effective_config.upstream_template_byte_limit == 0 =>
            {
                None
            }
            UpstreamTemplates::Observe | UpstreamTemplates::Adopt => upstream_template_of(record),
        };
        if effective_config.upstream_templates == UpstreamTemplates::Adopt
            && let Some(s) = upstream_raw
            && let Some(id) = self.try_adopt(line, s, masked_line, &effective_config)
        {
            return id;
        }
        let observed: Option<&str> = if effective_config.upstream_templates
            == UpstreamTemplates::Observe
        {
            upstream_raw.and_then(|s| {
                let cause = if s.len() > effective_config.upstream_template_byte_limit as usize {
                    "byte_limit"
                } else if upstream::parse_template(s).is_err() {
                    "grammar"
                } else {
                    return Some(s);
                };
                self.metrics.record_upstream_template_processed(
                    &record.tenant_id,
                    service,
                    Some(cause),
                );
                None
            })
        } else {
            None
        };

        // Phase 1 — read-only candidate selection. RFC §6.2 step
        // 4: among leaves in the same `(severity, scope, length,
        // prefix)` bucket, pick `argmax sim_seq`. The walk is
        // immutable so we can early-return (or fall through to
        // fresh-leaf creation) without committing a
        // `template_id` allocation.
        let best = self.find_best_candidate(record, masked_line);

        let threshold = effective_config.similarity_threshold;
        let floor = effective_config.similarity_floor;

        let template_id = match best {
            // No candidate at all → fresh leaf. Treated as clean
            // by definition: there was no weaker match to drop
            // into the lossy zone against, and no template to
            // declare a parse failure against.
            None => self.mint_fresh_leaf(
                line,
                FreshLeaf {
                    masked: masked_line,
                    parts,
                    observed,
                    zone: FreshLeafZone::Clean,
                },
                effective_config.max_templates,
            ),
            Some(c) => {
                match ConfidenceZone::classify(c.similarity, threshold, floor) {
                    // Clean: attach to candidate, optionally
                    // widening. RFC §6.2 step 5. No body
                    // retention. The helper emits its own
                    // record (one of: clean-reuse, widening, or
                    // degenerate-rejection); the per-tenant
                    // byte_limit is threaded through so the
                    // helper's rebuilt aligned params also get
                    // §6.5 capping.
                    ConfidenceZone::Clean => self.attach_and_maybe_widen(
                        line,
                        masked_line,
                        Attach::new(c, &effective_config, observed),
                        parts,
                    ),
                    // Lossy: new leaf rather than force-merge
                    // into a too-weak candidate (RFC §6.2 step
                    // 5b). Body retained; no *widening* event, but
                    // `create_new_leaf` audits the leaf's creation
                    // (RFC 0017 §3.1). The retention counter bumps
                    // here; `record_parse_failure` covers the
                    // parse-failure-zone path separately.
                    // The §6.3 lossy zone mints rather than
                    // force-merges into the too-weak candidate, so at
                    // the ceiling it must fail parse, not merge.
                    ConfidenceZone::Lossy => self.mint_fresh_leaf(
                        line,
                        FreshLeaf {
                            masked: masked_line,
                            parts,
                            observed,
                            zone: FreshLeafZone::Lossy {
                                confidence: c.similarity / threshold,
                            },
                        },
                        effective_config.max_templates,
                    ),
                    // Parse failure: no template allocated.
                    // Both counters bump via the shared helper.
                    ConfidenceZone::ParseFailure => {
                        self.emit_string_parse_failure(line, parts, "below_floor")
                    }
                }
            }
        };
        // RFC 0050 §3.2 `observe` success half of the `.processed`
        // shape: the valid string found a mined home (the
        // attach/create sites associated it). A record that ended
        // `NO_TEMPLATE` had no entry to associate with and is
        // already visible on the parse-failure counter.
        if observed.is_some() && template_id != NO_TEMPLATE {
            self.metrics
                .record_upstream_template_processed(&record.tenant_id, service, None);
        }
        template_id
    }

    /// Emit the record for a line that minted a fresh leaf — the one
    /// place the §6.3 fresh-leaf **body-retention rule** is expressed
    /// (epic #745 wave 1; this replaced two divergent copies in
    /// [`Self::ingest_string`]):
    ///
    /// - a no-candidate mint is clean by definition — confidence 1.0,
    ///   no body (only §6.5 overflow may retain it);
    /// - a §6.3 lossy-zone mint retains the body, bumps both
    ///   retention counters, and carries `similarity / threshold` as
    ///   its confidence; `lossy_flag` stays false (§6.6: "body
    ///   retained, reconstruction expected to match").
    fn emit_fresh_leaf_record(
        &mut self,
        line: StringLine<'_>,
        parts: LineParts,
        new_id: u64,
        zone: FreshLeafZone,
    ) -> u64 {
        let StringLine {
            record,
            service,
            raw,
        } = line;
        let mut rec = Self::record_envelope(record, BodyKind::String);
        rec.template_id = new_id;
        rec.template_version = 1;
        rec.separators = parts.separators;
        rec.params = parts.params;
        match zone {
            FreshLeafZone::Clean => rec.confidence = 1.0,
            FreshLeafZone::Lossy { confidence } => {
                self.body_retentions_total.fetch_add(1, Ordering::Relaxed);
                self.metrics.record_body_retention(&record.tenant_id);
                rec.confidence = confidence;
                rec.body = Some(raw.to_string());
            }
        }
        // §6.5: overflowed params force retention on either zone.
        self.apply_overflow_retention(line, &mut rec);
        self.emit_record(rec, service);
        new_id
    }

    /// Emit the §6.3 parse-failure record for a string line — the
    /// shared exit for the below-floor zone, the §6.4
    /// degenerate-widening rejection, the RFC 0023 §3.1 long-line
    /// guard, and the RFC 0023 template-ceiling diverts: no
    /// template, body retained verbatim, lossy-flagged, counted.
    fn emit_string_parse_failure(
        &mut self,
        line: StringLine<'_>,
        parts: LineParts,
        reason: &'static str,
    ) -> u64 {
        let StringLine {
            record,
            service,
            raw,
        } = line;
        let mut rec = Self::record_envelope(record, BodyKind::String);
        rec.separators = parts.separators;
        rec.params = parts.params;
        rec.body = Some(raw.to_string());
        rec.lossy_flag = true;
        // §6.5: bump `params_overflow_total` for any overflow params
        // (body is already retained for the parse-failure reason).
        self.apply_overflow_retention(line, &mut rec);
        self.emit_record(rec, service);
        self.record_parse_failure(record, service, reason);
        NO_TEMPLATE
    }

    /// RFC 0050 §3.2 `adopt` — attempt to take the record's
    /// `log.record.template` claim as its template.
    ///
    /// Returns `Some(template_id)` when the record was adopted and
    /// emitted; `None` when the claim was rejected (`byte_limit`,
    /// `grammar`, `alignment`, `template_ceiling` — each counted on
    /// `ourios.miner.upstream_template.processed`) and the caller
    /// must fall through to ordinary mining.
    ///
    /// Identity is the **canonical shape** (`format_template` over
    /// the parsed tokens, mask names normalised away) plus the §6.1
    /// `(severity, scope)` key. First sight of a canonical runs the
    /// RFC0050.6 convergence lookup — an existing mined leaf with
    /// exactly this shape takes the adoption (provenance grows to
    /// include `upstream_derived`, one audit event) — else an owned
    /// entry is interned with no tree leaf (RFC0050.2), under the
    /// RFC 0023 ceiling. Either resolution is cached, so the scan
    /// is once per canonical, never per record.
    fn try_adopt(
        &mut self,
        line: StringLine<'_>,
        upstream: &str,
        masked: MaskedLine<'_>,
        config: &MinerConfig,
    ) -> Option<u64> {
        let StringLine {
            record,
            service,
            raw,
        } = line;
        // §3.2: the byte cap runs before any work proportional to
        // the attribute's length.
        if upstream.len() > config.upstream_template_byte_limit as usize {
            return self.reject_upstream(record, service, "byte_limit");
        }
        let Ok(parsed) = upstream::parse_template(upstream) else {
            return self.reject_upstream(record, service, "grammar");
        };
        // §3.4: reconstruction decides usability — the alignment
        // recovers params and separators or refuses.
        let Ok(alignment) = upstream::align(&parsed, raw) else {
            return self.reject_upstream(record, service, "alignment");
        };
        let owned_tokens = parsed.to_owned_tokens();
        let canonical = format_template(&owned_tokens);
        let key = (
            canonical.clone(),
            record.severity_number,
            record.scope_name.clone(),
        );
        // Only a genuinely new identity takes a fresh id, and only from a
        // reserved block: a known canonical, or one converging onto a
        // mined leaf, resolves however exhausted the range is.
        let query = AdoptQuery {
            upstream,
            owned_tokens: &owned_tokens,
            key,
            config,
        };

        let (template_id, template_version) = match self.resolve_adoption(record, query) {
            AdoptResolution::Ceiling => {
                // RFC0050.5: at the ceiling, adoption stops
                // interning and the documented fallback is mining
                // (which will divert to parse-failure at the same
                // ceiling).
                return self.reject_upstream(record, service, "template_ceiling");
            }
            AdoptResolution::NoId => return None,
            AdoptResolution::Existing(id, version) => (id, version),
            AdoptResolution::FirstOnLeaf(id, version) => {
                // First adoption of this canonical — a provenance
                // transition, audited once (§3.3 / CLAUDE.md §3.1:
                // a clustering decision made elsewhere is audited
                // like a merge).
                self.emit_adopted_audit(line, id, version, &canonical);
                (id, version)
            }
            AdoptResolution::Interned(id) => {
                self.emit_adopted_audit(line, id, 1, &canonical);
                (id, 1)
            }
        };

        let (params, separators) =
            adopted_row_parts(&parsed, &alignment, masked, config.param_byte_limit);
        let mut rec = Self::record_envelope(record, BodyKind::String);
        rec.template_id = template_id;
        rec.template_version = template_version;
        rec.separators = separators;
        rec.params = params;
        rec.confidence = 1.0;
        self.apply_overflow_retention(line, &mut rec);
        self.emit_record(rec, service);
        self.metrics
            .record_upstream_template_processed(&record.tenant_id, service, None);
        Some(template_id)
    }

    /// Phase 1 of [`Self::attach_and_maybe_widen`]: mutate the
    /// candidate leaf via [`plan_attach`] and apply the RFC 0050
    /// `observe` association, holding the leaf borrow only over
    /// the mutation. No association on a rejection — the record
    /// exits via the parse-failure path, taking no template.
    fn plan_attach_on_candidate(
        &mut self,
        record: &OtlpLogRecord,
        masked: MaskedLine<'_>,
        attach: Attach<'_>,
    ) -> AttachPlan {
        let Attach {
            candidate,
            byte_limit,
            observed,
        } = attach;
        let state = self
            .tenants
            .get_mut(&record.tenant_id)
            .expect("tenant present: find_best_candidate returned Some(...)");
        let (depth, fanout) = (
            state.config.prefix_depth as usize,
            usize::from(state.config.max_node_children),
        );
        let parent = state.tree.descend_mut(masked.strs, depth, fanout);
        let leaf = &mut parent.leaves[candidate.leaf_idx];
        let plan = plan_attach(
            leaf,
            masked.strs,
            masked.wildcard_positions,
            masked.typed_params,
            byte_limit,
        );
        if !matches!(plan, AttachPlan::Rejected { .. }) {
            let bound = usize::from(state.config.upstream_association_limit);
            leaf.associate_upstream(observed, bound);
        }
        plan
    }

    /// Keep the RFC0050.6 convergence index in step with a
    /// template-changing attach: the leaf's canonical moved from
    /// the first `Widened` event's old form to the last one's new
    /// form. Type-expansions carry an unchanged template and need
    /// no move.
    fn move_mined_canonical(&mut self, record: &OtlpLogRecord, events: &[TemplateChange]) {
        let widened_move = events
            .iter()
            .fold(None, |acc: Option<(String, String)>, c| {
                if let TemplateChange::Widened {
                    old_template,
                    new_template,
                    ..
                } = c
                {
                    let old = acc.map_or_else(|| old_template.clone(), |(o, _)| o);
                    Some((old, new_template.clone()))
                } else {
                    acc
                }
            });
        if let Some((old_canonical, new_canonical)) = widened_move
            && let Some(state) = self.tenants.get_mut(&record.tenant_id)
        {
            let key = |canonical| (canonical, record.severity_number, record.scope_name.clone());
            state
                .mined_canonicals
                .replace(&key(old_canonical), key(new_canonical));
        }
    }

    /// Count one rejected upstream-template claim on the
    /// `.processed` counter and yield the "fall through to mining"
    /// signal (RFC 0050 §3.2).
    fn reject_upstream(
        &self,
        record: &OtlpLogRecord,
        service: Option<&str>,
        cause: &'static str,
    ) -> Option<u64> {
        self.metrics
            .record_upstream_template_processed(&record.tenant_id, service, Some(cause));
        None
    }

    /// Resolve one adopted canonical to its identity, under a
    /// single tenant borrow — the caller emits any audit event
    /// afterwards. See [`AdoptResolution`] for the four outcomes.
    fn resolve_adoption(
        &mut self,
        record: &OtlpLogRecord,
        query: AdoptQuery<'_>,
    ) -> AdoptResolution {
        let AdoptQuery {
            upstream,
            owned_tokens,
            key,
            config,
        } = query;
        let effective_config = *config;
        let assoc_limit = usize::from(config.upstream_association_limit);
        let state = self
            .tenants
            .entry(record.tenant_id.clone())
            .or_insert_with(|| TenantState::new(effective_config));
        match state.adopted_templates.get_mut(&key) {
            Some(AdoptedEntry::TreeBacked {
                template_id,
                template_version,
            }) => AdoptResolution::Existing(*template_id, *template_version),
            Some(AdoptedEntry::Owned(owned)) => {
                // A second raw spelling of the same canonical (mask
                // names differ) is worth keeping as an association;
                // repeats dedup.
                let _ = owned.associations.observe(upstream, assoc_limit);
                AdoptResolution::Existing(owned.template_id, 1)
            }
            None => {
                // The index guard makes a miss O(1): the tree walk
                // runs only when a mined leaf with this exact shape
                // is known to exist — once per converging canonical,
                // never for the unique-template flood (which
                // otherwise degrades toward O(n²) at the ceiling).
                let mined_hit = state
                    .mined_canonicals
                    .contains(&key)
                    .then(|| {
                        state.tree.find_exact_mut(
                            owned_tokens,
                            record.severity_number,
                            record.scope_name.as_deref(),
                        )
                    })
                    .flatten();
                if let Some(leaf) = mined_hit {
                    // Mined first, adopted second (RFC0050.6). The
                    // audit event fires only on a genuine provenance
                    // transition: a leaf adopted earlier, widened to
                    // a new canonical and adopted again already
                    // carries `upstream_derived` — that is a cache
                    // fill, not a transition.
                    let pair = (leaf.template_id, leaf.template_version);
                    let first = !leaf.provenance.contains(Provenance::UpstreamDerived);
                    leaf.provenance = leaf.provenance.insert(Provenance::UpstreamDerived);
                    state.adopted_templates.insert(
                        key,
                        AdoptedEntry::TreeBacked {
                            template_id: pair.0,
                            template_version: pair.1,
                        },
                    );
                    if first {
                        AdoptResolution::FirstOnLeaf(pair.0, pair.1)
                    } else {
                        AdoptResolution::Existing(pair.0, pair.1)
                    }
                } else if state.leaf_count + state.owned_adopted_count
                    >= config.max_templates as usize
                {
                    AdoptResolution::Ceiling
                } else {
                    // Interning is the only branch that takes an id, and
                    // takes it from a reserved block or not at all.
                    let Ok(candidate_id) = self.ids.take() else {
                        return AdoptResolution::NoId;
                    };
                    let mut associations = UpstreamAssociations::default();
                    let _ = associations.observe(upstream, assoc_limit);
                    state.adopted_templates.insert(
                        key,
                        AdoptedEntry::Owned(OwnedAdopted {
                            template_id: candidate_id,
                            provenance: ProvenanceSet::singleton(Provenance::UpstreamDerived),
                            associations,
                        }),
                    );
                    state.owned_adopted_count += 1;
                    state.template_count += 1;
                    AdoptResolution::Interned(candidate_id)
                }
            }
        }
    }

    /// Mint a fresh leaf for `line`, or divert it to the §6.3
    /// parse-failure path: at the RFC 0023 §3.1 per-tenant ceiling, or
    /// without a reservable id for a genuinely new identity. A line
    /// whose shape an adopted entry already owns converges onto that
    /// entry's id (RFC0050.6) and needs no fresh one.
    fn mint_fresh_leaf(
        &mut self,
        line: StringLine<'_>,
        fresh: FreshLeaf<'_>,
        max_templates: u32,
    ) -> u64 {
        let FreshLeaf {
            masked,
            parts,
            observed,
            zone,
        } = fresh;
        if self.at_ceiling(&line.record.tenant_id, max_templates) {
            return self.emit_string_parse_failure(line, parts, "template_ceiling");
        }
        match self.create_new_leaf(line, masked, observed) {
            Some(new_id) => self.emit_fresh_leaf_record(line, parts, new_id, zone),
            None => self.emit_string_parse_failure(line, parts, ID_RESERVATION_FAILED),
        }
    }

    /// Emit the RFC 0050 §3.3 `template_adopted` audit event.
    fn emit_adopted_audit(
        &mut self,
        line: StringLine<'_>,
        template_id: u64,
        template_version: u32,
        canonical: &str,
    ) {
        self.emit_template_change(
            line,
            template_id,
            TemplateChange::Adopted {
                template_version,
                new_template: canonical.to_string(),
            },
        );
    }

    /// Emit one template audit event for `line` (RFC 0001 §6.4; RFC
    /// 0017 §3.1 keeps the audit stream the template history of record).
    fn emit_template_change(
        &mut self,
        line: StringLine<'_>,
        template_id: u64,
        change: TemplateChange,
    ) {
        let StringLine { record, raw, .. } = line;
        self.audit_sink.emit(AuditEvent {
            tenant_id: record.tenant_id.clone(),
            timestamp: self.clock.now(),
            payload: AuditPayload::Template {
                template_id,
                triggering_line_hash: hash_triggering_line(raw.as_bytes()),
                triggering_line_sample: Some(sample_first_256_bytes(raw)),
                change,
            },
        });
    }

    /// RFC §6.2 step 4 — find the best-matching leaf in the
    /// `(severity, scope, length, prefix)` bucket, or `None` if
    /// the tenant is unseen, the prefix path doesn't exist, or
    /// the leaf list is empty (after filtering).
    fn find_best_candidate(
        &self,
        record: &OtlpLogRecord,
        masked: MaskedLine<'_>,
    ) -> Option<Candidate> {
        let MaskedLine {
            strs: masked_strs,
            wildcard_positions: line_wildcard_positions,
            ..
        } = masked;
        let state = self.tenants.get(&record.tenant_id)?;
        // Per-tenant `prefix_depth` (RFC 0004 §3.4) — read from
        // the captured `state.config` rather than the cluster
        // default.
        let parent = state.tree.descend(
            masked_strs,
            state.config.prefix_depth as usize,
            usize::from(state.config.max_node_children),
        )?;

        // Severity and scope are not part of the tree's keying: each
        // `(length, prefix)` bucket holds one leaf per `(severity, scope)`
        // pair and filters on the leaf-list side. A similarity tie goes to
        // the lowest `template_id` — a property of the leaves, not of
        // list order, which a snapshot restore does not reproduce.
        parent
            .leaves
            .iter()
            .enumerate()
            .filter(|(_, leaf)| {
                leaf.severity_number == record.severity_number
                    && leaf.scope_name.as_deref() == record.scope_name.as_deref()
            })
            .map(|(leaf_idx, leaf)| {
                debug_assert_eq!(leaf.template.len(), masked_strs.len());
                let similarity =
                    sim_seq_owned(masked_strs, &leaf.template, line_wildcard_positions);
                (
                    Candidate {
                        leaf_idx,
                        similarity,
                    },
                    leaf.template_id,
                )
            })
            .max_by(|(a, a_id), (b, b_id)| {
                a.similarity.total_cmp(&b.similarity).then(b_id.cmp(a_id))
            })
            .map(|(candidate, _)| candidate)
    }

    /// RFC §6.2 step 4 (fresh-leaf branch). Allocates a new
    /// `template_id`, materialises the prefix path, pushes a leaf
    /// whose template carries `OwnedToken::Wildcard` at every
    /// mask-emitted position and `OwnedToken::Fixed` elsewhere.
    /// `slot_types` is seeded from `typed_params` in ordinal order
    /// — `slot_types[k]` is the singleton `{typed_params[k]
    /// .type_tag}` for the k-th masked position, recording the
    /// type observed at that slot's first sight.
    ///
    /// RFC 0017 §3.1: this path emits a `TemplateChange::Created` audit
    /// event so a read-time registry can recover the leaf's v1 tokens. It
    /// is **not** a merge — `template_count` reflects the allocation and
    /// `merges_total` stays reserved for widening / type-expansion events
    /// on existing leaves. (Supersedes the original RFC0001.1
    /// "creation emits nothing" contract.)
    fn create_new_leaf(
        &mut self,
        line: StringLine<'_>,
        masked: MaskedLine<'_>,
        observed: Option<&str>,
    ) -> Option<u64> {
        let StringLine { record, .. } = line;
        let MaskedLine {
            strs: masked_strs,
            wildcard_positions: line_wildcard_positions,
            typed_params: line_typed_params,
        } = masked;
        debug_assert_eq!(
            line_wildcard_positions.len(),
            line_typed_params.len(),
            "mask invariant: typed_params parallel to wildcard_positions",
        );
        // Build the leaf template: Wildcard at every mask-emitted
        // position, Fixed at every other. Its canonical form keys the
        // audit event and the RFC 0050 convergence.
        let new_template = leaf_template(masked);
        let created_template = format_template(&new_template);
        let adopted_key = (
            created_template.clone(),
            record.severity_number,
            record.scope_name.clone(),
        );
        // RFC 0050 §3.3 / RFC0050.6, "adopted first, mined second": if
        // this exact canonical shape was already interned by adoption,
        // the mined leaf takes over that identity — same `template_id`,
        // provenance grown to `{mined, upstream_derived}`, associations
        // migrated. Otherwise the leaf takes a fresh id from a reserved
        // block, or is not created at all.
        let converged = self
            .tenants
            .get_mut(&record.tenant_id)
            .and_then(|state| state.take_converged_adoption(&adopted_key));
        let (new_id, provenance, upstream_associations, fresh) = match converged {
            Some(owned) => (
                owned.template_id,
                owned.provenance.insert(Provenance::Mined),
                owned.associations,
                false,
            ),
            None => (
                self.ids.take().ok()?,
                ProvenanceSet::singleton(Provenance::Mined),
                UpstreamAssociations::default(),
                true,
            ),
        };

        // Resolve effective config BEFORE the entry/get-or-insert
        // borrow on `self.tenants` — the `or_insert_with` closure
        // can't reach back to `self.tenant_overrides` while the
        // map is borrowed mutably.
        let effective_config = self.effective_config(&record.tenant_id);
        // Scope the `self.tenants` borrow so it is released before the
        // audit emit below (which borrows `self.audit_sink`); the leaf's
        // canonical template string is computed inside and handed out.
        {
            let state = self
                .tenants
                .entry(record.tenant_id.clone())
                .or_insert_with(|| TenantState::new(effective_config));
            let slot_types: Vec<SlotTypes> = line_typed_params
                .iter()
                .map(|tp| SlotTypes::singleton(tp.type_tag))
                .collect();
            let assoc_limit = usize::from(state.config.upstream_association_limit);
            let parent = state.tree.descend_mut(
                masked_strs,
                state.config.prefix_depth as usize,
                usize::from(state.config.max_node_children),
            );
            parent.leaves.push(Leaf {
                template: new_template,
                template_id: new_id,
                template_version: 1,
                severity_number: record.severity_number,
                scope_name: record.scope_name.clone(),
                slot_types,
                provenance,
                upstream_associations,
            });
            // RFC 0050 `observe`: the record that minted this leaf
            // may have carried a valid upstream string.
            if let Some(leaf) = parent.leaves.last_mut() {
                leaf.associate_upstream(observed, assoc_limit);
            }
            // Maintain the TenantState::template_count cache invariant —
            // every fresh allocation under `state` is mirrored here so
            // `MinerCluster::template_count` can stay O(1). `leaf_count`
            // mirrors tree leaves only; together with
            // `owned_adopted_count` it is the RFC 0023 ceiling basis,
            // which is why a convergence does not bump `template_count`:
            // the identity already counted when adoption interned it.
            if fresh {
                state.template_count += 1;
            }
            state.leaf_count += 1;
            // Index the fresh mined canonical for the RFC0050.6
            // convergence guard.
            state.mined_canonicals.insert(adopted_key);
        }
        // RFC 0017 §3.1 — audit the leaf's initial (version 1) creation so a
        // read-time template registry can recover the v1 tokens once the
        // originating rows age out. Same WAL-before-ack path as the widening
        // events; not a merge, so it does not bump `merges_total`.
        self.emit_template_change(
            line,
            new_id,
            TemplateChange::Created {
                new_template: created_template,
            },
        );
        Some(new_id)
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
    fn attach_and_maybe_widen(
        &mut self,
        line: StringLine<'_>,
        masked: MaskedLine<'_>,
        attach: Attach<'_>,
        parts: LineParts,
    ) -> u64 {
        let StringLine {
            record, service, ..
        } = line;
        // Ownership rationale: each exit path emits **one** data
        // record and never reuses `parts` after that emit. Taking it
        // by value lets each branch move its vectors straight into
        // the record without a `.to_vec()` clone.

        // Phase 1 — mutate the leaf and accumulate the audit-event
        // payloads (the helper holds the leaf borrow only over the
        // mutation); emitting through `self.audit_sink` is phase 2.
        let plan = self.plan_attach_on_candidate(record, masked, attach);

        match plan {
            AttachPlan::CleanReuse {
                template_id,
                template_version,
                params: aligned_params,
            } => {
                // §6.6: emit the params vector aligned with the
                // leaf's wildcard slots, not the line-ordered
                // `params_from_mask` we built earlier. The leaf
                // may carry wildcards from past widenings that
                // the current line has a literal at; those slots
                // need a Str-fallback entry that the mask emit
                // doesn't produce.
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.template_id = template_id;
                rec.template_version = template_version;
                rec.separators = parts.separators;
                rec.params = aligned_params;
                rec.confidence = 1.0;
                // §6.5: force body retention on this clean-reuse
                // record if any of its aligned params overflowed.
                self.apply_overflow_retention(line, &mut rec);
                self.emit_record(rec, service);
                template_id
            }
            AttachPlan::Rejected {
                template_id,
                version,
                current_template,
                would_be_template,
                would_be_positions,
            } => {
                // §6.4: the audit records *why* the attach refused to widen.
                self.emit_template_change(
                    line,
                    template_id,
                    TemplateChange::RejectedDegenerate {
                        version,
                        current_template,
                        would_be_template,
                        would_be_positions,
                    },
                );
                // §6.4 treats degenerate widening as a parse
                // failure that retains body (the line-ordered
                // params fallback is fine — reconstruct ignores
                // `params` on the lossy path).
                self.emit_string_parse_failure(line, parts, "degenerate_widening")
            }
            AttachPlan::Mutated {
                template_id,
                events,
                final_version,
                params: aligned_params,
            } => {
                self.move_mined_canonical(record, &events);
                for change in events {
                    let counts_as_merge = change.counts_as_merge();
                    let event_type = change.event_type();
                    self.emit_template_change(line, template_id, change);
                    if counts_as_merge {
                        self.merges_total.fetch_add(1, Ordering::Relaxed);
                        self.metrics.record_merge(&record.tenant_id, event_type);
                    }
                }
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.template_id = template_id;
                rec.template_version = final_version;
                rec.separators = parts.separators;
                rec.params = aligned_params;
                rec.confidence = 1.0;
                // §6.5: force body retention on this widened /
                // type-expanded record if any of its aligned
                // params overflowed.
                self.apply_overflow_retention(line, &mut rec);
                self.emit_record(rec, service);
                template_id
            }
        }
    }

    /// Emit a structured record's data row under `template_id`.
    fn emit_structured(&mut self, line: StructuredLine<'_>, template_id: u64) {
        let StructuredLine {
            record,
            service,
            body: any_value,
        } = line;
        // Emit a data record. Structured records carry no
        // separators or params — reconstruction goes via the
        // `body` field (per §6.2 step 0), and `lossy_flag = false`
        // per RFC §6.1 ("Always false when body_kind =
        // Structured").
        //
        // `body` carries the RFC 0005 §3.3 Ourios-canonical-JSON
        // encoding of the `AnyValue` — the bytes the writer
        // stores in the §3.2 `body` column for structured rows.
        // Two interlocking invariants prevent any fallback path
        // that would weaken this:
        //
        // - RFC 0001 §6.1 / body-representation table:
        //   `lossy_flag` is **always `false` when
        //   `body_kind = Structured`** ("the verbatim `body`
        //   column is the source of truth"). Setting it on the
        //   encoder path would mint a row shape the RFC says
        //   cannot exist.
        // - RFC 0005 §3.3: the `body` column for structured
        //   rows MUST hold canonical JSON. A
        //   `format!("{any_value:?}")` fallback would silently
        //   write spec-violating bytes into a §3.3-governed
        //   column — the masquerading-as-JSON failure mode the
        //   writer's prior `StructuredBodyNotYetCanonical`
        //   rejection prevented.
        //
        // `canonical::encode_any_value` is infallible on every
        // `AnyValue` value the type system admits.
        // `opentelemetry-proto`'s `with-serde` ships custom
        // serializers (see `proto.rs::serializer_f64`) that
        // emit `"NaN"` / `"Infinity"` / `"-Infinity"` strings
        // per the proto3 JSON spec rather than letting
        // `serde_json`'s default `f64` path emit `null` — which
        // also covers the only failure mode review raised on
        // an earlier revision. The recursive variants
        // (`ArrayValue`, `KvlistValue`) bottom out in the same
        // primitive serializers, so encode failure is
        // unreachable here. `.expect` documents the contract
        // rather than swallowing a `Result` we never inspect.
        let bytes = ourios_core::otlp::canonical::encode_any_value(any_value)
            .expect("RFC 0005 §3.3 encoder is infallible for any spec-compliant AnyValue");
        // RFC 0037 §3.2 (hazard #2 guard): observe the canonical-JSON body
        // size before `bytes` is moved into the record. Structured bodies are
        // never capped, so this histogram is the only guard against oversized
        // payloads — it makes the size visible per service without discarding
        // the operator's payload.
        self.metrics
            .record_structured_body_bytes(&record.tenant_id, service, bytes.len() as u64);
        let mut rec = Self::record_envelope(record, BodyKind::Structured);
        rec.template_id = template_id;
        rec.template_version = 1;
        rec.confidence = 1.0;
        rec.body = Some(String::from_utf8(bytes).expect("serde_json emits valid UTF-8"));
        self.emit_record(rec, service);
    }
}

#[cfg(test)]
mod tests;
