//! [`MinerCluster`] construction and builder setters, plus the §6.8
//! cumulative-counter accessors.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use ourios_config::MinerConfig;
use ourios_core::audit::{AuditSink, NoOpAuditSink};
use ourios_core::clock::{Clock, SystemClock};
use ourios_core::record::{NoOpRecordSink, RecordSink};
use ourios_core::tenant::TenantId;

use super::{MinedCapture, MinerCluster};
use crate::metrics::MinerMetrics;

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
            mined_capture: MinedCapture::Off,
            mined_capture_salvages: AtomicU64::new(0),
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
    ///
    /// [`TemplateChange::counts_as_merge`]: ourios_core::audit::TemplateChange::counts_as_merge
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
    ///
    /// [`Param`]: ourios_core::record::Param
    #[must_use]
    pub fn params_overflow_total(&self) -> u64 {
        self.params_overflow_total.load(Ordering::Relaxed)
    }
}
