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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, NoOpAuditSink, ParamType, SlotExpansion, SlotTypes,
    TemplateChange, hash_triggering_line, sample_first_256_bytes,
};
use ourios_core::clock::{Clock, SystemClock};
use ourios_core::confidence::ConfidenceZone;
use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::record::{BodyKind, MinedRecord, NoOpRecordSink, Param, RecordSink};
use ourios_core::tenant::TenantId;

use crate::mask::{mask, tag_str_for};
use crate::metrics::{MinerMetrics, service_of};
use crate::sim_seq::sim_seq_owned;
use crate::tokenize::tokenize;
use crate::tree::{Leaf, OwnedToken, Tree};

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
    /// Effective [`MinerConfig`] for this tenant, captured at
    /// lazy allocation time. Resolves to the per-tenant override
    /// (set via [`MinerCluster::with_tenant_config`]) if one
    /// exists; otherwise the cluster default. RFC 0004 §3.4 pins
    /// this as the per-tenant tunable surface; further mutation
    /// after allocation is out of scope (the RFC names dynamic
    /// reconfiguration as an open question; today's contract is
    /// startup-only).
    config: MinerConfig,
}

impl TenantState {
    fn new(config: MinerConfig) -> Self {
        Self {
            tree: Tree::new(),
            structured_templates: HashMap::new(),
            template_count: 0,
            config,
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
            tenant_overrides: HashMap::new(),
            tenants: HashMap::new(),
            // Start at 1 so 0 stays available as the [`NO_TEMPLATE`]
            // sentinel.
            next_template_id: 1,
            audit_sink: sink,
            record_sink: Box::new(NoOpRecordSink::new()),
            merges_total: AtomicU64::new(0),
            parse_failures_total: AtomicU64::new(0),
            body_retentions_total: AtomicU64::new(0),
            params_overflow_total: AtomicU64::new(0),
            clock: Box::new(SystemClock::new()),
            metrics: MinerMetrics::new(),
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

    /// Convenience for tests / tuning experiments: pre-bake a
    /// `MinerConfig` with an overridden `prefix_depth` and rebuild
    /// the cluster.
    ///
    /// Production callers should set `prefix_depth` directly via
    /// `MinerConfig::default().with_prefix_depth(...)` before
    /// calling [`Self::new`] / [`Self::with_audit_sink`]; this
    /// helper exists so the in-crate degenerate-guard test (which
    /// pins `prefix_depth = 0` to make every length-N line share
    /// one leaf list) keeps its terse setup.
    ///
    /// # Panics
    ///
    /// If `depth > PREFIX_DEPTH_CEILING` (the RFC 0001 §6.1
    /// ceiling). Test-only path; the validated config-builder
    /// surface ([`MinerConfig::with_prefix_depth`]) returns
    /// `Result` instead.
    #[must_use]
    pub fn with_prefix_depth(mut self, depth: u8) -> Self {
        self.config = self
            .config
            .with_prefix_depth(depth)
            .expect("test-only setter: depth must be within PREFIX_DEPTH_CEILING");
        self
    }

    /// Register a per-tenant [`MinerConfig`] override per RFC 0004
    /// §3.4. The override is captured by `TenantState` at lazy
    /// allocation (i.e. on the first ingest for `tenant_id`); set
    /// it before the tenant is first observed.
    ///
    /// If `tenant_id` was already observed before this call, the
    /// override has no effect — `TenantState` is allocated once
    /// and its config is captured at that moment. The
    /// startup-only contract is RFC 0004 §3.4's open question
    /// resolved in favour of "captured at allocation"; dynamic
    /// reconfiguration is a future RFC.
    ///
    /// Multiple calls with the same `tenant_id` before allocation
    /// keep the last-set override (`HashMap::insert` semantics).
    #[must_use]
    pub fn with_tenant_config(mut self, tenant_id: TenantId, config: MinerConfig) -> Self {
        self.tenant_overrides.insert(tenant_id, config);
        self
    }

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
    /// (see [`TemplateChange::counts_as_merge`]). Rejection events
    /// are recorded but do not increment this counter.
    /// Read-side placeholder for the §6.8 Prometheus gauge.
    #[must_use]
    pub fn merges_total(&self) -> u64 {
        self.merges_total.load(Ordering::Relaxed)
    }

    /// Cumulative count of lines that produced no template. Two
    /// disjoint sources contribute, but the counter is one gauge:
    ///
    /// - §6.3 / §6.4 body-retention paths: empty /
    ///   whitespace-only `Body::String`, over-cap lines
    ///   (`> u16::MAX` tokens), the §6.4 degenerate-template
    ///   rejection branch, and the §6.3 parse-failure zone
    ///   (`simSeq < similarity_floor`). These also bump
    ///   `body_retentions_total`.
    /// - §6.6 tokenizer-failure paths (today: embedded NUL byte
    ///   per H7.2). These set `lossy_flag = true` on the emitted
    ///   record and do **not** bump `body_retentions_total` (the
    ///   `body_retention_ratio` gauge surfaces §6.3 zone
    ///   retention, not the orthogonal §6.6 reconstruction-
    ///   impossible retention).
    ///
    /// Read-side placeholder for the §6.8
    /// `parse_failures_total` Prometheus gauge.
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

    /// Cumulative count of per-parameter byte-limit overflow
    /// events per RFC §6.5. Increments by the count of
    /// `Overflow`-tagged [`Param`]s on each emitted record (so a
    /// record with two oversized params bumps the counter by 2).
    /// Read-side placeholder for the §6.8
    /// `ourios.miner.params.overflow` *counter* metric; the §3.2
    /// `ourios.miner.params.overflow.utilization` *gauge* is the
    /// derived rolling ratio with the `> 0.01` per-service
    /// alert threshold — both ship together once the exporter
    /// lands and are not this method's responsibility.
    #[must_use]
    pub fn params_overflow_total(&self) -> u64 {
        self.params_overflow_total.load(Ordering::Relaxed)
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
    fn apply_overflow_retention(
        &self,
        record: &OtlpLogRecord,
        service: Option<&str>,
        rec: &mut MinedRecord,
        raw: &str,
    ) {
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
    fn record_parse_failure(&self, record: &OtlpLogRecord, service: Option<&str>) {
        self.parse_failures_total.fetch_add(1, Ordering::Relaxed);
        self.body_retentions_total.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .record_parse_failure(&record.tenant_id, service);
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
            .record_parse_failure(&record.tenant_id, service);
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
/// `Vec<Param>` shape `MinedRecord::params` carries, applying the
/// §6.5 per-parameter byte-limit check.
///
/// Any value whose UTF-8 byte length exceeds `byte_limit` is
/// replaced by an `Overflow` marker (RFC §6.5); the caller is
/// responsible for setting `body = Some(raw)` on the emitted
/// record when [`crate::overflow::any_overflow`] returns true for
/// the resulting params vector.
fn params_from_mask(typed_params: &[crate::mask::TypedParam<'_>], byte_limit: u32) -> Vec<Param> {
    typed_params
        .iter()
        .map(|p| crate::overflow::cap_param_value(p.type_tag, p.value.to_string(), byte_limit))
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

/// On widening, seed [`Leaf::slot_types`] for each newly-introduced
/// `Wildcard` position. The initial type set captures both
/// observations the widening witnessed:
///
/// - the pre-widen `Fixed` token — under PR-B-1's model this is
///   always a *literal* token (mask-emitted positions enter the
///   leaf as `Wildcard` from creation in [`MinerCluster::
///   create_new_leaf`]), so its `ParamType` is unconditionally
///   `Str` per RFC §6.2 step 5b.
/// - the line's token at that position (the value that triggered
///   the widening), classified from mask's output by
///   [`param_type_for_line_position`].
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
    post_widen_template: &[OwnedToken],
    line_wildcard_positions: &[usize],
    line_typed_params: &[crate::mask::TypedParam<'_>],
    positions_widened: &[usize],
) {
    debug_assert!(
        positions_widened.windows(2).all(|w| w[0] < w[1]),
        "positions_widened must be sorted ascending",
    );

    let mut ordinal = 0usize;
    let mut widen_iter = positions_widened.iter().copied().peekable();
    for (p, tok) in post_widen_template.iter().enumerate() {
        if matches!(tok, OwnedToken::Wildcard) {
            if widen_iter.peek().copied() == Some(p) {
                // Line side: authoritative classification from
                // mask. Leaf side: PR-B-1 invariant — the
                // pre-widen Fixed at a widened position is always
                // a literal (mask-emit positions enter as
                // Wildcard), so the initial slot type is
                // {Str, line_type}.
                let line_type =
                    param_type_for_line_position(p, line_wildcard_positions, line_typed_params);
                let initial = SlotTypes::singleton(ParamType::Str).insert(line_type);
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

/// RFC §6.6 alignment: build the `params` vector with one entry
/// per `Wildcard` slot in the leaf template, left-to-right.
///
/// For each wildcard at template position `p`:
///
/// - If the line had a mask emit at the same position
///   (`p ∈ line_wildcard_positions`), use that `TypedParam`
///   verbatim — the original token bytes and its `ParamType`.
/// - Else (the line had a literal at `p` but the leaf carries a
///   `Wildcard` there — either from a past widening of a
///   literal-token mismatch, or because this attach just
///   freshly-widened a literal at `p`), fall back to
///   `{ type_tag: Str, value: masked_strs[p] }`. `masked_strs[p]`
///   for a non-mask position is the original literal token (mask
///   leaves unclassified tokens unchanged), so the STR fallback
///   captures the bytes reconstruction will need.
///
/// This is the contract [`crate::reconstruct::reconstruct`] reads
/// against. Producers
/// for fresh-leaf paths (`None` / `Lossy` zones in
/// [`MinerCluster::ingest_string`]) build params via the simpler
/// [`params_from_mask`] because their template Wildcards align
/// 1:1 with the line's mask positions by construction; everything
/// else routes through this helper.
///
/// **Scope boundary.** STR fallback is invoked only after a leaf
/// has been *found*. The Drain tree's prefix routing keys each
/// level by the concrete masked token, so a leaf with a
/// `Wildcard` slot inside `prefix_depth` is structurally
/// unreachable from a line whose prefix masks to a different
/// concrete token at that position (e.g. a literal `abc` at
/// position 1 cannot find a leaf whose position-1 prefix key is
/// the mask-emitted `<NUM>`). This is a property of the Drain
/// tree (paper §3.2, RFC 0001 §6.1), not a bug in this helper;
/// any change to make wildcards reachable from divergent prefix
/// tokens (multi-bucket lookup, wildcard-aware re-bucketing) is
/// its own RFC-level decision.
fn build_record_params(
    template: &[OwnedToken],
    masked_strs: &[&str],
    line_wildcard_positions: &[usize],
    line_typed_params: &[crate::mask::TypedParam<'_>],
    byte_limit: u32,
) -> Vec<Param> {
    debug_assert_eq!(
        line_wildcard_positions.len(),
        line_typed_params.len(),
        "mask invariant: typed_params parallel to wildcard_positions",
    );
    debug_assert_eq!(
        template.len(),
        masked_strs.len(),
        "sim_seq precondition: template and line are the same length",
    );

    let wildcard_count = template
        .iter()
        .filter(|t| matches!(t, OwnedToken::Wildcard))
        .count();
    let mut out = Vec::with_capacity(wildcard_count);
    let mut k = 0usize;
    for (p, tok) in template.iter().enumerate() {
        if !matches!(tok, OwnedToken::Wildcard) {
            continue;
        }
        let (type_tag, value) =
            if k < line_wildcard_positions.len() && line_wildcard_positions[k] == p {
                let entry = (
                    line_typed_params[k].type_tag,
                    line_typed_params[k].value.to_string(),
                );
                k += 1;
                entry
            } else {
                // STR fallback for an existing-Wildcard / freshly-
                // widened-literal slot — see helper docstring.
                (ParamType::Str, masked_strs[p].to_string())
            };
        // RFC §6.5 byte-limit check at the param boundary: an
        // over-cap value becomes an Overflow marker.
        out.push(crate::overflow::cap_param_value(
            type_tag, value, byte_limit,
        ));
    }
    debug_assert_eq!(
        k,
        line_wildcard_positions.len(),
        "every mask emit position must coincide with a template Wildcard",
    );
    out
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
    /// Reuse `(template_id, template_version)` verbatim. `params`
    /// is aligned with the leaf's wildcard slots per RFC §6.6 —
    /// see [`build_record_params`].
    CleanReuse {
        template_id: u64,
        template_version: u32,
        params: Vec<Param>,
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
    /// template-change payload in emission order: `Widened` before
    /// `TypeExpanded` per RFC §6.2's combined-attach contract.
    /// `params` is aligned with the post-widen template's wildcard
    /// slots.
    Mutated {
        template_id: u64,
        events: Vec<TemplateChange>,
        final_version: u32,
        params: Vec<Param>,
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
    byte_limit: u32,
) -> AttachPlan {
    let positions_widened =
        find_widening_positions(masked_strs, &leaf.template, line_wildcard_positions);

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
                params: build_record_params(
                    &leaf.template,
                    masked_strs,
                    line_wildcard_positions,
                    line_typed_params,
                    byte_limit,
                ),
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
        let params = build_record_params(
            &leaf.template,
            masked_strs,
            line_wildcard_positions,
            line_typed_params,
            byte_limit,
        );
        return AttachPlan::Mutated {
            template_id: leaf.template_id,
            final_version: new_version,
            params,
            events: vec![TemplateChange::TypeExpanded {
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

    // Widening path. PR-B-1 invariant: every Fixed token in a
    // leaf is a literal (mask-emit positions enter as Wildcard),
    // so the pre-widen template doesn't need to be snapshot for
    // slot seeding — the seed is always `{Str, line_type}`.
    let template_id = leaf.template_id;
    let old_version = leaf.template_version;
    let old_template_str = format_template(&leaf.template);
    let positions_u16 = positions_to_u16(&positions_widened);

    apply_widening(&mut leaf.template, &positions_widened);
    update_slot_types_on_widening(
        &mut leaf.slot_types,
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

    let mut events: Vec<TemplateChange> = Vec::with_capacity(2);
    events.push(TemplateChange::Widened {
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
        events.push(TemplateChange::TypeExpanded {
            old_version: version_after_widen,
            new_version: version_after_expand,
            old_template: template_after_widen.clone(),
            new_template: template_after_widen,
            slots_expanded: expansions,
        });
        version_after_expand
    };

    // Build params aligned to the post-widen template's wildcard
    // slots. §6.6 reconstruction reads against this alignment.
    // §6.5 cap applied per-slot inside `build_record_params`.
    let params = build_record_params(
        &leaf.template,
        masked_strs,
        line_wildcard_positions,
        line_typed_params,
        byte_limit,
    );

    AttachPlan::Mutated {
        template_id,
        events,
        final_version,
        params,
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
                // carry. `lossy_flag = true` because there is no
                // template, so reconstruction is not possible.
                let mut rec = Self::record_envelope(record, BodyKind::Absent);
                rec.lossy_flag = true;
                self.emit_record(rec, service);
                NO_TEMPLATE
            }
            Some(Body::String(raw)) => self.ingest_string(record, service, raw),
            Some(Body::Structured(av)) => self.ingest_structured(record, service, av),
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
    fn ingest_string(&mut self, record: &OtlpLogRecord, service: Option<&str>, raw: &str) -> u64 {
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
        let separators = separators_to_owned(&tokenized.separators);
        let params = params_from_mask(&masked.typed_params, effective_config.param_byte_limit);
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
            self.emit_record(rec, service);
            self.record_parse_failure(record, service);
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
            // §6.5: bump `params_overflow_total` if any param
            // exceeded the byte limit. Body retention is already
            // set above for this parse-failure path.
            self.apply_overflow_retention(record, service, &mut rec, raw);
            self.emit_record(rec, service);
            self.record_parse_failure(record, service);
            return NO_TEMPLATE;
        }

        // Phase 1 — read-only candidate selection. RFC §6.2 step
        // 4: among leaves in the same `(severity, scope, length,
        // prefix)` bucket, pick `argmax sim_seq`. The walk is
        // immutable so we can early-return (or fall through to
        // fresh-leaf creation) without committing a
        // `template_id` allocation.
        let best = self.find_best_candidate(record, &masked_strs, &masked.wildcard_positions);

        let threshold = effective_config.similarity_threshold;
        let floor = effective_config.similarity_floor;

        match best {
            // No candidate at all → fresh leaf. Treated as clean
            // by definition: there was no weaker match to drop
            // into the lossy zone against, and no template to
            // declare a parse failure against.
            None => {
                let new_id = self.create_new_leaf(
                    record,
                    raw,
                    &masked_strs,
                    &masked.wildcard_positions,
                    &masked.typed_params,
                );
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.template_id = new_id;
                rec.template_version = 1;
                rec.separators = separators;
                rec.params = params;
                rec.confidence = 1.0;
                // §6.5: force body retention on this fresh-leaf
                // record if any of its params overflowed.
                self.apply_overflow_retention(record, service, &mut rec, raw);
                self.emit_record(rec, service);
                new_id
            }
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
                        record,
                        service,
                        raw,
                        &masked_strs,
                        &masked.wildcard_positions,
                        &masked.typed_params,
                        c,
                        separators,
                        params,
                        effective_config.param_byte_limit,
                    ),
                    // Lossy: new leaf rather than force-merge
                    // into a too-weak candidate (RFC §6.2 step
                    // 5b). Body retained; no *widening* event, but
                    // `create_new_leaf` audits the leaf's creation
                    // (RFC 0017 §3.1). The retention counter bumps
                    // here; `record_parse_failure` covers the
                    // parse-failure-zone path separately.
                    ConfidenceZone::Lossy => {
                        self.body_retentions_total.fetch_add(1, Ordering::Relaxed);
                        self.metrics.record_body_retention(&record.tenant_id);
                        let new_id = self.create_new_leaf(
                            record,
                            raw,
                            &masked_strs,
                            &masked.wildcard_positions,
                            &masked.typed_params,
                        );
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
                        // §6.5: bump `params_overflow_total` for
                        // any overflow params (body is already
                        // retained for the §6.3 reason).
                        self.apply_overflow_retention(record, service, &mut rec, raw);
                        self.emit_record(rec, service);
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
                        // §6.5: bump `params_overflow_total` for
                        // any overflow params (body is already
                        // retained for the parse-failure reason).
                        self.apply_overflow_retention(record, service, &mut rec, raw);
                        self.emit_record(rec, service);
                        self.record_parse_failure(record, service);
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
        line_wildcard_positions: &[usize],
    ) -> Option<Candidate> {
        let state = self.tenants.get(&record.tenant_id)?;
        // Per-tenant `prefix_depth` (RFC 0004 §3.4) — read from
        // the captured `state.config` rather than the cluster
        // default.
        let parent = state
            .tree
            .descend(masked_strs, state.config.prefix_depth as usize)?;

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
            let similarity = sim_seq_owned(masked_strs, &leaf.template, line_wildcard_positions);
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
    /// `template_id`, materialises the prefix path, pushes a leaf
    /// whose template carries `OwnedToken::Wildcard` at every
    /// mask-emitted position and `OwnedToken::Fixed` elsewhere.
    /// `slot_types` is seeded from `typed_params` in ordinal order
    /// — `slot_types[k]` is the singleton `{typed_params[k]
    /// .type_tag}` for the k-th masked position, recording the
    /// type observed at that slot's first sight.
    ///
    /// RFC0001.1: this path **does not** emit an audit event —
    /// `template_count` already reflects the allocation and
    /// `merges_total` is reserved for widening / type-expansion
    /// events on existing leaves.
    fn create_new_leaf(
        &mut self,
        record: &OtlpLogRecord,
        raw: &str,
        masked_strs: &[&str],
        line_wildcard_positions: &[usize],
        line_typed_params: &[crate::mask::TypedParam<'_>],
    ) -> u64 {
        debug_assert_eq!(
            line_wildcard_positions.len(),
            line_typed_params.len(),
            "mask invariant: typed_params parallel to wildcard_positions",
        );
        let new_id = self.next_template_id;
        self.next_template_id += 1;

        // Resolve effective config BEFORE the entry/get-or-insert
        // borrow on `self.tenants` — the `or_insert_with` closure
        // can't reach back to `self.tenant_overrides` while the
        // map is borrowed mutably.
        let effective_config = self.effective_config(&record.tenant_id);
        // Scope the `self.tenants` borrow so it is released before the
        // audit emit below (which borrows `self.audit_sink`); the leaf's
        // canonical template string is computed inside and handed out.
        let created_template = {
            let state = self
                .tenants
                .entry(record.tenant_id.clone())
                .or_insert_with(|| TenantState::new(effective_config));
            let parent = state
                .tree
                .descend_mut(masked_strs, state.config.prefix_depth as usize);
            // Build the leaf template: Wildcard at every mask-emitted
            // position, Fixed at every other. `wildcard_positions` is
            // ascending (single forward pass over the tokens) so we
            // can walk both arrays in lockstep without allocating a
            // membership set.
            let mut new_template = Vec::with_capacity(masked_strs.len());
            let mut wp_iter = line_wildcard_positions.iter().copied().peekable();
            for (p, s) in masked_strs.iter().enumerate() {
                if wp_iter.peek().copied() == Some(p) {
                    new_template.push(OwnedToken::Wildcard);
                    wp_iter.next();
                } else {
                    new_template.push(OwnedToken::Fixed((*s).to_string()));
                }
            }
            debug_assert!(
                wp_iter.peek().is_none(),
                "every wildcard_position must land within masked_strs.len()",
            );
            let slot_types: Vec<SlotTypes> = line_typed_params
                .iter()
                .map(|tp| SlotTypes::singleton(tp.type_tag))
                .collect();
            // Canonical form for the audit event, taken before `new_template`
            // is moved into the leaf.
            let created_template = format_template(&new_template);
            parent.leaves.push(Leaf {
                template: new_template,
                template_id: new_id,
                template_version: 1,
                severity_number: record.severity_number,
                scope_name: record.scope_name.clone(),
                slot_types,
            });
            // Maintain the TenantState::template_count cache invariant —
            // every fresh allocation under `state` is mirrored here so
            // `MinerCluster::template_count` can stay O(1).
            state.template_count += 1;
            created_template
        };
        // RFC 0017 §3.1 — audit the leaf's initial (version 1) creation so a
        // read-time template registry can recover the v1 tokens once the
        // originating rows age out. Same WAL-before-ack path as the widening
        // events; not a merge, so it does not bump `merges_total`.
        self.audit_sink.emit(AuditEvent {
            tenant_id: record.tenant_id.clone(),
            timestamp: self.clock.now(),
            payload: AuditPayload::Template {
                template_id: new_id,
                triggering_line_hash: hash_triggering_line(raw.as_bytes()),
                triggering_line_sample: Some(sample_first_256_bytes(raw)),
                change: TemplateChange::Created {
                    new_template: created_template,
                },
            },
        });
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
        service: Option<&str>,
        raw: &str,
        masked_strs: &[&str],
        line_wildcard_positions: &[usize],
        line_typed_params: &[crate::mask::TypedParam<'_>],
        candidate: Candidate,
        separators: Vec<String>,
        params: Vec<Param>,
        byte_limit: u32,
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
            let parent = state
                .tree
                .descend_mut(masked_strs, state.config.prefix_depth as usize);
            let leaf = &mut parent.leaves[candidate.leaf_idx];
            plan_attach(
                leaf,
                masked_strs,
                line_wildcard_positions,
                line_typed_params,
                byte_limit,
            )
        };

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
                rec.separators = separators;
                rec.params = aligned_params;
                rec.confidence = 1.0;
                // §6.5: force body retention on this clean-reuse
                // record if any of its aligned params overflowed.
                self.apply_overflow_retention(record, service, &mut rec, raw);
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
                self.audit_sink.emit(AuditEvent {
                    tenant_id: record.tenant_id.clone(),
                    timestamp: self.clock.now(),
                    payload: AuditPayload::Template {
                        template_id,
                        triggering_line_hash: hash_triggering_line(raw.as_bytes()),
                        triggering_line_sample: Some(sample_first_256_bytes(raw)),
                        change: TemplateChange::RejectedDegenerate {
                            version,
                            current_template,
                            would_be_template,
                            would_be_positions,
                        },
                    },
                });
                // §6.4 treats degenerate widening as a parse
                // failure that retains body. `lossy_flag = true`
                // so reconstruct surfaces the retained body and
                // ignores `params`; the line-ordered fallback is
                // fine here.
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.separators = separators;
                rec.params = params;
                rec.body = Some(raw.to_string());
                rec.lossy_flag = true;
                // §6.5: bump `params_overflow_total` if the
                // line-ordered params contained any oversized
                // values (body already retained for §6.4).
                self.apply_overflow_retention(record, service, &mut rec, raw);
                self.emit_record(rec, service);
                self.record_parse_failure(record, service);
                NO_TEMPLATE
            }
            AttachPlan::Mutated {
                template_id,
                events,
                final_version,
                params: aligned_params,
            } => {
                for change in events {
                    let counts_as_merge = change.counts_as_merge();
                    let event_type = change.event_type();
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
                    if counts_as_merge {
                        self.merges_total.fetch_add(1, Ordering::Relaxed);
                        self.metrics.record_merge(&record.tenant_id, event_type);
                    }
                }
                let mut rec = Self::record_envelope(record, BodyKind::String);
                rec.template_id = template_id;
                rec.template_version = final_version;
                rec.separators = separators;
                rec.params = aligned_params;
                rec.confidence = 1.0;
                // §6.5: force body retention on this widened /
                // type-expanded record if any of its aligned
                // params overflowed.
                self.apply_overflow_retention(record, service, &mut rec, raw);
                self.emit_record(rec, service);
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
    fn ingest_structured(
        &mut self,
        record: &OtlpLogRecord,
        service: Option<&str>,
        any_value: &ourios_core::otlp::AnyValue,
    ) -> u64 {
        let key = (record.severity_number, record.scope_name.clone());
        // Same pre-compute pattern as `create_new_leaf`: resolve
        // effective config before the mutable borrow on
        // `self.tenants`.
        let effective_config = self.effective_config(&record.tenant_id);
        let state = self
            .tenants
            .entry(record.tenant_id.clone())
            .or_insert_with(|| TenantState::new(effective_config));
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
        // `body` field (per §6.2 step 0), and `lossy_flag = false`
        // per RFC §6.1 ("Always false when body_kind =
        // Structured").
        //
        // `body` carries the RFC 0005 §3.3 OTLP-canonical-JSON
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
        let mut rec = Self::record_envelope(record, BodyKind::Structured);
        rec.template_id = template_id;
        rec.template_version = 1;
        rec.confidence = 1.0;
        rec.body = Some(String::from_utf8(bytes).expect("serde_json emits valid UTF-8"));
        self.emit_record(rec, service);

        template_id
    }

    /// Number of distinct templates this tenant has accumulated
    /// (tree leaves + structured-template entries). Returns 0 for
    /// a tenant the cluster has never seen.
    ///
    /// O(1): served from the `TenantState::template_count`
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

    /// Capture one tenant's full template state as a serialisable
    /// [`SnapshotState`](crate::snapshot::SnapshotState) per RFC 0001
    /// §6.9. Returns an empty state (no leaves, no structured
    /// templates) for an unseen tenant.
    ///
    /// This is the producer side of the §6.9 snapshot format: it
    /// captures every `Body::String` leaf (template tokens,
    /// `template_id`, `template_version`, the `(severity_number,
    /// scope_name)` template key, and per-slot `slot_types`) plus the
    /// §6.2 step-0 structured-template-id map. `wal_high_water` is the
    /// caller's to supply — the cluster does not track WAL offsets —
    /// so it is left `None` here; the snapshot writer fills it from
    /// the WAL at the segment-rotation boundary it snapshots on.
    #[must_use]
    pub fn snapshot_state(&self, tenant_id: &TenantId) -> crate::snapshot::SnapshotState {
        use crate::snapshot::{
            LeafRecord, SnapshotState, StructuredTemplateRecord, TokenRecord,
            slot_types_vec_to_record,
        };

        let Some(state) = self.tenants.get(tenant_id) else {
            return SnapshotState {
                leaves: Vec::new(),
                structured_templates: Vec::new(),
                wal_high_water: None,
            };
        };

        // `collect_leaves` and the `structured_templates` map both iterate
        // in `HashMap` order, which varies across runs — sort by the
        // cluster-unique `template_id` so the serialized snapshot is
        // byte-deterministic (no spurious churn between snapshots of an
        // unchanged tree).
        let mut leaves: Vec<LeafRecord> = state
            .tree
            .collect_leaves()
            .into_iter()
            .map(|leaf| LeafRecord {
                template: leaf.template.iter().map(TokenRecord::from).collect(),
                template_id: leaf.template_id,
                template_version: leaf.template_version,
                severity_number: leaf.severity_number,
                scope_name: leaf.scope_name.clone(),
                slot_types: slot_types_vec_to_record(&leaf.slot_types),
            })
            .collect();
        leaves.sort_by_key(|leaf| leaf.template_id);

        let mut structured_templates: Vec<StructuredTemplateRecord> = state
            .structured_templates
            .iter()
            .map(
                |((severity_number, scope_name), template_id)| StructuredTemplateRecord {
                    severity_number: *severity_number,
                    scope_name: scope_name.clone(),
                    template_id: *template_id,
                },
            )
            .collect();
        structured_templates.sort_by_key(|record| record.template_id);

        SnapshotState {
            leaves,
            structured_templates,
            wal_high_water: None,
        }
    }

    /// Every tenant with allocated state, sorted for determinism —
    /// the snapshot writer iterates this to produce one artefact
    /// per tenant in a stable order.
    #[must_use]
    pub fn tenant_ids(&self) -> Vec<TenantId> {
        let mut ids: Vec<TenantId> = self.tenants.keys().cloned().collect();
        ids.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        ids
    }

    /// Restore one tenant's template state from a deserialised
    /// snapshot — RFC 0001 §6.9 step (2)'s tree restore, active per
    /// the 2026-06-12 v2 amendment. The caller (the ingester's
    /// recovery driver) runs this **before** any live ingest for
    /// `tenant_id`, then replays only the WAL tail above the
    /// snapshot's recorded high-water mark.
    ///
    /// # Errors
    ///
    /// - [`RestoreError::TenantAlreadyLive`] if the tenant already
    ///   has state — restoring over a live tree would double-apply
    ///   the lines the snapshot captured.
    /// - [`RestoreError::Inconsistent`] if the snapshot violates a
    ///   live-tree invariant. The driver maps this to *discard and
    ///   full-replay* — §6.9 treats a semantically inconsistent
    ///   snapshot exactly like a corrupt one.
    pub fn restore_tenant(
        &mut self,
        tenant_id: &TenantId,
        state: &crate::snapshot::SnapshotState,
    ) -> Result<(), RestoreError> {
        if self.tenants.contains_key(tenant_id) {
            return Err(RestoreError::TenantAlreadyLive);
        }
        let config = self.effective_config(tenant_id);
        let prefix_depth = usize::from(config.prefix_depth);
        let mut tenant = TenantState::new(config);

        // Ids are unique cluster-wide and the structured map keys on
        // (severity, scope); a duplicate of either could not have
        // come from a live tree, and silently keeping one of the two
        // entries would desync `template_count` from the tree.
        let mut seen_ids: HashSet<u64> = HashSet::new();

        for record in &state.leaves {
            if !seen_ids.insert(record.template_id) {
                return Err(RestoreError::Inconsistent {
                    detail: format!("template_id {} appears more than once", record.template_id),
                });
            }
            // Live ingest guarantees ≥ 1 token (tokenize); an empty
            // template could not have come from a live tree.
            if record.template.is_empty() {
                return Err(RestoreError::Inconsistent {
                    detail: format!("template_id {}: empty template", record.template_id),
                });
            }
            let template: Vec<OwnedToken> = record.template.iter().map(OwnedToken::from).collect();
            let wildcard_count = template
                .iter()
                .filter(|t| matches!(t, OwnedToken::Wildcard))
                .count();
            let slot_types = restore_slot_types(record, wildcard_count)?;

            // `descend_mut` reads the slice length as the length
            // bucket and only the first `min(prefix_depth, len)`
            // entries as the prefix path, so positions past the path
            // take a filler. A path-position wildcard can only arise
            // from mask emission at leaf creation, and every line
            // reaching the leaf carries the identical masked tag
            // there — widening and type-expansion are impossible at
            // path positions because tree candidates share their
            // first walk_depth masked tokens by construction. Its
            // recorded slot set is therefore a singleton of a
            // mask-emitted type, whose tag string is the path
            // component.
            let walk_depth = prefix_depth.min(template.len());
            let mut masked: Vec<&str> = Vec::with_capacity(template.len());
            let mut slot = 0usize;
            for (position, token) in template.iter().enumerate() {
                match token {
                    OwnedToken::Fixed(s) => masked.push(s),
                    OwnedToken::Wildcard if position < walk_depth => {
                        masked.push(path_tag(record, slot, position)?);
                        slot += 1;
                    }
                    OwnedToken::Wildcard => {
                        masked.push("<*>");
                        slot += 1;
                    }
                }
            }

            let parent = tenant.tree.descend_mut(&masked, prefix_depth);
            parent.leaves.push(Leaf {
                template,
                template_id: record.template_id,
                template_version: record.template_version,
                severity_number: record.severity_number,
                scope_name: record.scope_name.clone(),
                slot_types,
            });
        }

        for record in &state.structured_templates {
            if !seen_ids.insert(record.template_id) {
                return Err(RestoreError::Inconsistent {
                    detail: format!("template_id {} appears more than once", record.template_id),
                });
            }
            let key = (record.severity_number, record.scope_name.clone());
            if tenant
                .structured_templates
                .insert(key, record.template_id)
                .is_some()
            {
                return Err(RestoreError::Inconsistent {
                    detail: format!(
                        "structured key (severity {}, scope {:?}) appears more than once",
                        record.severity_number, record.scope_name,
                    ),
                });
            }
        }
        // Mirror live ingest's cache invariant: every fresh
        // allocation — tree leaf or structured-map entry — counts.
        tenant.template_count = state.leaves.len() + state.structured_templates.len();

        // The id allocator is cluster-wide; without this bump a
        // post-restore allocation would collide with a restored id.
        let max_restored = state
            .leaves
            .iter()
            .map(|l| l.template_id)
            .chain(state.structured_templates.iter().map(|s| s.template_id))
            .max();
        if let Some(max_restored) = max_restored {
            self.next_template_id = self.next_template_id.max(max_restored + 1);
        }

        self.tenants.insert(tenant_id.clone(), tenant);
        Ok(())
    }
}

/// Rebuild a leaf's per-slot type sets from the recorded snapshot
/// during [`MinerCluster::restore_tenant`], rejecting a set count
/// that disagrees with the wildcard count or an empty recorded set
/// (live ingest produces neither).
fn restore_slot_types(
    record: &crate::snapshot::LeafRecord,
    wildcard_count: usize,
) -> Result<Vec<SlotTypes>, RestoreError> {
    if record.slot_types.len() != wildcard_count {
        return Err(RestoreError::Inconsistent {
            detail: format!(
                "template_id {}: {} slot-type sets for {wildcard_count} wildcard slots",
                record.template_id,
                record.slot_types.len(),
            ),
        });
    }
    let mut slot_types = Vec::with_capacity(record.slot_types.len());
    for (slot, recorded) in record.slot_types.iter().enumerate() {
        let mut types = recorded.iter().copied().map(ParamType::from);
        let Some(first) = types.next() else {
            return Err(RestoreError::Inconsistent {
                detail: format!(
                    "template_id {} slot {slot}: empty recorded type set",
                    record.template_id,
                ),
            });
        };
        slot_types.push(types.fold(SlotTypes::singleton(first), SlotTypes::insert));
    }
    Ok(slot_types)
}

/// Resolve the descend-path component for a wildcard at a prefix
/// position during [`MinerCluster::restore_tenant`]. See the
/// path-position rationale at the call site.
fn path_tag(
    record: &crate::snapshot::LeafRecord,
    slot: usize,
    position: usize,
) -> Result<&'static str, RestoreError> {
    let tag = match record.slot_types[slot].as_slice() {
        [single] => tag_str_for(ParamType::from(*single)),
        _ => None,
    };
    tag.ok_or_else(|| RestoreError::Inconsistent {
        detail: format!(
            "template_id {} slot {slot} at path position {position}: \
             type set {:?} is not a singleton mask-emitted type",
            record.template_id, record.slot_types[slot],
        ),
    })
}

/// Errors from [`MinerCluster::restore_tenant`].
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreError {
    /// The tenant already has live state. Restore runs before live
    /// ingest; restoring over a live tree would double-apply the
    /// lines the snapshot captured.
    TenantAlreadyLive,
    /// The snapshot violates a live-tree invariant. The recovery
    /// driver maps this to *discard and full-replay* — RFC 0001
    /// §6.9 treats a semantically inconsistent snapshot exactly
    /// like a corrupt one.
    Inconsistent {
        /// Names the offending `template_id` and slot.
        detail: String,
    },
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TenantAlreadyLive => {
                f.write_str("tenant already has live state; restore must precede ingest")
            }
            Self::Inconsistent { detail } => write!(f, "inconsistent snapshot: {detail}"),
        }
    }
}

impl std::error::Error for RestoreError {}

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
fn find_widening_positions(
    line: &[&str],
    template: &[OwnedToken],
    line_wildcard_positions: &[usize],
) -> Vec<usize> {
    debug_assert_eq!(line.len(), template.len());
    line.iter()
        .zip(template.iter())
        .enumerate()
        .filter_map(|(i, (l, t))| match t {
            OwnedToken::Fixed(s) => {
                // Symmetric with `sim_seq_owned`'s Fixed-match
                // rule: a leaf `Fixed` matches `line[i]` only when
                // the strings agree AND the line at `i` is *not*
                // a mask-emit. The literal-tag collision
                // (`Fixed("<NUM>")` ≡ a literal `<NUM>` user
                // input, line at `i` is a real numeric → masked
                // string also `"<NUM>"`) must widen so the line's
                // typed value lands in `params` rather than being
                // silently absorbed by string equality.
                let line_is_mask_emit = line_wildcard_positions.binary_search(&i).is_ok();
                if !line_is_mask_emit && s.as_str() == *l {
                    None
                } else {
                    Some(i)
                }
            }
            OwnedToken::Wildcard => None,
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

    use crate::snapshot::{
        LeafRecord, ParamTypeRecord, SnapshotState, StructuredTemplateRecord, TokenRecord,
    };

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

    /// Drain the sink and return only the template *changes* a widening /
    /// type-expansion / rejection test asserts on, dropping the per-leaf
    /// `Created` events RFC 0017 §3.1 emits on every allocation. Leaf
    /// creation is audited now (so a read-time registry can recover v1
    /// tokens), but its correctness is covered by
    /// `fresh_leaf_emits_created_event` and the RFC0017.1 acceptance test;
    /// filtering here keeps each widening test decoupled from how many
    /// leaves the scenario happens to allocate rather than re-asserting the
    /// creation count in every one.
    fn drain_changes(sink: &SharedAuditSink) -> Vec<AuditEvent> {
        sink.drain()
            .into_iter()
            .filter(|e| {
                !matches!(
                    &e.payload,
                    AuditPayload::Template {
                        change: TemplateChange::Created { .. },
                        ..
                    }
                )
            })
            .collect()
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
    fn snapshot_state_orders_records_by_template_id() {
        // The tree and the structured-template map both iterate in
        // `HashMap` order; `snapshot_state` sorts by the cluster-unique
        // `template_id` so the serialized snapshot is byte-deterministic.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");
        for line in [
            "GET /home 200",
            "user 42 logged in",
            "cache evicted 5 keys",
            "disk usage high",
        ] {
            let _ = cluster.ingest(&string_record(&t, line));
        }
        // Distinct (severity, scope) keys populate the structured-template
        // map so its ordering is exercised too.
        for (severity, scope) in [(9, Some("lib.a")), (5, Some("lib.b")), (13, None)] {
            let _ = cluster.ingest(&structured_record(&t, severity, scope));
        }

        let state = cluster.snapshot_state(&t);

        assert!(state.leaves.len() >= 2, "needs multiple leaves to order");
        assert!(
            state
                .leaves
                .windows(2)
                .all(|w| w[0].template_id <= w[1].template_id),
            "snapshot leaves must be sorted by template_id, got {:?}",
            state
                .leaves
                .iter()
                .map(|l| l.template_id)
                .collect::<Vec<_>>(),
        );
        assert!(
            state.structured_templates.len() >= 2,
            "needs multiple structured templates to order",
        );
        assert!(
            state
                .structured_templates
                .windows(2)
                .all(|w| w[0].template_id <= w[1].template_id),
            "structured templates must be sorted by template_id, got {:?}",
            state
                .structured_templates
                .iter()
                .map(|s| s.template_id)
                .collect::<Vec<_>>(),
        );
    }

    // ---------- §6.9 restore (RFC 0001 v2 amendment) ----------

    #[test]
    fn restore_round_trips_snapshot_state() {
        let mut original = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");
        // Varied shapes: typed wildcards (NUM, UUID), a widened slot
        // ("in"/"out" → Str wildcard past the prefix path), and a
        // no-wildcard leaf.
        for line in [
            "user 42 logged in",
            "user 17 logged out",
            "GET /home 200",
            "request 550e8400-e29b-41d4-a716-446655440000 accepted",
        ] {
            let _ = original.ingest(&string_record(&t, line));
        }
        for (severity, scope) in [(9, Some("lib.a")), (13, None)] {
            let _ = original.ingest(&structured_record(&t, severity, scope));
        }
        let s1 = original.snapshot_state(&t);

        let mut restored = MinerCluster::new(MinerConfig::default());
        restored
            .restore_tenant(&t, &s1)
            .expect("restore succeeds on a live-produced snapshot");

        assert_eq!(restored.snapshot_state(&t), s1);
        assert_eq!(restored.template_count(&t), original.template_count(&t));
    }

    #[test]
    fn restored_tree_continues_identically() {
        let t = TenantId::new("tenant-x");
        let mut original = MinerCluster::new(MinerConfig::default());
        for line in ["user 42 logged in", "GET /home 200"] {
            let _ = original.ingest(&string_record(&t, line));
        }
        let mut restored = MinerCluster::new(MinerConfig::default());
        restored
            .restore_tenant(&t, &original.snapshot_state(&t))
            .expect("restore succeeds");

        // §3.5.3 equivalence at the miner level: the same follow-up
        // lines must match the same templates AND allocate the same
        // fresh ids in both clusters.
        for line in [
            "user 17 logged in",    // attaches to the restored leaf
            "cache evicted 5 keys", // allocates a fresh id
        ] {
            let rec = string_record(&t, line);
            assert_eq!(
                original.ingest(&rec),
                restored.ingest(&rec),
                "line {line:?}"
            );
        }
        assert_eq!(restored.snapshot_state(&t), original.snapshot_state(&t));
    }

    #[test]
    fn restore_with_wildcard_in_prefix_path() {
        // The first token masks (IPv4) → the leaf carries Wildcard
        // at path position 0; restore must rebuild the descend path
        // from the slot's mask tag.
        let t = TenantId::new("tenant-x");
        let mut original = MinerCluster::new(MinerConfig::default());
        let id = original.ingest(&string_record(&t, "10.0.0.1 connection accepted"));
        let s1 = original.snapshot_state(&t);
        assert!(
            matches!(s1.leaves[0].template[0], TokenRecord::Wildcard),
            "precondition: the leaf must carry a wildcard at path position 0",
        );

        let mut restored = MinerCluster::new(MinerConfig::default());
        restored.restore_tenant(&t, &s1).expect("restore succeeds");
        assert_eq!(restored.snapshot_state(&t), s1);

        // A new matching line attaches to the restored leaf: same
        // id, no new template, version unchanged.
        let id2 = restored.ingest(&string_record(&t, "10.0.0.2 connection accepted"));
        assert_eq!(id2, id);
        assert_eq!(restored.template_count(&t), 1);
        assert_eq!(restored.templates_for(&t)[0].template_version, 1);
    }

    #[test]
    fn restore_rejects_live_tenant() {
        let t = TenantId::new("tenant-x");
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let _ = cluster.ingest(&string_record(&t, "hello world"));
        let snapshot = cluster.snapshot_state(&t);

        let err = cluster
            .restore_tenant(&t, &snapshot)
            .expect_err("restoring over a live tenant must fail");
        assert!(matches!(err, RestoreError::TenantAlreadyLive));
    }

    #[test]
    fn restore_rejects_inconsistent_slot() {
        // A path-position wildcard can only arise from mask
        // emission, so its recorded slot set must be a singleton
        // mask-emitted type; `[Str]` at position 0 cannot come
        // from a live tree.
        let state = SnapshotState {
            leaves: vec![LeafRecord {
                template: vec![
                    TokenRecord::Wildcard,
                    TokenRecord::Fixed("connection".to_string()),
                    TokenRecord::Fixed("accepted".to_string()),
                ],
                template_id: 1,
                template_version: 1,
                severity_number: 0,
                scope_name: None,
                slot_types: vec![vec![ParamTypeRecord::Str]],
            }],
            structured_templates: vec![],
            wal_high_water: None,
        };
        let mut cluster = MinerCluster::new(MinerConfig::default());

        let err = cluster
            .restore_tenant(&TenantId::new("tenant-x"), &state)
            .expect_err("a Str slot at a path position must be inconsistent");
        assert!(matches!(err, RestoreError::Inconsistent { .. }));
    }

    #[test]
    fn restore_rejects_duplicate_template_id() {
        // Ids are unique cluster-wide; the same id on a leaf and a
        // structured template could not come from a live tree.
        let state = SnapshotState {
            leaves: vec![LeafRecord {
                template: vec![
                    TokenRecord::Fixed("disk".to_string()),
                    TokenRecord::Fixed("full".to_string()),
                ],
                template_id: 7,
                template_version: 1,
                severity_number: 0,
                scope_name: None,
                slot_types: vec![],
            }],
            structured_templates: vec![StructuredTemplateRecord {
                severity_number: 9,
                scope_name: None,
                template_id: 7,
            }],
            wal_high_water: None,
        };
        let mut cluster = MinerCluster::new(MinerConfig::default());

        let err = cluster
            .restore_tenant(&TenantId::new("tenant-x"), &state)
            .expect_err("a duplicate template_id must be inconsistent");
        match err {
            RestoreError::Inconsistent { detail } => {
                assert!(detail.contains('7'), "detail names the id, got {detail:?}");
            }
            other => panic!("expected Inconsistent, got {other:?}"),
        }
    }

    #[test]
    fn restore_rejects_duplicate_structured_key() {
        // The structured map keys on (severity, scope); a duplicate
        // key would silently drop one entry while template_count
        // counted both.
        let state = SnapshotState {
            leaves: vec![],
            structured_templates: vec![
                StructuredTemplateRecord {
                    severity_number: 9,
                    scope_name: Some("lib.a".to_string()),
                    template_id: 1,
                },
                StructuredTemplateRecord {
                    severity_number: 9,
                    scope_name: Some("lib.a".to_string()),
                    template_id: 2,
                },
            ],
            wal_high_water: None,
        };
        let mut cluster = MinerCluster::new(MinerConfig::default());

        let err = cluster
            .restore_tenant(&TenantId::new("tenant-x"), &state)
            .expect_err("a duplicate structured key must be inconsistent");
        match err {
            RestoreError::Inconsistent { detail } => {
                assert!(
                    detail.contains('9') && detail.contains("lib.a"),
                    "detail names the key, got {detail:?}",
                );
            }
            other => panic!("expected Inconsistent, got {other:?}"),
        }
    }

    #[test]
    fn restore_bumps_the_id_allocator() {
        // The allocator is cluster-wide; a restored id must never
        // be re-minted for a new template.
        let state = SnapshotState {
            leaves: vec![LeafRecord {
                template: vec![
                    TokenRecord::Fixed("disk".to_string()),
                    TokenRecord::Fixed("usage".to_string()),
                    TokenRecord::Fixed("high".to_string()),
                ],
                template_id: 7,
                template_version: 1,
                severity_number: 0,
                scope_name: None,
                slot_types: vec![],
            }],
            structured_templates: vec![],
            wal_high_water: None,
        };
        let t = TenantId::new("tenant-x");
        let mut cluster = MinerCluster::new(MinerConfig::default());
        cluster
            .restore_tenant(&t, &state)
            .expect("restore succeeds");

        let new_id = cluster.ingest(&string_record(&t, "cache evicted 5 keys"));
        assert!(
            new_id >= 8,
            "new template must not collide with restored id 7, got {new_id}",
        );
    }

    #[test]
    fn tenant_ids_returns_sorted_tenants() {
        let mut cluster = MinerCluster::new(MinerConfig::default());
        for name in ["tenant-b", "tenant-a", "tenant-c"] {
            let _ = cluster.ingest(&string_record(&TenantId::new(name), "hello world"));
        }

        assert_eq!(
            cluster.tenant_ids(),
            vec![
                TenantId::new("tenant-a"),
                TenantId::new("tenant-b"),
                TenantId::new("tenant-c"),
            ],
        );
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

        let events = drain_changes(&sink);
        assert_eq!(events.len(), 1);
        let AuditPayload::Template {
            template_id,
            change:
                TemplateChange::Widened {
                    old_version,
                    new_version,
                    positions_widened,
                    old_template,
                    new_template,
                },
            ..
        } = &events[0].payload
        else {
            panic!("expected Template/Widened, got {:?}", events[0].payload);
        };
        assert_eq!(*template_id, id_in);
        assert_eq!(*old_version, 1);
        assert_eq!(*new_version, 2);
        assert_eq!(*positions_widened, vec![3]);
        // PR-B-1: mask-emit positions (`<NUM>` at index 1, `<IP>`
        // at index 5) enter the leaf as `Wildcard` from creation,
        // so the canonical-form template renders them as `<*>`,
        // not as the tag string. The audit-event shape is
        // unchanged — only the rendered template strings differ
        // because the type information now lives in the parallel
        // `slot_types` vector rather than encoded in the template
        // (RFC 0001 §6.6 reconstruction substitutes back via
        // `params`, not the template string).
        assert_eq!(old_template, "user <*> logged in from <*>");
        assert_eq!(new_template, "user <*> logged <*> from <*>");
    }

    #[test]
    fn fresh_leaf_emits_created_event() {
        // RFC 0017 §3.1 overturns the former "fresh leaf emits nothing"
        // contract: leaf allocation now emits a `template_created` audit
        // event (so a read-time registry can recover v1 tokens), while
        // still NOT counting as a merge. Two distinct fresh leaves →
        // exactly two `Created` events, `merges_total` still 0.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let id_a = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let id_b = cluster.ingest(&string_record(&t, "GET /home 200"));

        assert_eq!(cluster.template_count(&t), 2);
        assert_eq!(cluster.merges_total(), 0, "creation is not a merge");

        let events = sink.drain();
        assert_eq!(events.len(), 2, "one Created event per fresh leaf");
        for (event, id) in events.iter().zip([id_a, id_b]) {
            let AuditPayload::Template {
                template_id,
                change: TemplateChange::Created { new_template },
                ..
            } = &event.payload
            else {
                panic!("expected Template/Created, got {:?}", event.payload);
            };
            assert_eq!(*template_id, id);
            assert!(
                !new_template.is_empty(),
                "creation carries the initial tokens",
            );
        }
    }

    #[test]
    fn exact_sim_seq_match_attaches_without_widening_or_audit() {
        // A line whose mask matches an existing leaf exactly (no
        // mismatched Fixed positions, no new ParamType at any
        // existing wildcard) reuses the leaf with no version bump
        // and no audit event.
        //
        // PR-B-1 locking-test update: under the new leaf model
        // mask-emit positions enter the leaf as `Wildcard` from
        // creation (with `slot_types[0] = {Num}` for the `<NUM>`
        // at position 1). The relevant contract is therefore
        // "no version bump, no audit, slot_types unchanged" — not
        // "no wildcards in the template at all". Both lines mask
        // to the same shape (`<NUM>` at position 1), and the
        // second line's `<NUM>` is already in `slot_types[0]`'s
        // set, so type-expansion doesn't fire either. The leaf's
        // wildcard set is asserted explicitly so a future bug
        // that accidentally widened position 2 or 3 still fails.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let id2 = cluster.ingest(&string_record(&t, "user 17 logged in"));

        assert_eq!(id1, id2);
        assert_eq!(cluster.merges_total(), 0);
        assert!(drain_changes(&sink).is_empty());

        let templates = cluster.templates_for(&t);
        assert_eq!(templates.len(), 1);
        assert_eq!(
            templates[0].template_version, 1,
            "no version bump on same-shape clean attach",
        );
        let wildcard_positions: Vec<usize> = templates[0]
            .template
            .iter()
            .enumerate()
            .filter_map(|(i, t)| matches!(t, OwnedToken::Wildcard).then_some(i))
            .collect();
        assert_eq!(
            wildcard_positions,
            vec![1],
            "leaf's wildcard set must match the line's mask set: {:?}",
            templates[0].template,
        );
        // The slot's type set stayed at {Num} — the second `<NUM>`
        // line is already in the set, so no expansion fired.
        assert_eq!(templates[0].slot_types.len(), 1);
        let types: Vec<_> = templates[0].slot_types[0].iter().collect();
        assert_eq!(types, vec![ParamType::Num]);
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

        let events = drain_changes(&sink);
        assert_eq!(events.len(), 1);
        let AuditPayload::Template {
            change:
                TemplateChange::Widened {
                    old_version,
                    new_version,
                    ..
                },
            ..
        } = &events[0].payload
        else {
            panic!("expected Template/Widened, got {:?}", events[0].payload);
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

        let events = drain_changes(&sink);
        assert_eq!(events.len(), 2);
        let AuditPayload::Template {
            change:
                TemplateChange::Widened {
                    old_version: ov0,
                    new_version: nv0,
                    positions_widened: p0,
                    ..
                },
            ..
        } = &events[0].payload
        else {
            panic!(
                "event 0: expected Template/Widened, got {:?}",
                events[0].payload
            );
        };
        assert_eq!((*ov0, *nv0, p0.clone()), (1, 2, vec![4]));
        let AuditPayload::Template {
            change:
                TemplateChange::Widened {
                    old_version: ov1,
                    new_version: nv1,
                    positions_widened: p1,
                    ..
                },
            ..
        } = &events[1].payload
        else {
            panic!(
                "event 1: expected Template/Widened, got {:?}",
                events[1].payload
            );
        };
        assert_eq!((*ov1, *nv1, p1.clone()), (2, 3, vec![5]));
    }

    #[test]
    fn fresh_leaf_carries_wildcard_at_mask_positions_with_seeded_slot_types() {
        // PR-B-1 contract: at fresh-leaf creation, `mask()`'s
        // wildcard_positions feed directly into the leaf's
        // template (Wildcard at those positions, Fixed elsewhere)
        // and `slot_types` is seeded from `typed_params` in
        // ordinal order (one entry per masked position).
        //
        // This is the structural prerequisite for §6.6
        // reconstruction: the template Wildcard slots and the
        // `params` vector now align position-for-ordinal.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        // Mask emits at positions 1 (`<NUM>`) and 5 (`<IP>`).
        let _ = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));

        // Fresh-leaf creation emits a `Created` event now (RFC 0017 §3.1),
        // but no *widening / type-expansion* — even with non-empty
        // slot_types. `drain_changes` filters the Created event out.
        assert!(drain_changes(&sink).is_empty());

        let templates = cluster.templates_for(&t);
        assert_eq!(templates.len(), 1);
        let snap = &templates[0];

        // Template shape: Wildcard at mask positions, Fixed
        // elsewhere.
        assert_eq!(snap.template.len(), 6);
        assert!(matches!(snap.template[0], OwnedToken::Fixed(ref s) if s == "user"));
        assert!(matches!(snap.template[1], OwnedToken::Wildcard));
        assert!(matches!(snap.template[2], OwnedToken::Fixed(ref s) if s == "logged"));
        assert!(matches!(snap.template[3], OwnedToken::Fixed(ref s) if s == "in"));
        assert!(matches!(snap.template[4], OwnedToken::Fixed(ref s) if s == "from"));
        assert!(matches!(snap.template[5], OwnedToken::Wildcard));

        // slot_types seeded from typed_params in ordinal order.
        assert_eq!(snap.slot_types.len(), 2);
        assert_eq!(
            snap.slot_types[0].iter().collect::<Vec<_>>(),
            vec![ParamType::Num],
        );
        assert_eq!(
            snap.slot_types[1].iter().collect::<Vec<_>>(),
            vec![ParamType::Ip],
        );
    }

    // ---------- §6.4 type expansion (PR-B-0) ----------

    #[test]
    fn literal_widening_seeds_slot_types_with_str_for_pre_widen_and_line() {
        // A literal-vs-literal widening at a position that wasn't
        // previously a wildcard. Under PR-B-1 the fresh leaf also
        // carries a Wildcard at position 1 (the `<NUM>` from the
        // mask emit at creation), so after widening position 3
        // there are TWO wildcard slots: ordinal 0 = the mask-emit
        // wildcard (slot_types = {Num}), ordinal 1 = the literal
        // widening (slot_types = {Str}).
        //
        // PR-B-1 locking-test update (was
        // `literal_widening_seeds_slot_types_with_str_for_both_
        // observations` under PR-B-0, when fresh leaves had no
        // wildcards yet). The contract being pinned now is:
        //   - literal widening produces exactly one
        //     `TemplateWidened` event (no type expansion at
        //     position 1 — the existing `<NUM>` slot already
        //     contains Num, and the second line's `<NUM>` doesn't
        //     trigger an expansion).
        //   - The newly-widened slot at ordinal 1 contains {Str}
        //     (the pre-widen literal "in" and the triggering "out"
        //     both classify as Str).
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let _ = cluster.ingest(&string_record(&t, "user 42 logged out"));

        let events = drain_changes(&sink);
        assert_eq!(events.len(), 1, "literal widening: one event only");
        assert!(matches!(
            events[0].payload,
            AuditPayload::Template {
                change: TemplateChange::Widened { .. },
                ..
            }
        ));

        let templates = cluster.templates_for(&t);
        assert_eq!(templates.len(), 1);
        assert_eq!(
            templates[0].slot_types.len(),
            2,
            "two wildcard slots: ordinal 0 from mask emit, ordinal 1 from widening",
        );
        let ordinal_0: Vec<_> = templates[0].slot_types[0].iter().collect();
        assert_eq!(ordinal_0, vec![ParamType::Num]);
        let ordinal_1: Vec<_> = templates[0].slot_types[1].iter().collect();
        assert_eq!(ordinal_1, vec![ParamType::Str]);
    }

    #[test]
    fn mask_tag_transition_at_typed_wildcard_emits_type_expanded() {
        // CLAUDE.md §3.1 regression for mask-tag type transitions
        // at a wildcard slot.
        //
        // Setup: the fresh leaf carries a Wildcard at position 2
        // (from the `<NUM>` mask emit at creation) with
        // `slot_types[0] = {Num}`. The second line lands `<UUID>`
        // at the same position. Under PR-B-1 the leaf position is
        // already a Wildcard, so `find_widening_positions` returns
        // empty (no Fixed mismatch); the §3.1 signal moves to the
        // type-expansion path instead, which fires
        // `TemplateTypeExpanded` and grows the slot's type set.
        //
        // PR-B-1 locking-test update (was
        // `fixed_mask_tag_widening_captures_both_param_types_in_
        // slot` under PR-B-0, when fresh leaves stored
        // `Fixed("<NUM>")` and the same case fired
        // `TemplateWidened`). The §3.1 invariant — every mask-tag
        // transition at a tree-routed slot must produce an audit
        // signal — is preserved end-to-end; only the *event kind*
        // changes.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        // Prefix ["user", "logged"] shared; mask-tag divergence
        // at position 2 (Num vs Uuid).
        let _ = cluster.ingest(&string_record(&t, "user logged 42 in"));
        let _ = cluster.ingest(&string_record(
            &t,
            "user logged 550e8400-e29b-41d4-a716-446655440000 in",
        ));

        let events = drain_changes(&sink);
        assert_eq!(events.len(), 1, "single TemplateTypeExpanded, no widening");
        let AuditPayload::Template {
            change:
                TemplateChange::TypeExpanded {
                    old_version,
                    new_version,
                    slots_expanded,
                    ..
                },
            ..
        } = &events[0].payload
        else {
            panic!(
                "expected Template/TypeExpanded, got {:?}",
                events[0].payload
            );
        };
        assert_eq!((*old_version, *new_version), (1, 2));
        assert_eq!(slots_expanded.len(), 1);
        assert_eq!(slots_expanded[0].slot_index, 0);
        assert_eq!(slots_expanded[0].added_types, vec![ParamType::Uuid]);

        let templates = cluster.templates_for(&t);
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].slot_types.len(), 1);
        let types: Vec<_> = templates[0].slot_types[0].iter().collect();
        assert_eq!(
            types,
            vec![ParamType::Uuid, ParamType::Num],
            "slot must record both Num (from creation) and Uuid (from this attach)",
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
            drain_changes(&sink).is_empty(),
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

        let events = drain_changes(&sink);
        assert_eq!(events.len(), 1, "exactly one TemplateTypeExpanded");
        let AuditPayload::Template {
            change:
                TemplateChange::TypeExpanded {
                    old_version,
                    new_version,
                    slots_expanded,
                    ..
                },
            ..
        } = &events[0].payload
        else {
            panic!(
                "expected Template/TypeExpanded, got {:?}",
                events[0].payload
            );
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
        // `TemplateChange::counts_as_merge`). A pure type-expansion
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

        let events = drain_changes(&sink);
        assert_eq!(events.len(), 2, "combined widening + type expansion");
        let AuditPayload::Template {
            change:
                TemplateChange::Widened {
                    old_version: w_old,
                    new_version: w_new,
                    positions_widened,
                    ..
                },
            ..
        } = &events[0].payload
        else {
            panic!(
                "event 0 must be Template/Widened (widening fires before expansion), got {:?}",
                events[0].payload,
            );
        };
        assert_eq!((*w_old, *w_new), (2, 3));
        assert_eq!(*positions_widened, vec![3]);

        let AuditPayload::Template {
            change:
                TemplateChange::TypeExpanded {
                    old_version: e_old,
                    new_version: e_new,
                    slots_expanded,
                    ..
                },
            ..
        } = &events[1].payload
        else {
            panic!(
                "event 1 must be Template/TypeExpanded, got {:?}",
                events[1].payload,
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
        // Under PR-B-1's leaf model the §3.1 audit signal moves
        // from `TemplateWidened` to `TemplateTypeExpanded`:
        // masked positions enter the leaf as `Wildcard` from
        // creation, so the second line doesn't trigger a Fixed
        // mismatch; the divergence surfaces as the slot's type
        // set growing to {Num, Uuid}. The §3.1 contract — every
        // mask-tag transition at a tree-routed slot audits — is
        // preserved; the event kind changes.
        let (mut cluster, sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        let _ = cluster.ingest(&string_record(&t, "GET /home 42 ok"));
        let _ = cluster.ingest(&string_record(
            &t,
            "GET /home 550e8400-e29b-41d4-a716-446655440000 ok",
        ));

        let events = drain_changes(&sink);
        assert!(
            !events.is_empty(),
            "§3.1: mask-tag type change at a tree-routed wildcard slot must audit",
        );
        let AuditPayload::Template {
            change: TemplateChange::TypeExpanded { slots_expanded, .. },
            ..
        } = &events[0].payload
        else {
            panic!(
                "expected Template/TypeExpanded, got {:?}",
                events[0].payload
            );
        };
        assert_eq!(slots_expanded.len(), 1);
        assert_eq!(slots_expanded[0].slot_index, 0);
        assert_eq!(slots_expanded[0].added_types, vec![ParamType::Uuid]);

        // Confirm the slot's type set captures the divergence so a
        // *third* mask-tag type at the same position would emit
        // another TemplateTypeExpanded.
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
            drain_changes(&sink).is_empty(),
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
    fn literal_mask_tag_in_leaf_vs_real_mask_emit_on_line_does_not_silently_merge() {
        // Symmetric regression for the PR #35 review concern: a
        // leaf with `Fixed("<NUM>")` (because the *first* line
        // carried the literal token `<NUM>`) MUST NOT merge with a
        // later line that puts a real numeric value at the same
        // position. The masked-line token at that position is also
        // `"<NUM>"`, so a naive string-equality match in
        // `sim_seq_owned` / `find_widening_positions` would mark
        // it as a Fixed match — silently merging the two log
        // shapes AND dropping the numeric's value from `params`
        // (§3.1 + §3.3 violation; reconstruct would render
        // `<NUM>` literally instead of recovering `42`).
        //
        // `line_wildcard_positions` plumbed through sim_seq +
        // find_widening fixes both: the line at position 1 is a
        // mask emit (in `line_wildcard_positions`), so it does
        // NOT match the leaf's literal `Fixed("<NUM>")`. sim_seq
        // returns 2/3 ≈ 0.667 < 0.7 threshold → Lossy zone →
        // fresh leaf. Two templates result, body retained on the
        // Lossy line.
        let (mut cluster, audit_sink) = cluster_with_observable_sink();
        let t = TenantId::new("tenant-x");

        // L1: literal `<NUM>` token. mask() doesn't classify it.
        let raw_l1 = "value <NUM> ok";
        let id1 = cluster.ingest(&string_record(&t, raw_l1));
        // L2: real numeric at the same position. mask() emits
        // `<NUM>` here.
        let raw_l2 = "value 42 ok";
        let id2 = cluster.ingest(&string_record(&t, raw_l2));

        // Distinct template ids — no silent merge.
        assert_ne!(
            id1, id2,
            "leaf `Fixed(\"<NUM>\")` (literal) must not absorb a real mask-emit `<NUM>`",
        );
        assert_eq!(cluster.template_count(&t), 2);
        // No widening fired (the Lossy zone created a new leaf rather than
        // widening). The two leaf creations each emit a `Created` event
        // (RFC 0017 §3.1), which `drain_changes` filters out — so there are
        // no widening / type-expansion / rejection events.
        assert!(drain_changes(&audit_sink).is_empty());
        // The §6.3 lossy zone bumped body_retentions for L2 (and
        // retained its body on the emitted record).
        assert_eq!(cluster.body_retentions_total(), 1);
    }

    #[test]
    fn existing_wildcard_receives_literal_emits_str_param_and_reconstructs() {
        // PR-B-2 STR-fallback regression for the "existing wildcard
        // receives a literal observation" path (distinct from the
        // freshly-widened-literal-slot case covered by H7.4).
        //
        // Setup: a prior `<NUM>` mask emit creates a Wildcard at
        // position 2 with `slot_types[0] = {Num}`. A later line at
        // the same position carries a literal whose mask does not
        // classify (e.g. "abc-def-1234" — neither digits nor UUID
        // nor IPv4). sim_seq still matches (Wildcard matches
        // anything), so the attach is Clean — and `build_record_
        // params` must emit `{Str, "abc-def-1234"}` for the slot
        // so reconstruct round-trips the literal verbatim.
        //
        // The pre-PR-B-2 code (params_from_mask) would have emitted
        // params=[] for this attach because mask emitted nothing —
        // reconstruct would have produced no bytes at the wildcard
        // position. PR-B-2's `build_record_params` walks the
        // leaf's wildcards and inserts the STR fallback.
        //
        // **Scope note.** This test exercises a wildcard at
        // template position **2** — that is, *beyond* the default
        // `prefix_depth = 2`, so both ingests share the same
        // tree parent (positions 0–1 = `["user", "logged"]` for
        // both). The STR-fallback path is structurally
        // unreachable for wildcards INSIDE the prefix depth: the
        // tree partitions by the concrete masked token at each
        // prefix level, so a line whose prefix masks to a
        // different concrete token (e.g. literal `abc` vs mask-
        // emitted `<NUM>` at position 1 under default
        // `prefix_depth = 2`) ends up in a different parent and
        // finds no candidate to attach to. That is a property of
        // the Drain tree's prefix-routing scheme (paper §3.2,
        // RFC 0001 §6.1), not a bug in PR-B-2's STR fallback;
        // future work to make wildcard slots reachable from
        // diverging prefix tokens (multi-bucket lookup or
        // wildcard-aware re-bucketing) is its own RFC-level
        // change. The test deliberately stays inside the
        // structurally-reachable case.
        let (mut cluster, _audit, records) = cluster_with_observable_sinks();
        let t = TenantId::new("tenant-x");
        let make = |raw: &str| string_record(&t, raw);

        // L1: creates the wildcard at position 2 with
        // slot_types[0] = {Num} (mask emit).
        let _ = cluster.ingest(&make("user logged 42 in"));
        let l1_emit = records.drain();
        assert_eq!(l1_emit.len(), 1);

        // L2: literal at position 2 lands on the existing
        // wildcard. {Str} expands the slot's type set → emits
        // TemplateTypeExpanded, but the record's params must
        // carry the literal so reconstruct works.
        let raw_l2 = "user logged abc-def-1234 in";
        let _ = cluster.ingest(&make(raw_l2));

        let l2_emit = records.drain();
        assert_eq!(l2_emit.len(), 1);
        let rec = &l2_emit[0];

        // params has exactly one entry for the one wildcard slot,
        // and it's a STR fallback carrying the literal verbatim.
        assert_eq!(rec.params.len(), 1, "one wildcard → one param");
        assert_eq!(
            rec.params[0].type_tag,
            ParamType::Str,
            "literal at an existing wildcard → STR fallback",
        );
        assert_eq!(rec.params[0].value, "abc-def-1234");

        // End-to-end: reconstruct round-trips the original bytes.
        let snapshots = cluster.templates_for(&t);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            crate::reconstruct::reconstruct(rec, &snapshots[0].template),
            raw_l2.as_bytes().to_vec(),
            "STR-fallback alignment must let reconstruct recover the literal byte-for-byte",
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

        let events = drain_changes(&sink);
        assert_eq!(events.len(), 1);
        let AuditPayload::Template {
            triggering_line_hash,
            triggering_line_sample,
            ..
        } = &events[0].payload
        else {
            panic!("expected Template, got {:?}", events[0].payload);
        };
        assert_eq!(*triggering_line_hash, hash_triggering_line(l2.as_bytes()));
        assert_eq!(triggering_line_sample.as_deref(), Some(l2));
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
        assert!(drain_changes(&sink).is_empty());
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

        let events = drain_changes(&sink);
        assert_eq!(events.len(), 2);
        assert!(
            matches!(
                events[0].payload,
                AuditPayload::Template {
                    change: TemplateChange::Widened { .. },
                    ..
                }
            ),
            "event 0: expected Template/Widened, got {:?}",
            events[0].payload,
        );
        // Rejection variant carries no version bump and surfaces
        // the would-be template the operator was protected from.
        let AuditPayload::Template {
            change:
                TemplateChange::RejectedDegenerate {
                    would_be_template, ..
                },
            ..
        } = &events[1].payload
        else {
            panic!(
                "event 1: expected Template/RejectedDegenerate, got {:?}",
                events[1].payload,
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
            drain_changes(&sink).is_empty(),
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
        let events = drain_changes(&sink);
        assert_eq!(events.len(), 1);
        let AuditPayload::Template {
            template_id,
            change: TemplateChange::Widened {
                positions_widened, ..
            },
            ..
        } = &events[0].payload
        else {
            panic!("expected Template/Widened, got {:?}", events[0].payload);
        };
        assert_eq!(*template_id, id_a);
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

    /// Pin the exact RFC 0005 §3.3 canonical-JSON bytes the
    /// miner stores in `MinedRecord.body` for a structured
    /// row. Catches a regression to debug formatting (the
    /// prior `format!("{any_value:?}")` placeholder), AND
    /// catches an `opentelemetry-proto` upgrade that breaks
    /// the OTLP-JSON spec mapping (camelCase, string-encoded
    /// `i64`, base64 bytes). A non-trivial `AnyValue` exercises
    /// the recursive `KvlistValue` path through the encoder.
    #[test]
    fn structured_body_is_stored_as_otlp_canonical_json() {
        use ourios_core::otlp::{KeyValue as ProtoKv, KeyValueList};
        let records = SharedRecordSink::new();
        let mut cluster =
            MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
        let av = AnyValue {
            value: Some(AvValue::KvlistValue(KeyValueList {
                values: vec![ProtoKv {
                    key: "user.id".to_string(),
                    value: Some(AnyValue {
                        value: Some(AvValue::IntValue(42)),
                    }),
                    ..Default::default()
                }],
            })),
        };
        let record = OtlpLogRecord {
            tenant_id: TenantId::new("tenant-x"),
            severity_number: 9,
            scope_name: Some("bench.scope".to_string()),
            body: Some(Body::Structured(av)),
            ..Default::default()
        };
        cluster.ingest(&record);
        let emitted = records.drain();
        assert_eq!(emitted.len(), 1);
        let body = emitted[0].body.as_deref().expect("structured body is Some");
        // Pinned canonical form per the proto3 JSON spec
        // mapping: camelCase keys, `i64` as a quoted string,
        // recursive `kvlistValue` shape. The opentelemetry-proto
        // `with-serde` derives emit fields in struct-definition
        // order, which is what serde_json::to_vec produces
        // deterministically — RFC0006.7 reproducibility relies
        // on this same byte stability.
        assert_eq!(
            body, r#"{"kvlistValue":{"values":[{"key":"user.id","value":{"intValue":"42"}}]}}"#,
            "miner must store RFC 0005 §3.3 canonical JSON, not a debug rendering",
        );
        assert!(
            !emitted[0].lossy_flag,
            "RFC 0001 §6.1: lossy_flag is always false on BodyKind::Structured",
        );
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
        let positions = find_widening_positions(&line, &template, &[]);
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
        assert!(
            drain_changes(&sink).is_empty(),
            "lossy attach emits no audit event"
        );
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
        assert!(
            drain_changes(&sink).is_empty(),
            "parse failure emits no audit event"
        );
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
        // RFC §6.1: Structured records always carry
        // `lossy_flag = false`. The producer populates `body`
        // with a stored representation of the structured value
        // so `reconstruct()` returns what we stored, satisfying
        // §3.3. Today that representation is the AnyValue's
        // `Debug` form — an interim placeholder. The follow-up
        // PR replaces it with OTLP-canonical JSON without
        // changing the schema field or `lossy_flag`. See
        // `ingest_structured` for the rationale.
        assert!(rec.separators.is_empty());
        assert!(rec.params.is_empty());
        assert!(
            rec.body.is_some(),
            "structured records must carry the stored body representation"
        );
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
