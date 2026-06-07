//! Operator-driven, audited alias index (RFC 0001 §6.7).
//!
//! An **alias set** is a per-tenant `[§3.7]` equivalence class of
//! `template_id`s that an operator has asserted mean the same template.
//! Aliasing is **cross-leaf only**: it groups `template_id`s the miner
//! allocated as separate leaves. The cross-*version* axis (one leaf's
//! `template_id` is stable across widenings, only `template_version`
//! advances) is *not* an alias concern, so this module holds no
//! `template_version` field.
//!
//! Membership is the only thing that carries contract weight:
//! [`AliasMap::resolves`] expands by membership and nothing else
//! (RFC0001.13). The **canonical representative** of a class is *derived*
//! as `min(members)` — a stable, order-independent display/identity
//! convenience, **not** what defines membership; it re-derives whenever
//! membership changes. This rule is an evolvable implementation detail,
//! not a contract.
//!
//! # Source of truth vs. projection
//!
//! The durable [`AuditPayload::AliasAsserted`] /
//! [`AuditPayload::AliasRetracted`](crate::audit::AuditPayload) event log
//! (on the §6.4 audit stream, WAL-durable under the §3.4
//! WAL-before-ack barrier) is the source of truth. The [`AliasMap`] is an
//! **in-memory projection** of that log: it is foldable from the events
//! ([`AliasMap::apply`] / [`AliasMap::from_events`]), so a fresh process
//! reconstructs the same classes by replaying the stream. The
//! **physical on-disk map artifact** (its serialization format and the
//! snapshot/refresh cadence) is explicitly **out of scope here** — that
//! is the RFC 0005 storage split (sibling to issue #147). This module
//! adds no new on-disk write plane.
//!
//! The operator API ([`AliasMap::assert`] / [`AliasMap::retract`])
//! validates the request, emits the audited event through an injected
//! [`AuditSink`], and folds the same event into the in-memory classes —
//! so the projection an operator sees in-process matches what a later
//! replay of the log produces.

use std::collections::{BTreeSet, HashMap};
use std::time::SystemTime;

use opentelemetry::metrics::Counter;
use opentelemetry::{KeyValue, global};

use crate::audit::{AuditEvent, AuditPayload, AuditSink};
use crate::tenant::TenantId;

/// Maximum length of an alias assertion's `reason`, in bytes.
///
/// Mirrors the RFC §6.4 triggering-line-sample cap (256 B) so the
/// audit stream's per-event size stays bounded regardless of operator
/// input.
pub const REASON_BYTE_LIMIT: usize = 256;

/// `alias_assertions_total` (RFC 0001 §6.8 telemetry table). Named with
/// the pre-redesign identifier the table pins — the dotted-`ourios.*`
/// semconv conversion is the deferred §6.8 redesign, not this slice.
const METRIC_ALIAS_ASSERTIONS_TOTAL: &str = "alias_assertions_total";
/// `alias_retractions_total` (RFC 0001 §6.8 telemetry table).
const METRIC_ALIAS_RETRACTIONS_TOTAL: &str = "alias_retractions_total";
/// The `tenant_id` data-point attribute key. Pre-redesign name per the
/// §6.8 table (the namespaced `ourios.tenant` key is the deferred
/// dotted-semconv redesign).
const ATTR_TENANT_ID: &str = "tenant_id";

/// The operator / API principal that issued an alias assertion.
///
/// `[§3.1]` "explicit": aliasing is never anonymous, so every assertion
/// names its actor. This is purely the *identity* of the principal — the
/// authentication / authorization model is out of scope; an `ActorId`
/// is whatever id the control plane already authenticated.
///
/// Construction validates that the id is non-empty (an empty actor would
/// defeat the "never anonymous" contract); see [`ActorId::new`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActorId(String);

impl ActorId {
    /// Wrap a non-empty string as an `ActorId`.
    ///
    /// # Errors
    /// Returns [`AliasError::EmptyActor`] if `s` is empty.
    pub fn new(s: impl Into<String>) -> Result<Self, AliasError> {
        let s = s.into();
        if s.is_empty() {
            return Err(AliasError::EmptyActor);
        }
        Ok(Self(s))
    }

    /// Borrow the underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ActorId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Errors from the operator-driven alias API.
///
/// One variant per validated precondition; hand-rolled to match the
/// crate's existing error style (see [`MinerConfigError`](crate::config)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasError {
    /// An assertion's `actor` was empty — aliasing is never anonymous
    /// (`[§3.1]` "explicit").
    EmptyActor,
    /// An assertion's `reason` exceeded [`REASON_BYTE_LIMIT`] bytes.
    /// Carries the offending length for diagnostics.
    ReasonTooLong(usize),
    /// An [`AliasMap::assert`] named fewer than two distinct
    /// `template_id`s in its asserted set (`{representative_id} ∪
    /// member_ids`). A class of one id is not an alias set — that id
    /// resolves only to itself (RFC0001.16) — so a one-id assertion is
    /// a no-op the caller almost certainly did not intend; reject rather
    /// than silently emit a meaningless audit event.
    DegenerateAssertion,
}

impl std::fmt::Display for AliasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyActor => write!(
                f,
                "alias assertion actor must be non-empty (`[§3.1]`: aliasing is never anonymous)",
            ),
            Self::ReasonTooLong(len) => write!(
                f,
                "alias assertion reason is {len} bytes, exceeding the {REASON_BYTE_LIMIT}-byte \
                 limit (RFC 0001 §6.4)",
            ),
            Self::DegenerateAssertion => write!(
                f,
                "alias assertion must name at least two distinct template_ids in its asserted \
                 set {{representative_id}} ∪ member_ids (a one-id class is not an alias set, \
                 RFC0001.16)",
            ),
        }
    }
}

impl std::error::Error for AliasError {}

/// The operator context common to an alias assert / retract call: who
/// issued it, why, and when.
///
/// Bundling these keeps the [`AliasMap`] operator API to a small,
/// readable argument list and pins the "every alias action names an
/// actor" contract (`[§3.1]`) at the type level — an [`Operator`]
/// cannot exist without a validated [`ActorId`]. `reason` is validated
/// (≤ [`REASON_BYTE_LIMIT`]) at the call boundary, not here, so the
/// error surfaces from the operation that records it.
#[derive(Debug, Clone)]
pub struct Operator {
    /// The principal issuing the action — never anonymous (`[§3.1]`).
    pub actor: ActorId,
    /// Operator-supplied justification, ≤ [`REASON_BYTE_LIMIT`] bytes
    /// (validated by [`AliasMap::assert`] / [`AliasMap::retract`]).
    /// Empty string when none given.
    pub reason: String,
    /// When the action was issued — stamped on the audit event.
    pub timestamp: SystemTime,
}

impl Operator {
    /// An [`Operator`] context with an explicit `timestamp`.
    #[must_use]
    pub fn new(actor: ActorId, reason: impl Into<String>, timestamp: SystemTime) -> Self {
        Self {
            actor,
            reason: reason.into(),
            timestamp,
        }
    }

    /// An [`Operator`] context stamped at [`SystemTime::now`].
    #[must_use]
    pub fn now(actor: ActorId, reason: impl Into<String>) -> Self {
        Self::new(actor, reason, SystemTime::now())
    }
}

/// Per-tenant projection of the alias event log (RFC 0001 §6.7).
///
/// Holds each tenant's equivalence classes, folded from the durable
/// [`AuditPayload::AliasAsserted`] / [`AuditPayload::AliasRetracted`]
/// stream. The operator API ([`Self::assert`] / [`Self::retract`])
/// emits the audited event through an injected [`AuditSink`] *and*
/// folds it into the in-memory classes; [`Self::from_events`] /
/// [`Self::apply`] rebuild the same classes from a replayed log.
///
/// Classes are scoped strictly per [`TenantId`] `[§3.7]`: an assertion
/// in one tenant never affects another.
#[derive(Debug)]
pub struct AliasMap {
    /// Per-tenant set of equivalence classes. Each inner `BTreeSet` is
    /// one class of ≥ 2 members (singletons are not stored — an id in
    /// no class resolves to `{id}` by [`Self::resolves`]).
    classes: HashMap<TenantId, Vec<BTreeSet<u64>>>,
    assertions_total: Counter<u64>,
    retractions_total: Counter<u64>,
}

impl AliasMap {
    /// Build an empty map whose counters resolve through the process-
    /// global meter (RFC 0001 §6.8 API/SDK split: a no-op when no
    /// provider is installed, so constructing and recording is always
    /// safe).
    ///
    /// `alias_assertions_total` / `alias_retractions_total` carry a
    /// `tenant_id` attribute, so a per-tenant point appears on the first
    /// real assertion / retraction (§6.8 collect-on-read) — they are not
    /// zero-seeded, which would emit a spurious attribute-less series.
    #[must_use]
    pub fn new() -> Self {
        let meter = global::meter("ourios.miner");
        let assertions_total = meter
            .u64_counter(METRIC_ALIAS_ASSERTIONS_TOTAL)
            .with_unit("{assertion}")
            .build();
        let retractions_total = meter
            .u64_counter(METRIC_ALIAS_RETRACTIONS_TOTAL)
            .with_unit("{retraction}")
            .build();
        Self {
            classes: HashMap::new(),
            assertions_total,
            retractions_total,
        }
    }

    /// Assert that the union `{representative_id} ∪ member_ids` is one
    /// equivalence class under `tenant`.
    ///
    /// Validates (`by.reason` ≤ [`REASON_BYTE_LIMIT`]; the `by.actor`
    /// non-empty contract is already a type guarantee of [`ActorId`],
    /// ≥ 2 distinct ids), emits an [`AuditPayload::AliasAsserted`]
    /// event through `sink`, folds the asserted set into the tenant's
    /// classes (union-on-overlap: any pre-existing class sharing a
    /// member merges in), and increments `alias_assertions_total`.
    /// `representative_id` is the operator's anchor id only — membership
    /// is the union, independent of which id was named the anchor.
    ///
    /// # Errors
    /// - [`AliasError::ReasonTooLong`] if `by.reason` exceeds the limit.
    /// - [`AliasError::DegenerateAssertion`] if the asserted set has
    ///   fewer than two distinct ids.
    pub fn assert(
        &mut self,
        sink: &mut dyn AuditSink,
        tenant: &TenantId,
        representative_id: u64,
        member_ids: Vec<u64>,
        by: Operator,
    ) -> Result<(), AliasError> {
        validate_reason(&by.reason)?;

        let asserted: BTreeSet<u64> = std::iter::once(representative_id)
            .chain(member_ids.iter().copied())
            .collect();
        if asserted.len() < 2 {
            return Err(AliasError::DegenerateAssertion);
        }

        let event = AuditEvent {
            tenant_id: tenant.clone(),
            timestamp: by.timestamp,
            payload: AuditPayload::AliasAsserted {
                representative_id,
                member_ids,
                actor: by.actor,
                reason: by.reason,
            },
        };
        sink.emit(event);

        self.union_in(tenant, &asserted);
        self.assertions_total.add(
            1,
            &[KeyValue::new(ATTR_TENANT_ID, tenant.as_str().to_owned())],
        );
        Ok(())
    }

    /// Retract `id` from its alias class under `tenant`.
    ///
    /// Representative-independent: `id` may be any member, including the
    /// derived canonical. Emits an [`AuditPayload::AliasRetracted`]
    /// event through `sink` (with `representative_id = id` as the
    /// operator's anchor and empty `member_ids`), removes `id` from its
    /// class, and increments `alias_retractions_total`. A class that
    /// drops to a single member is no longer an alias set and is
    /// dropped — that lone id then resolves only to itself
    /// (RFC0001.16). The canonical re-derives as `min` of the remainder
    /// on the next [`Self::resolves`].
    ///
    /// Retracting an id that is in no class is a valid no-op on the
    /// projection (the id already resolves to itself), but it is still
    /// audited — un-aliasing is explicit and recorded either way
    /// (`[§3.1]`).
    ///
    /// # Errors
    /// - [`AliasError::ReasonTooLong`] if `by.reason` exceeds the limit.
    pub fn retract(
        &mut self,
        sink: &mut dyn AuditSink,
        tenant: &TenantId,
        id: u64,
        by: Operator,
    ) -> Result<(), AliasError> {
        validate_reason(&by.reason)?;

        let event = AuditEvent {
            tenant_id: tenant.clone(),
            timestamp: by.timestamp,
            payload: AuditPayload::AliasRetracted {
                representative_id: id,
                member_ids: Vec::new(),
                actor: by.actor,
                reason: by.reason,
            },
        };
        sink.emit(event);

        self.remove_id(tenant, id);
        self.retractions_total.add(
            1,
            &[KeyValue::new(ATTR_TENANT_ID, tenant.as_str().to_owned())],
        );
        Ok(())
    }

    /// The equivalence class containing `id` under `tenant`.
    ///
    /// Returns the whole class (representative and every member —
    /// expansion is by the set, not the assertion direction,
    /// RFC0001.13). An `id` in no class resolves to the singleton
    /// `{id}` (RFC0001.16), identical to bare `template_id = id`.
    #[must_use]
    pub fn resolves(&self, tenant: &TenantId, id: u64) -> BTreeSet<u64> {
        self.classes
            .get(tenant)
            .and_then(|classes| classes.iter().find(|c| c.contains(&id)))
            .cloned()
            .unwrap_or_else(|| std::iter::once(id).collect())
    }

    /// The derived canonical representative — `min(members)` — of the
    /// class containing `id` under `tenant`, or `id` itself when `id` is
    /// in no class.
    ///
    /// A display/identity convenience, **not** what defines membership
    /// (RFC 0001 §6.7); it re-derives whenever membership changes.
    #[must_use]
    pub fn canonical(&self, tenant: &TenantId, id: u64) -> u64 {
        self.classes
            .get(tenant)
            .and_then(|classes| classes.iter().find(|c| c.contains(&id)))
            .and_then(|c| c.first().copied())
            .unwrap_or(id)
    }

    /// Fold one durable alias event into the projection.
    ///
    /// The replay path: applying every [`AuditPayload::AliasAsserted`] /
    /// [`AuditPayload::AliasRetracted`] event in log order reconstructs
    /// the same classes the operator API produced live. Non-alias
    /// payloads ([`AuditPayload::Template`] /
    /// [`AuditPayload::Compaction`]) are ignored — the alias projection
    /// folds only its own two event kinds off the shared §6.4 stream.
    pub fn apply(&mut self, event: &AuditEvent) {
        match &event.payload {
            AuditPayload::AliasAsserted {
                representative_id,
                member_ids,
                ..
            } => {
                let asserted: BTreeSet<u64> = std::iter::once(*representative_id)
                    .chain(member_ids.iter().copied())
                    .collect();
                // A degenerate (< 2 ids) asserted set folds to nothing —
                // the live API rejects it, but a replay must tolerate any
                // log content without panicking.
                if asserted.len() >= 2 {
                    self.union_in(&event.tenant_id, &asserted);
                }
            }
            AuditPayload::AliasRetracted {
                representative_id,
                member_ids,
                ..
            } => {
                for id in std::iter::once(*representative_id).chain(member_ids.iter().copied()) {
                    self.remove_id(&event.tenant_id, id);
                }
            }
            AuditPayload::Template { .. } | AuditPayload::Compaction { .. } => {}
        }
    }

    /// Rebuild a projection by folding `events` in log order.
    ///
    /// The counters resolve through the global meter as in [`Self::new`]
    /// but a pure replay does **not** re-increment them — the counts
    /// belong to the live operator actions, not to a projection rebuild.
    #[must_use]
    pub fn from_events<'a, I>(events: I) -> Self
    where
        I: IntoIterator<Item = &'a AuditEvent>,
    {
        let mut map = Self::new();
        for event in events {
            map.apply(event);
        }
        map
    }

    /// Union `asserted` into `tenant`'s classes, merging every existing
    /// class that shares a member (union-on-overlap, order-independent).
    fn union_in(&mut self, tenant: &TenantId, asserted: &BTreeSet<u64>) {
        let classes = self.classes.entry(tenant.clone()).or_default();
        let mut merged = asserted.clone();
        // Drain out every class overlapping the (growing) `merged` set,
        // absorbing each into it; repeat until a pass finds no overlap,
        // since absorbing one class can bring in ids that now overlap a
        // class an earlier pass skipped.
        loop {
            let mut absorbed_any = false;
            let mut i = 0;
            while i < classes.len() {
                if classes[i].iter().any(|id| merged.contains(id)) {
                    let overlapping = classes.swap_remove(i);
                    merged.extend(overlapping);
                    absorbed_any = true;
                } else {
                    i += 1;
                }
            }
            if !absorbed_any {
                break;
            }
        }
        classes.push(merged);
    }

    /// Remove `id` from `tenant`'s classes, dropping any class that
    /// falls below two members (no longer an alias set) and any
    /// tenant entry that empties out.
    fn remove_id(&mut self, tenant: &TenantId, id: u64) {
        let Some(classes) = self.classes.get_mut(tenant) else {
            return;
        };
        for class in &mut *classes {
            class.remove(&id);
        }
        classes.retain(|c| c.len() >= 2);
        if classes.is_empty() {
            self.classes.remove(tenant);
        }
    }
}

impl Default for AliasMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Reject a `reason` longer than [`REASON_BYTE_LIMIT`] bytes.
fn validate_reason(reason: &str) -> Result<(), AliasError> {
    if reason.len() > REASON_BYTE_LIMIT {
        return Err(AliasError::ReasonTooLong(reason.len()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::InMemoryAuditSink;

    fn actor() -> ActorId {
        ActorId::new("op-alice").expect("non-empty actor")
    }

    fn op() -> Operator {
        Operator::now(actor(), "")
    }

    #[test]
    fn actor_id_rejects_empty() {
        assert_eq!(ActorId::new(""), Err(AliasError::EmptyActor));
        assert!(ActorId::new("op").is_ok());
    }

    #[test]
    fn assert_rejects_over_limit_reason() {
        // Arrange.
        let mut map = AliasMap::new();
        let mut sink = InMemoryAuditSink::new();
        let t = TenantId::new("t");
        let reason = "x".repeat(REASON_BYTE_LIMIT + 1);

        // Act.
        let result = map.assert(&mut sink, &t, 1, vec![2], Operator::now(actor(), reason));

        // Assert — rejected before any event is emitted.
        assert_eq!(
            result,
            Err(AliasError::ReasonTooLong(REASON_BYTE_LIMIT + 1))
        );
        assert!(sink.is_empty(), "no event emitted on a rejected assertion");
    }

    #[test]
    fn assert_rejects_single_id_set() {
        let mut map = AliasMap::new();
        let mut sink = InMemoryAuditSink::new();
        let t = TenantId::new("t");

        // representative == member → asserted set is {7}, one id.
        let result = map.assert(&mut sink, &t, 7, vec![7], op());
        assert_eq!(result, Err(AliasError::DegenerateAssertion));
        assert!(sink.is_empty());
    }

    #[test]
    fn union_on_overlap_merges_classes() {
        // Arrange — two assertions sharing member B merge into one class.
        let mut map = AliasMap::new();
        let mut sink = InMemoryAuditSink::new();
        let t = TenantId::new("t");

        // Act — {A,B} then {B,C}; overlap on B.
        map.assert(&mut sink, &t, 1, vec![2], op()).unwrap();
        map.assert(&mut sink, &t, 2, vec![3], op()).unwrap();

        // Assert — one class {1,2,3}; every member resolves to it.
        let expected: BTreeSet<u64> = [1, 2, 3].into_iter().collect();
        assert_eq!(map.resolves(&t, 1), expected);
        assert_eq!(map.resolves(&t, 3), expected);
        assert_eq!(map.canonical(&t, 3), 1, "canonical is min(members)");
    }

    #[test]
    fn from_events_reconstructs_the_live_projection() {
        // Arrange — drive the live API, capture the event log.
        let mut live = AliasMap::new();
        let mut sink = InMemoryAuditSink::new();
        let t = TenantId::new("t");
        live.assert(&mut sink, &t, 1, vec![2, 3], op()).unwrap();
        live.retract(&mut sink, &t, 2, op()).unwrap();
        let log = sink.drain();

        // Act — replay the log into a fresh projection.
        let replayed = AliasMap::from_events(&log);

        // Assert — replay matches the live projection.
        assert_eq!(replayed.resolves(&t, 1), live.resolves(&t, 1));
        assert_eq!(replayed.resolves(&t, 1), [1, 3].into_iter().collect());
        assert_eq!(replayed.resolves(&t, 2), [2].into_iter().collect());
    }

    // RFC 0001 §6.8 telemetry table: `alias_assertions_total` /
    // `alias_retractions_total` are mandatory counters with a
    // `tenant_id` attribute. Collect the exported metric stream through
    // an in-memory reader (no OTLP endpoint) and assert both surface,
    // exactly as the compaction-metrics test does. `init_in_memory`
    // installs the *global* provider, so this is a single-provider test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn alias_counters_are_exported_with_tenant_attribute() {
        use opentelemetry_sdk::metrics::data::{
            AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
        };

        // Arrange — in-memory provider, then the map (so its counters
        // resolve against it).
        let (guard, exporter) = ourios_telemetry::init_in_memory("ourios-test");
        let mut map = AliasMap::new();
        let mut sink = InMemoryAuditSink::new();
        let t = TenantId::new("acme");

        // Act — one assertion, one retraction.
        map.assert(&mut sink, &t, 1, vec![2], op()).unwrap();
        map.retract(&mut sink, &t, 2, op()).unwrap();
        guard.force_flush().expect("force_flush succeeds");

        // Assert — both counters are in the exported stream, and each
        // carries a datapoint with the tenant_id attribute.
        let rms = exporter.get_finished_metrics().expect("metrics exported");
        let names: Vec<String> = rms
            .iter()
            .flat_map(ResourceMetrics::scope_metrics)
            .flat_map(ScopeMetrics::metrics)
            .map(|m| m.name().to_string())
            .collect();
        for expected in [
            METRIC_ALIAS_ASSERTIONS_TOTAL,
            METRIC_ALIAS_RETRACTIONS_TOTAL,
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "exported stream missing {expected}, got {names:?}",
            );
        }

        let tenant_attr_present = |name: &str| {
            let data = rms
                .iter()
                .flat_map(ResourceMetrics::scope_metrics)
                .flat_map(ScopeMetrics::metrics)
                .find(|m| m.name() == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .data();
            let AggregatedMetrics::U64(MetricData::Sum(sum)) = data else {
                panic!("{name} should be a u64 sum");
            };
            sum.data_points().any(|dp| {
                dp.attributes()
                    .any(|kv| kv.key.as_str() == ATTR_TENANT_ID && kv.value.as_str() == "acme")
            })
        };
        assert!(
            tenant_attr_present(METRIC_ALIAS_ASSERTIONS_TOTAL),
            "alias_assertions_total must carry the tenant_id attribute",
        );
        assert!(
            tenant_attr_present(METRIC_ALIAS_RETRACTIONS_TOTAL),
            "alias_retractions_total must carry the tenant_id attribute",
        );
    }
}
