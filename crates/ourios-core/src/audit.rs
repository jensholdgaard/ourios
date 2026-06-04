//! Audit events for template-mining decisions.
//!
//! RFC 0001 §6.4 commits to an audit-event contract: every
//! widening (or rejected widening) of a leaf's template emits an
//! event recording who decided what and against what input. The
//! event types here are the wire-stable shape; [`AuditSink`] is
//! the boundary at which producers (currently `ourios-miner`,
//! eventually also `ourios-parquet` on read-back) hand events off
//! to whatever records them durably.
//!
//! # Sinks
//!
//! The trait is introduced from day one rather than waiting for a
//! second impl: the WAL adapter that replaces [`InMemoryAuditSink`]
//! is a named roadmap item (`docs/roadmap.md` Phase 2 / RFC 0001
//! §6.4's *WAL durability ordering of audit events*), so the
//! abstraction names a committed contract rather than a
//! hypothetical one. The in-memory impl in this module is the
//! pre-WAL placeholder; the WAL impl will additionally enforce the
//! ordering-plus-durability barrier vs. data records that §6.4
//! requires, which this placeholder does not.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::tenant::TenantId;

/// The specific state-change of a template-mining
/// [`AuditPayload::Template`] event (RFC 0001 §6.4).
///
/// Each variant carries only the fields meaningful for that change
/// — `Widened` cannot represent `slots_expanded`, and
/// `RejectedDegenerate` cannot represent a version bump. The
/// compiler enforces those per-variant contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateChange {
    /// An existing template gained one or more wildcard slots
    /// because a clean attach would otherwise mismatch positions
    /// (RFC §6.2 step 5).
    Widened {
        /// The leaf's `template_version` before this attach.
        old_version: u32,
        /// `old_version + 1`. Pinned by construction: a widening
        /// always bumps the version by exactly one.
        new_version: u32,
        /// Canonical-form template before the widening
        /// (literals + `<*>` for already-wildcard positions).
        old_template: String,
        /// Canonical-form template after the widening.
        new_template: String,
        /// Token positions (zero-indexed) that became `<*>` in
        /// this attach. Always non-empty for this variant — a
        /// would-be widening with zero new wildcards is a clean
        /// attach, which emits no audit event.
        positions_widened: Vec<u16>,
    },
    /// An existing template's wildcard slot type-set widened to
    /// include a new [`ParamType`] (RFC §6.2 step 5 type
    /// expansion).
    ///
    /// **Reserved variant.** Emission lands in the follow-up
    /// type-expansion PR; the variant ships here so the schema is
    /// wire-stable across that change.
    TypeExpanded {
        old_version: u32,
        new_version: u32,
        old_template: String,
        new_template: String,
        /// Wildcard-slot indices and the [`ParamType`]s newly
        /// observed there.
        slots_expanded: Vec<SlotExpansion>,
    },
    /// A would-be widening was rejected because it would have
    /// left the template with zero non-wildcard tokens (RFC §6.4
    /// degenerate-template guard).
    ///
    /// Rejection does not bump `template_version` and does not
    /// mutate the leaf, so there is no `old_version` / `new_version`
    /// pair to carry — just the single `version` that was current
    /// when the rejection happened.
    RejectedDegenerate {
        /// The leaf's `template_version` at the time of rejection.
        version: u32,
        /// The leaf's canonical-form template — unchanged by the
        /// rejection.
        current_template: String,
        /// The canonical-form template the widening *would* have
        /// produced. Surfaced so an operator inspecting the audit
        /// stream can see the degenerate shape that was avoided.
        would_be_template: String,
        /// Positions the rejected widening would have replaced
        /// with `<*>`.
        would_be_positions: Vec<u16>,
    },
}

/// Variant-specific payload for an [`AuditEvent`].
///
/// The top-level axis is *what kind of thing happened*: a
/// template-mining decision about a leaf, or a compaction of a
/// sealed partition (RFC 0009 §3.6). Each carries only the fields
/// that apply to it — a [`AuditPayload::Compaction`] event has no
/// `template_id` by construction, so the invalid combination is
/// unrepresentable. New event kinds (retention, schema evolution,
/// …) join as sibling variants without disturbing the others.
///
/// The miner emits one of these per leaf state change; data records
/// that *cause* a change are durability-ordered after the events
/// justifying their `template_version` stamp (a barrier this enum
/// names but does not itself enforce — see the module-level note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditPayload {
    /// A template-mining decision about a specific leaf
    /// (RFC 0001 §6.4).
    Template {
        /// The leaf the event applies to.
        template_id: u64,
        /// Truncated blake3 of `L_raw`, used by the §6.7 drift
        /// query to join an event to the data record(s) that
        /// triggered it.
        triggering_line_hash: [u8; 16],
        /// First 256 *bytes* of `L_raw`, truncated at a UTF-8 char
        /// boundary. `None` when retention is opted out at config
        /// time; the miner always sets it today.
        triggering_line_sample: Option<String>,
        /// Which template state-change this is.
        change: TemplateChange,
    },
    /// A compaction consolidated a sealed partition's files
    /// (RFC 0009 §3.6). Carries no template identity.
    Compaction {
        /// The compacted data partition, as the canonical
        /// `year=…/month=…/day=…/hour=…` key under the event's
        /// `tenant_id` (RFC 0005 §3.4).
        partition: String,
        /// Input file names that were merged away.
        input_files: Vec<String>,
        /// The consolidated output file (the sole live file after
        /// the commit).
        output_file: String,
        /// Manifest generation the consolidation committed at
        /// (RFC 0009 §3.4).
        generation: u64,
        /// Rows in the consolidated file — equal to the total input
        /// rows, the conserved count (RFC0009.2).
        rows: u64,
    },
}

/// Stable on-disk `event_kind` ordinals (RFC 0005 §3.7 mapping).
/// This is the canonical source; the Parquet writer/reader import
/// these rather than redefining them.
pub const EVENT_KIND_TEMPLATE_WIDENED: u8 = 0;
/// See [`EVENT_KIND_TEMPLATE_WIDENED`].
pub const EVENT_KIND_TEMPLATE_TYPE_EXPANDED: u8 = 1;
/// See [`EVENT_KIND_TEMPLATE_WIDENED`].
pub const EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE: u8 = 2;
/// See [`EVENT_KIND_TEMPLATE_WIDENED`].
pub const EVENT_KIND_COMPACTION: u8 = 3;

/// Canonical `event_type` strings paired with the ordinals above
/// (RFC 0005 §3.7 / RFC 0001 §6.4 / RFC 0009 §3.6).
pub const EVENT_TYPE_TEMPLATE_WIDENED: &str = "template_widened";
/// See [`EVENT_TYPE_TEMPLATE_WIDENED`].
pub const EVENT_TYPE_TEMPLATE_TYPE_EXPANDED: &str = "template_type_expanded";
/// See [`EVENT_TYPE_TEMPLATE_WIDENED`].
pub const EVENT_TYPE_TEMPLATE_WIDENING_REJECTED_DEGENERATE: &str =
    "template_widening_rejected_degenerate";
/// See [`EVENT_TYPE_TEMPLATE_WIDENED`].
pub const EVENT_TYPE_COMPACTION: &str = "compaction";

impl AuditPayload {
    /// The stable `event_kind` ordinal for this payload (RFC 0005
    /// §3.7 dual-column storage) — derived from the type, never
    /// stored.
    #[must_use]
    pub fn event_kind(&self) -> u8 {
        match self {
            Self::Template { change, .. } => match change {
                TemplateChange::Widened { .. } => EVENT_KIND_TEMPLATE_WIDENED,
                TemplateChange::TypeExpanded { .. } => EVENT_KIND_TEMPLATE_TYPE_EXPANDED,
                TemplateChange::RejectedDegenerate { .. } => {
                    EVENT_KIND_TEMPLATE_WIDENING_REJECTED_DEGENERATE
                }
            },
            Self::Compaction { .. } => EVENT_KIND_COMPACTION,
        }
    }

    /// The canonical `event_type` string paired with
    /// [`Self::event_kind`].
    #[must_use]
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Template { change, .. } => match change {
                TemplateChange::Widened { .. } => EVENT_TYPE_TEMPLATE_WIDENED,
                TemplateChange::TypeExpanded { .. } => EVENT_TYPE_TEMPLATE_TYPE_EXPANDED,
                TemplateChange::RejectedDegenerate { .. } => {
                    EVENT_TYPE_TEMPLATE_WIDENING_REJECTED_DEGENERATE
                }
            },
            Self::Compaction { .. } => EVENT_TYPE_COMPACTION,
        }
    }

    /// `true` for events that count toward `merges_total` per
    /// RFC §6.4 — the two structural template widenings. Rejections
    /// and compactions are recorded but do not increment the counter.
    #[must_use]
    pub fn counts_as_merge(&self) -> bool {
        match self {
            Self::Template { change, .. } => change.counts_as_merge(),
            Self::Compaction { .. } => false,
        }
    }
}

impl TemplateChange {
    /// `true` for the two structural widenings (RFC §6.4
    /// `merges_total`); rejections do not count.
    #[must_use]
    pub fn counts_as_merge(&self) -> bool {
        matches!(self, Self::Widened { .. } | Self::TypeExpanded { .. })
    }
}

/// Type tag for a masked parameter slot.
///
/// Matches RFC 0001 §6.1's `ParamType` alphabet. Hosted in
/// `ourios-core` so both the audit-event schema (which references
/// it in [`SlotExpansion`]) and `ourios-miner`'s masking layer
/// (which emits it as the `type_tag` on every typed parameter)
/// share a single type. Not every variant has an emitter yet —
/// `Hex`, `Ts`, `Path`, and `Overflow` ship now so the schema is
/// stable across the §6.5 mask-rule expansions and the H2
/// overflow PR that will add their emitters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamType {
    /// IPv4 dotted-quad per the `IP` masking rule. The rendered
    /// tag is `<IP>`.
    Ip,
    /// UUID per the `UUID` masking rule.
    Uuid,
    /// Integer or float as recognised by the `NUM` masking rule.
    Num,
    /// Hex string (reserved; no emitter yet).
    Hex,
    /// Timestamp (reserved; no emitter yet).
    Ts,
    /// Filesystem or URL path (reserved; no emitter yet).
    Path,
    /// Free-text fallback. Used when a wildcard slot's contents
    /// are not matched by any mask rule. Includes the "literal at
    /// the position of a freshly-widened wildcard" payload per
    /// §6.2 step 5's "Build the params array" rule.
    Str,
    /// `params` value that exceeded the per-parameter byte limit
    /// per §6.5; the original value spills to the `body` column
    /// and the slot's payload becomes `{length, sha256_prefix}`.
    /// Reserved here; emitter lands with the overflow PR.
    Overflow,
    /// Reader-side catch-all per RFC 0005 §3.9: a
    /// `params.type_tag` ordinal that the current `ParamType`
    /// enum doesn't recognise (e.g. a value written by a future
    /// writer that added a new variant). Carries the raw ordinal
    /// so the writer can round-trip it on read-then-write, and
    /// so a future Rust upgrade that adds the missing variant
    /// can migrate `Unknown(N)` rows into the typed variant by
    /// matching the ordinal. RFC 0005 §6.6's reconstruction path
    /// treats `Unknown` as lossy (falls back to the body column).
    Unknown(i32),
}

/// Slot expansion entry carried on `TemplateTypeExpanded` events
/// per RFC §6.4.
///
/// The slot index is the wildcard-slot ordinal in the leaf's
/// template (counted left-to-right). `added_types` lists the
/// [`ParamType`]s now in the slot's type set that were not there
/// before this attach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotExpansion {
    pub slot_index: u16,
    pub added_types: Vec<ParamType>,
}

/// Compact bitset over the [`ParamType`] alphabet — one byte per
/// wildcard slot — recording every `ParamType` observed in that
/// slot per RFC 0001 §6.1 (`slot_types: Vec<HashSet<ParamType>>`).
///
/// The byte layout is private; the public surface is value-based
/// (`singleton`, `insert`, `contains`, `iter`, `is_empty`, `union`)
/// so the layout can change without breaking callers. A new
/// `ParamType` variant forces an update to [`SlotTypes::bit`] —
/// the compiler enforces totality via the exhaustive match. Eight
/// variants fit a `u8`; if the alphabet grows past eight, widen the
/// backing integer here (the same exhaustive match will fail to
/// compile and force the choice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SlotTypes(u8);

impl SlotTypes {
    /// Empty type set.
    #[must_use]
    pub const fn new() -> Self {
        Self(0)
    }

    /// Type set containing only `t`.
    #[must_use]
    pub const fn singleton(t: ParamType) -> Self {
        Self(Self::bit(t))
    }

    /// Returns a copy of `self` with `t` added. Idempotent — adding
    /// a type already present leaves the set unchanged.
    #[must_use]
    pub const fn insert(self, t: ParamType) -> Self {
        Self(self.0 | Self::bit(t))
    }

    /// `true` iff `t` is in the set.
    #[must_use]
    pub const fn contains(self, t: ParamType) -> bool {
        (self.0 & Self::bit(t)) != 0
    }

    /// `true` iff the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Set-union with `other`.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Iterates the variants in [`ParamType`] declaration order
    /// (`Ip, Uuid, Num, Hex, Ts, Path, Str, Overflow`). The order
    /// is stable — `SlotExpansion::added_types` and any other
    /// audit-event payload built by collecting this iterator
    /// inherits it, so two ingests that expand a slot with the
    /// same set of types produce the same wire-form audit.
    pub fn iter(self) -> impl Iterator<Item = ParamType> {
        const ALL: [ParamType; 8] = [
            ParamType::Ip,
            ParamType::Uuid,
            ParamType::Num,
            ParamType::Hex,
            ParamType::Ts,
            ParamType::Path,
            ParamType::Str,
            ParamType::Overflow,
        ];
        ALL.into_iter().filter(move |t| self.contains(*t))
    }

    const fn bit(t: ParamType) -> u8 {
        match t {
            ParamType::Ip => 1 << 0,
            ParamType::Uuid => 1 << 1,
            ParamType::Num => 1 << 2,
            ParamType::Hex => 1 << 3,
            ParamType::Ts => 1 << 4,
            ParamType::Path => 1 << 5,
            ParamType::Str => 1 << 6,
            ParamType::Overflow => 1 << 7,
            // `Unknown` is a reader-side catch-all for ordinals
            // a future writer added that the current enum
            // doesn't recognise; it has no bit position in the
            // `SlotTypes` bitset (the miner never produces
            // `Unknown` — it's purely an on-read shape). Map
            // to 0 so type-tracking is a no-op on the unknown
            // case; the writer round-trips the ordinal via
            // `param_type_ordinal` independently.
            ParamType::Unknown(_) => 0,
        }
    }
}

impl FromIterator<ParamType> for SlotTypes {
    fn from_iter<I: IntoIterator<Item = ParamType>>(iter: I) -> Self {
        iter.into_iter().fold(Self::new(), Self::insert)
    }
}

/// RFC 0001 §6.4 / RFC 0009 §3.6 audit-event schema.
///
/// Splits into the shared envelope (`tenant_id`, `timestamp`) and a
/// kind-specific [`payload`](AuditPayload). The payload owns every
/// field that is not common to all events — so a compaction event
/// carries no `template_id` by construction. `event_kind` /
/// `event_type` (RFC 0005 §3.7) are derived from the payload
/// ([`AuditPayload::event_kind`]), never stored on the struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub tenant_id: TenantId,
    pub timestamp: SystemTime,
    pub payload: AuditPayload,
}

/// Truncated blake3 of `bytes` for [`AuditEvent::triggering_line_hash`].
///
/// Returns the first 16 bytes of the blake3 digest. Centralised here
/// so every producer (and the future WAL-side joiner) uses the same
/// rule; if the truncation boundary ever changes it changes in one
/// place.
#[must_use]
pub fn hash_triggering_line(bytes: &[u8]) -> [u8; 16] {
    let h = blake3::hash(bytes);
    let mut out = [0_u8; 16];
    out.copy_from_slice(&h.as_bytes()[..16]);
    out
}

/// First 256 *bytes* of `raw`, truncated at the nearest preceding
/// UTF-8 char boundary so the result is always valid `String`.
///
/// Helper for populating [`AuditEvent::triggering_line_sample`] per
/// the RFC §6.4 "first 256 B of `L_raw`" rule. Bytes — not chars —
/// so the bound on the audit-stream's per-event size is predictable
/// regardless of the input's multibyte content.
#[must_use]
pub fn sample_first_256_bytes(raw: &str) -> String {
    let mut end = raw.len().min(256);
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    raw[..end].to_string()
}

/// Sink for audit events.
///
/// Producers call [`Self::emit`] once per state change. Durability
/// and ordering vs. data records are the sink's contract — see the
/// module-level note on the WAL impl's additional barriers.
///
/// The trait is `Send` so a `Box<dyn AuditSink>` can move across
/// threads with the cluster that owns it.
pub trait AuditSink: Send {
    /// Consume one event. Sinks own the event; producers must not
    /// retain references.
    fn emit(&mut self, event: AuditEvent);
}

/// Sink that drops every event it receives.
///
/// The production default until `ourios-wal` lands.
/// [`InMemoryAuditSink`] buffers events in an unbounded `Vec` —
/// fine for tests but a memory leak for any long-running
/// production miner since the buffer is not externally drainable
/// through the trait object. Defaulting to `NoOp` keeps
/// production safe; tests that need to *observe* events opt in
/// via [`SharedAuditSink`] through
/// `MinerCluster::with_audit_sink` in `ourios-miner`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpAuditSink;

impl NoOpAuditSink {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl AuditSink for NoOpAuditSink {
    fn emit(&mut self, _event: AuditEvent) {
        // Drop on the floor. The §6.4 audit contract is upheld
        // by the cluster's emit-site itself; this sink simply
        // declines to persist.
    }
}

/// `Vec`-backed sink for tests and the pre-WAL bootstrap.
///
/// Holds events in memory in emission order. Tests use
/// [`Self::drain`] (or, more commonly, the [`SharedAuditSink`]
/// wrapper) to inspect what was emitted. **Not** safe to use as
/// the production default — the buffer grows without bound and
/// is not externally drainable through a `Box<dyn AuditSink>`.
/// Use [`NoOpAuditSink`] for production until the WAL-backed
/// sink lands.
#[derive(Debug, Default)]
pub struct InMemoryAuditSink {
    events: Vec<AuditEvent>,
}

impl InMemoryAuditSink {
    #[must_use]
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Take ownership of every buffered event in emission order.
    /// The sink is empty afterwards; subsequent [`Self::emit`]
    /// calls accumulate fresh events.
    pub fn drain(&mut self) -> Vec<AuditEvent> {
        std::mem::take(&mut self.events)
    }
}

impl AuditSink for InMemoryAuditSink {
    fn emit(&mut self, event: AuditEvent) {
        self.events.push(event);
    }
}

/// [`InMemoryAuditSink`] wrapped in `Arc<Mutex<_>>` so a producer
/// can own the sink for emission while a test (or any observer)
/// still has a handle for inspection.
///
/// `Clone` yields another handle to the *same* buffer — that's the
/// whole point: hand one clone to [`MinerCluster::with_audit_sink`]
/// and keep another to drain after the act. `InMemoryAuditSink`
/// alone would require `&mut self` to drain, which the trait-object
/// indirection on the producer side rules out.
#[derive(Debug, Clone, Default)]
pub struct SharedAuditSink {
    inner: Arc<Mutex<InMemoryAuditSink>>,
}

impl SharedAuditSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take ownership of every buffered event in emission order.
    /// The shared buffer is empty afterwards.
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while holding the
    /// internal mutex. Sinks have no recovery story for a
    /// poisoned mutex — a poisoned audit buffer cannot be
    /// trusted to be complete or ordered.
    #[must_use]
    pub fn drain(&self) -> Vec<AuditEvent> {
        self.inner
            .lock()
            .expect("audit sink mutex poisoned")
            .drain()
    }

    /// # Panics
    ///
    /// As [`Self::drain`].
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("audit sink mutex poisoned").len()
    }

    /// # Panics
    ///
    /// As [`Self::drain`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("audit sink mutex poisoned")
            .is_empty()
    }
}

impl AuditSink for SharedAuditSink {
    fn emit(&mut self, event: AuditEvent) {
        self.inner
            .lock()
            .expect("audit sink mutex poisoned")
            .emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widened_event(tenant: &TenantId) -> AuditEvent {
        AuditEvent {
            tenant_id: tenant.clone(),
            timestamp: SystemTime::now(),
            payload: AuditPayload::Template {
                template_id: 1,
                triggering_line_hash: hash_triggering_line(b"user 42 logged out"),
                triggering_line_sample: Some("user 42 logged out".to_string()),
                change: TemplateChange::Widened {
                    old_version: 1,
                    new_version: 2,
                    old_template: "user 42 logged in".to_string(),
                    new_template: "user 42 logged <*>".to_string(),
                    positions_widened: vec![3],
                },
            },
        }
    }

    fn rejection_event(tenant: &TenantId) -> AuditEvent {
        AuditEvent {
            tenant_id: tenant.clone(),
            timestamp: SystemTime::now(),
            payload: AuditPayload::Template {
                template_id: 1,
                triggering_line_hash: hash_triggering_line(b"zzz qqq rrr"),
                triggering_line_sample: Some("zzz qqq rrr".to_string()),
                change: TemplateChange::RejectedDegenerate {
                    version: 2,
                    current_template: "alpha <*> <*>".to_string(),
                    would_be_template: "<*> <*> <*>".to_string(),
                    would_be_positions: vec![0],
                },
            },
        }
    }

    /// A compaction audit event for the new RFC 0009 §3.6 payload.
    fn compaction_event(tenant: &TenantId) -> AuditEvent {
        AuditEvent {
            tenant_id: tenant.clone(),
            timestamp: SystemTime::now(),
            payload: AuditPayload::Compaction {
                partition: "year=2026/month=04/day=02/hour=10".to_string(),
                input_files: vec!["a.parquet".to_string(), "b.parquet".to_string()],
                output_file: "c.parquet".to_string(),
                generation: 7,
                rows: 100,
            },
        }
    }

    #[test]
    fn in_memory_sink_records_emission_order() {
        let mut sink = InMemoryAuditSink::new();
        let t = TenantId::new("tenant-x");

        sink.emit(widened_event(&t));
        sink.emit(rejection_event(&t));

        assert_eq!(sink.len(), 2);
        let drained = sink.drain();
        assert_eq!(drained.len(), 2);
        assert!(matches!(
            drained[0].payload,
            AuditPayload::Template {
                change: TemplateChange::Widened { .. },
                ..
            },
        ));
        assert!(matches!(
            drained[1].payload,
            AuditPayload::Template {
                change: TemplateChange::RejectedDegenerate { .. },
                ..
            },
        ));
        assert!(sink.is_empty(), "drain leaves the sink empty");
    }

    #[test]
    fn shared_sink_clone_observes_same_buffer() {
        let producer_handle = SharedAuditSink::new();
        let observer_handle = producer_handle.clone();
        let t = TenantId::new("tenant-x");

        // Produce via one handle.
        let mut producer = producer_handle;
        producer.emit(widened_event(&t));

        // Observe via the other.
        assert_eq!(observer_handle.len(), 1);
        let drained = observer_handle.drain();
        assert_eq!(drained.len(), 1);
        assert!(matches!(
            drained[0].payload,
            AuditPayload::Template {
                change: TemplateChange::Widened { .. },
                ..
            },
        ));
        // The producer's view is also drained — same buffer.
        assert!(observer_handle.is_empty());
    }

    #[test]
    fn counts_as_merge_distinguishes_widenings_from_rejections() {
        let t = TenantId::new("tenant-x");
        assert!(widened_event(&t).payload.counts_as_merge());
        assert!(!rejection_event(&t).payload.counts_as_merge());
        assert!(
            !compaction_event(&t).payload.counts_as_merge(),
            "a compaction is not a merge"
        );
    }

    #[test]
    fn event_kind_and_type_map_per_rfc_0005_3_7() {
        // Arrange / Act / Assert — the derived dual-column mapping.
        let t = TenantId::new("tenant-x");
        assert_eq!(
            widened_event(&t).payload.event_kind(),
            EVENT_KIND_TEMPLATE_WIDENED
        );
        assert_eq!(
            widened_event(&t).payload.event_type(),
            EVENT_TYPE_TEMPLATE_WIDENED
        );
        assert_eq!(
            compaction_event(&t).payload.event_kind(),
            EVENT_KIND_COMPACTION
        );
        assert_eq!(
            compaction_event(&t).payload.event_type(),
            EVENT_TYPE_COMPACTION
        );
    }

    #[test]
    fn hash_triggering_line_is_deterministic_and_distinct_for_distinct_input() {
        let a = hash_triggering_line(b"user logged in");
        let b = hash_triggering_line(b"user logged in");
        let c = hash_triggering_line(b"user logged out");

        assert_eq!(a, b, "blake3 of the same input is stable");
        assert_ne!(a, c, "distinct inputs produce distinct prefixes");
    }

    #[test]
    fn sample_first_256_bytes_respects_char_boundary() {
        // A multibyte char straddling byte 256 must be excluded so
        // the result is valid UTF-8.
        //
        // Build a string of 255 ASCII bytes + one 2-byte char ('é').
        // The 2-byte char starts at byte 255 and ends at byte 257; a
        // naive `raw[..256]` would slice mid-char and panic. The
        // helper must instead stop at byte 255 (the boundary
        // preceding the multibyte char).
        let mut raw = "a".repeat(255);
        raw.push('é');
        assert_eq!(raw.len(), 257);

        let sample = sample_first_256_bytes(&raw);
        assert_eq!(
            sample.len(),
            255,
            "sample must back up to the preceding char boundary",
        );
        assert!(
            sample.chars().all(|c| c == 'a'),
            "sample contains only the pre-boundary ASCII chars",
        );
    }

    #[test]
    fn sample_first_256_bytes_returns_input_when_shorter_than_limit() {
        let short = "hello";
        assert_eq!(sample_first_256_bytes(short), short);
    }

    #[test]
    fn sample_first_256_bytes_returns_first_256_for_ascii_overflow() {
        let raw = "x".repeat(300);
        let sample = sample_first_256_bytes(&raw);
        assert_eq!(sample.len(), 256);
    }

    #[test]
    fn slot_types_default_and_new_are_empty() {
        assert!(SlotTypes::new().is_empty());
        assert!(SlotTypes::default().is_empty());
    }

    #[test]
    fn slot_types_singleton_contains_only_that_variant() {
        let s = SlotTypes::singleton(ParamType::Num);
        assert!(s.contains(ParamType::Num));
        assert!(!s.contains(ParamType::Uuid));
        assert!(!s.contains(ParamType::Str));
    }

    #[test]
    fn slot_types_insert_is_idempotent() {
        // The expansion logic re-inserts on every attach (cheap and
        // monotonic); idempotence pins the contract that a no-op
        // attach won't accidentally toggle bits.
        let s = SlotTypes::singleton(ParamType::Num);
        assert_eq!(s.insert(ParamType::Num), s);
    }

    #[test]
    fn slot_types_iter_yields_declared_order_regardless_of_insertion_order() {
        // ParamType declaration order: Ip, Uuid, Num, Hex, Ts, Path,
        // Str, Overflow. Pinned because TemplateTypeExpanded payloads
        // collect from this iterator — a reorder here would flip
        // the on-wire `added_types` ordering on the audit event.
        let s = SlotTypes::new()
            .insert(ParamType::Str)
            .insert(ParamType::Num)
            .insert(ParamType::Ip);
        let collected: Vec<_> = s.iter().collect();
        assert_eq!(
            collected,
            vec![ParamType::Ip, ParamType::Num, ParamType::Str],
        );
    }

    #[test]
    fn slot_types_from_iter_round_trips_through_iter() {
        let original: Vec<ParamType> = vec![ParamType::Num, ParamType::Uuid, ParamType::Str];
        let set: SlotTypes = original.iter().copied().collect();
        let collected: Vec<_> = set.iter().collect();
        // Declared order, not insertion order — see the iter test.
        assert_eq!(
            collected,
            vec![ParamType::Uuid, ParamType::Num, ParamType::Str],
        );
    }

    #[test]
    fn slot_types_union_is_set_union() {
        let a = SlotTypes::singleton(ParamType::Num).insert(ParamType::Uuid);
        let b = SlotTypes::singleton(ParamType::Ip).insert(ParamType::Num);
        let u = a.union(b);
        assert!(u.contains(ParamType::Num));
        assert!(u.contains(ParamType::Uuid));
        assert!(u.contains(ParamType::Ip));
        assert!(!u.contains(ParamType::Str));
    }

    #[test]
    fn slot_types_bit_assignments_are_mutually_distinct() {
        // Each ParamType must map to its own bit; otherwise insert/
        // contains would alias variants. Reflectively iterate every
        // variant (declared once in the const ALL inside iter()) by
        // round-tripping each through singleton + iter.
        for t in [
            ParamType::Ip,
            ParamType::Uuid,
            ParamType::Num,
            ParamType::Hex,
            ParamType::Ts,
            ParamType::Path,
            ParamType::Str,
            ParamType::Overflow,
        ] {
            let s = SlotTypes::singleton(t);
            let observed: Vec<_> = s.iter().collect();
            assert_eq!(
                observed,
                vec![t],
                "singleton({t:?}) must round-trip to a single-element iter",
            );
        }
    }
}
