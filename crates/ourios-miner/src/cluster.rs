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
//! What this module is and is not (yet):
//!
//! - Ingest now consumes [`OtlpLogRecord`] per RFC 0001 §6.1 (the
//!   pre-amendment `&str` signature is gone). Step 0 of §6.2 forks
//!   on `body.kind`: `String` records run the existing
//!   tokenize/mask/descend pipeline against the per-tenant prefix
//!   [`Tree`]; non-`String` (`Structured`) records short-circuit
//!   to a flat per-tenant `(severity_number, scope_name) → template_id`
//!   map keyed on the §6.1 *Template-key composition* tuple
//!   `(severity_number, scope_name, BodyKind::Structured)`.
//! - The attach decision for `String` records is **exact-match
//!   only**: a candidate line attaches to a leaf when
//!   `sim_seq(line, leaf.template) == 1.0` *and* the leaf's
//!   `(severity_number, scope_name)` equals the record's; anything
//!   else creates a new leaf in the same `(length, prefix)` bucket.
//!   RFC §6.2 step 5b widening (and the §3.1 invariant requiring
//!   an audit event for every merge) is the focus of a later PR —
//!   and because no widening happens here, no merge happens, and
//!   §3.1 is preserved vacuously.
//! - It does not emit audit events, telemetry, body retention,
//!   or `lossy_flag` — all those follow once widening lands.
//! - It does not write Parquet records — the on-disk shape is
//!   `ourios-parquet`'s problem.
//!
//! [`Tree`]: crate::tree::Tree

use std::collections::HashMap;

use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::tenant::TenantId;

use crate::mask::mask;
use crate::sim_seq::sim_seq;
use crate::tokenize::tokenize;
use crate::tree::{DEFAULT_PREFIX_DEPTH, Leaf, OwnedToken, Tree};

/// Sentinel `template_id` returned by [`MinerCluster::ingest`] when
/// no template was allocated for the input — currently both the
/// empty-input parse-failure path (`Body::String` whose tokenize
/// step yields zero tokens) and the absent-body case
/// (`record.body == None`). Real templates always have id `>= 1`
/// (see `next_template_id` initialisation).
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
}

/// Per-tenant template store.
///
/// Private: the cross-tenant API surface lives on
/// [`MinerCluster`]; per-tenant access goes through the cluster
/// helpers below. `tree` is the Drain prefix tree for the
/// `Body::String` branch; under the current exact-match-only
/// attach policy every stored template is a
/// `Vec<OwnedToken::Fixed(_)>` (no [`OwnedToken::Wildcard`]
/// positions until widening lands).
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
/// [`MinerCluster::ingest`]. It exists because the public
/// [`MinerCluster::template_count`] metric is intended for
/// frequent introspection (and will back the §6.8
/// `template_count` gauge once telemetry lands), and walking the
/// tree on every read would be O(N-leaves). The cache invariant
/// is: every fresh `template_id` allocation on this tenant
/// (whether tree leaf or structured map insert) increments the
/// cache by exactly one.
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
    /// Build an empty cluster. No tenant state allocated until
    /// the first `ingest` for a given tenant.
    #[must_use]
    pub fn new(config: MinerConfig) -> Self {
        Self {
            config,
            tenants: HashMap::new(),
            // Start at 1 so 0 stays available as the [`NO_TEMPLATE`]
            // sentinel.
            next_template_id: 1,
        }
    }

    /// Borrow the cluster's [`MinerConfig`].
    ///
    /// All tenants currently share one config; per-tenant
    /// overrides are a future PR.
    #[must_use]
    pub fn config(&self) -> &MinerConfig {
        &self.config
    }

    /// Ingest a structured OTLP log record. Returns the
    /// `template_id` allocated (or reused) for the record's
    /// §6.1 *Template-key composition* tuple, or [`NO_TEMPLATE`]
    /// (`0`) for the two parse-failure paths described below.
    ///
    /// The body fork follows RFC 0001 §6.2 step 0:
    ///
    /// - `Body::String(s)` — tokenize/mask/descend the prefix tree
    ///   per the existing pipeline. The leaf-list lookup compares
    ///   masked tokens **and** the record's
    ///   `(severity_number, scope_name)` — two records with the
    ///   same tokens but different severity or scope get distinct
    ///   `template_id`s (locks H1.4 / H1.5).
    /// - `Body::Structured(_)` — short-circuit. The `AnyValue`
    ///   tree is **not** walked; the template id is keyed on
    ///   `(severity_number, scope_name, BodyKind::Structured)`
    ///   per §6.1, and the same tuple reuses the same id on
    ///   subsequent records. RFC 0001 §6.1 also pins
    ///   `confidence = 1.0` and `lossy_flag = false` for this
    ///   branch (RFC0001.9) — those values are *derived by the
    ///   miner* (sentinel-constants for the structured branch),
    ///   not stored on `OtlpLogRecord`. They aren't on the
    ///   cluster's return surface yet because the Parquet writer
    ///   crate that consumes them doesn't exist; when it lands the
    ///   miner will emit them alongside the `template_id`. No
    ///   fields on `OtlpLogRecord` carry these values today.
    /// - `None` — the wire delivered no body. Returns
    ///   [`NO_TEMPLATE`]; no allocation. Whether absent body
    ///   should get its own sentinel template id is currently
    ///   undefined in §6.1 and is left for a follow-up: the
    ///   sentinel-return keeps the contract conservative and
    ///   never silently coalesces an absent body with any other
    ///   bucket.
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

    /// Existing tokenize → mask → descend pipeline, with the
    /// `(severity_number, scope_name)` discriminator extending
    /// the leaf-match check.
    fn ingest_string(&mut self, record: &OtlpLogRecord, raw: &str) -> u64 {
        let tokenized = tokenize(raw);
        let masked = mask(&tokenized.tokens);
        let masked_strs: Vec<&str> = masked.tokens.into_iter().collect();

        if masked_strs.is_empty() {
            return NO_TEMPLATE;
        }

        // Phase 1 — read-only lookup. Avoids the mutable-borrow
        // chain through `tenants.entry()` + `descend_mut` so we
        // can early-return without committing a `template_id`
        // allocation. RFC §6.2 step 4: candidate selection.
        let exact_match_id = self
            .tenants
            .get(&record.tenant_id)
            .and_then(|state| state.tree.descend(&masked_strs, DEFAULT_PREFIX_DEPTH))
            .and_then(|parent| {
                parent.leaves.iter().find(|leaf| {
                    matches_exactly(
                        &masked_strs,
                        leaf,
                        record.severity_number,
                        record.scope_name.as_deref(),
                    )
                })
            })
            .map(|leaf| leaf.template_id);

        if let Some(id) = exact_match_id {
            return id;
        }

        // Phase 2 — no exact match; allocate id, materialise the
        // path (creating any missing nodes), push the new leaf.
        // RFC §6.2 step 4: fresh-leaf creation when no candidate
        // matches.
        let new_id = self.next_template_id;
        self.next_template_id += 1;

        let state = self
            .tenants
            .entry(record.tenant_id.clone())
            .or_insert_with(TenantState::new);
        let parent = state.tree.descend_mut(&masked_strs, DEFAULT_PREFIX_DEPTH);
        let new_template: Vec<OwnedToken> = masked_strs
            .iter()
            .map(|s| OwnedToken::Fixed((*s).to_string()))
            .collect();
        parent.leaves.push(Leaf {
            template: new_template,
            template_id: new_id,
            severity_number: record.severity_number,
            scope_name: record.scope_name.clone(),
        });
        // Maintain the TenantState::template_count cache invariant —
        // every fresh allocation under `state` is mirrored here so
        // `MinerCluster::template_count` can stay O(1).
        state.template_count += 1;
        new_id
    }

    /// `Body::Structured` short-circuit per RFC 0001 §6.2 step 0.
    /// The tree is not walked; the per-tenant
    /// `(severity_number, scope_name) → template_id` map is the
    /// entire lookup. First observation of a tuple allocates;
    /// subsequent records with the same tuple reuse.
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
        // Same cache invariant as ingest_string: one fresh
        // allocation, one cache increment.
        state.template_count += 1;
        new_id
    }

    /// Number of distinct templates this tenant has accumulated
    /// (tree leaves + structured-template entries). Returns 0 for
    /// a tenant the cluster has never seen.
    ///
    /// O(1): served from the [`TenantState::template_count`]
    /// cache rather than walking the tree. The cache is
    /// maintained by [`MinerCluster::ingest`].
    #[must_use]
    pub fn template_count(&self, tenant_id: &TenantId) -> usize {
        self.tenants.get(tenant_id).map_or(0, |s| s.template_count)
    }

    /// Snapshot of `(masked_template, template_id)` pairs for
    /// one tenant's `Body::String` templates. Returns an empty
    /// vec for unseen tenants.
    ///
    /// Order is not guaranteed (`HashMap` iteration). Callers
    /// that need a stable order should sort. Structured-body
    /// templates (§6.2 step-0 short-circuit) are not returned by
    /// this helper — they have no token shape to surface — and
    /// will get a sibling `structured_templates_for` accessor when
    /// the operator-introspection API formalises.
    ///
    /// Under the current exact-match-only attach policy every
    /// stored template is fixed-token-only, so each template is
    /// returned as its `Vec<String>` shape. When widening lands
    /// and templates start containing wildcards, this signature
    /// will need to widen to `Vec<(Vec<OwnedToken>, u64)>` (a
    /// contract change worth surfacing in that PR — `<*>` as a
    /// `String` sentinel is the failure mode the
    /// [`crate::tree::OwnedToken`] type exists to prevent).
    #[must_use]
    pub fn templates_for(&self, tenant_id: &TenantId) -> Vec<(Vec<String>, u64)> {
        self.tenants.get(tenant_id).map_or_else(Vec::new, |s| {
            s.tree
                .collect_leaves()
                .into_iter()
                .map(|leaf| (owned_template_as_strings(&leaf.template), leaf.template_id))
                .collect()
        })
    }
}

/// Exact-match check between a freshly-masked line and a stored
/// leaf, including the `(severity_number, scope_name)` half of
/// the §6.1 template key. Equivalent to
/// `sim_seq(...) == 1.0 && leaf.severity_number == severity &&
/// leaf.scope_name.as_deref() == scope` for fixed-only templates,
/// but materialises no intermediate `Vec<Token<'_>>` — the
/// hot-path cost of the [`OwnedToken::as_borrowed`] +
/// `collect()` route would be paid on *every* leaf in the
/// candidate set on every line. The next PR will switch this
/// helper to a `sim_seq`-via-iterator path once widening makes
/// the wildcard branch necessary.
fn matches_exactly(line: &[&str], leaf: &Leaf, severity: u8, scope: Option<&str>) -> bool {
    if severity != leaf.severity_number {
        return false;
    }
    if scope != leaf.scope_name.as_deref() {
        return false;
    }
    if line.len() != leaf.template.len() {
        return false;
    }
    // Sanity: this helper's correctness on the token-shape arm is
    // the same as sim_seq(line, template) == 1.0 (where wildcards
    // count as matches). Assert in debug to catch divergence early.
    debug_assert_eq!(
        line.iter().zip(leaf.template.iter()).all(|(s, t)| match t {
            OwnedToken::Fixed(stored) => *s == stored.as_str(),
            OwnedToken::Wildcard => true,
        }),
        {
            let view: Vec<crate::sim_seq::Token<'_>> =
                leaf.template.iter().map(OwnedToken::as_borrowed).collect();
            (sim_seq(line, &view) - 1.0).abs() < f32::EPSILON
        },
        "matches_exactly diverged from sim_seq == 1.0",
    );
    line.iter().zip(leaf.template.iter()).all(|(s, t)| match t {
        OwnedToken::Fixed(stored) => *s == stored.as_str(),
        OwnedToken::Wildcard => true,
    })
}

fn owned_template_as_strings(template: &[OwnedToken]) -> Vec<String> {
    template
        .iter()
        .map(|t| match t {
            OwnedToken::Fixed(s) => s.clone(),
            // Under the current attach policy this branch is
            // unreachable (no widening => no wildcards), but the
            // function stays total. The "<*>" sentinel is acceptable
            // here only because the return type is a snapshot for
            // operator introspection, never fed back into the
            // tokenize→mask→ingest pipeline.
            OwnedToken::Wildcard => "<*>".to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
    /// the given severity and scope. The inner `AnyValue` carries
    /// an integer body; the §6.2 short-circuit ignores the inner
    /// value entirely (only the discriminator tuple matters for
    /// template-id allocation).
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

    // ---------- existing String-body behaviour preserved ----------

    #[test]
    fn ingest_returns_same_template_id_for_repeat_shape() {
        // Arrange — two lines with the same masked shape, same
        // (severity, scope).
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act
        let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let id2 = cluster.ingest(&string_record(&t, "user 17 logged in"));

        // Assert — <NUM> abstracts the user id, so both lines mask
        // to the same shape and `sim_seq == 1.0` on the existing
        // leaf, reusing its template_id.
        assert_eq!(id1, id2);
        assert_eq!(cluster.template_count(&t), 1);
    }

    #[test]
    fn ingest_returns_distinct_template_ids_for_distinct_shapes() {
        // Arrange
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act
        let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let id2 = cluster.ingest(&string_record(&t, "GET /home 200"));

        // Assert
        assert_ne!(id1, id2);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn template_count_is_zero_for_unseen_tenant() {
        // Arrange
        let cluster = MinerCluster::new(MinerConfig::default());
        let unseen = TenantId::new("never-ingested");

        // Act
        let n = cluster.template_count(&unseen);

        // Assert
        assert_eq!(n, 0);
        assert!(cluster.templates_for(&unseen).is_empty());
    }

    #[test]
    fn ingest_lazily_allocates_per_tenant_state() {
        // Arrange — a fresh cluster has no tenants.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");
        assert_eq!(cluster.template_count(&t), 0);

        // Act — first ingest must materialise the tenant state.
        let _ = cluster.ingest(&string_record(&t, "hello world"));

        // Assert
        assert_eq!(cluster.template_count(&t), 1);
    }

    #[test]
    fn ingest_creates_separate_leaves_for_near_match_under_same_parent() {
        // Arrange — two lines sharing length and prefix but
        // differing at a later position. Under the
        // exact-match-only attach policy (RFC §6.2 step 5 widening
        // not yet implemented), `sim_seq` between them is `5/6 ≈
        // 0.833` and falls below `1.0`, so they must produce
        // distinct template_ids.
        //
        // **Locking-test note:** this test pins the *no-widening*
        // contract. When widening lands in a later PR these two
        // lines will widen into a single template with `<*>` at
        // position 3, the assertions below will no longer hold,
        // and per `CLAUDE.md` §6.2 ("Tests are specifications") the
        // next PR's review must explicitly acknowledge the
        // contract change before this test is updated — it must
        // not be silently edited into compliance.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act — both lines mask to "user <NUM> logged in from <IP>"
        // / "user <NUM> logged out from <IP>" (length 6, same
        // prefix tokens at positions 0 and 1).
        let id_in = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));
        let id_out = cluster.ingest(&string_record(&t, "user 42 logged out from 10.0.0.1"));

        // Assert — distinct templates today; same `(length,
        // prefix)` bucket but different leaves.
        assert_ne!(id_in, id_out);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn ingest_returns_no_template_sentinel_for_empty_string_body() {
        // Arrange — `tokenize` documents that empty and
        // whitespace-only inputs produce zero tokens; `mask`
        // preserves length, so `Tree::descend{,_mut}` would see
        // an empty slice and trip its `N ≥ 1` precondition.
        // `ingest` short-circuits to the [`NO_TEMPLATE`] sentinel
        // instead of panicking — placeholder for the §6.8
        // `parse_failures_total` metric path.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act
        let id_empty = cluster.ingest(&string_record(&t, ""));
        let id_blank = cluster.ingest(&string_record(&t, "   \t\n"));

        // Assert — both inputs yield the sentinel; no template
        // is allocated; the tenant's count stays at 0.
        assert_eq!(id_empty, NO_TEMPLATE);
        assert_eq!(id_blank, NO_TEMPLATE);
        assert_eq!(cluster.template_count(&t), 0);
    }

    #[test]
    fn template_count_grows_with_each_distinct_template() {
        // Arrange — three lines: two share a masked shape, one
        // differs. The cache on TenantState must track exactly
        // the number of distinct templates the tree holds.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act — first ingest creates leaf 1, second hits the
        // existing leaf (no count change), third creates leaf 2.
        let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
        let _ = cluster.ingest(&string_record(&t, "user 17 logged in"));
        let _ = cluster.ingest(&string_record(&t, "GET /home 200"));

        // Assert — pin the cache invariant.
        let cached = cluster.template_count(&t);
        assert_eq!(cached, 2);
    }

    // ---------- new behaviour: body fork + structured short-circuit ----------

    #[test]
    fn ingest_returns_no_template_for_absent_body() {
        // Arrange — the wire delivered `LogRecord.body = None`.
        // §6.1 doesn't pin a behaviour for this; the conservative
        // choice is the [`NO_TEMPLATE`] sentinel (no allocation,
        // no silent coalescing into the structured bucket).
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");
        let r = OtlpLogRecord {
            tenant_id: t.clone(),
            body: None,
            ..Default::default()
        };

        // Act
        let id = cluster.ingest(&r);

        // Assert
        assert_eq!(id, NO_TEMPLATE);
        assert_eq!(cluster.template_count(&t), 0);
    }

    #[test]
    fn structured_body_short_circuit_allocates_one_template_per_severity_scope_tuple() {
        // Arrange — one tenant, three structured records with the
        // same (severity, scope). RFC 0001 §6.1 *Template-key
        // composition*: the structured branch shares a single
        // `template_id` per `(severity_number, scope_name,
        // BodyKind::Structured)` tuple, regardless of the inner
        // `AnyValue` shape. The §6.2 step-0 short-circuit means
        // the tree is not walked at all.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act
        let id1 = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id2 = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id3 = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));

        // Assert — same id, single allocation.
        assert_eq!(id1, id2);
        assert_eq!(id2, id3);
        assert_eq!(cluster.template_count(&t), 1);
    }

    #[test]
    fn structured_body_distinguishes_severity_within_one_scope() {
        // Arrange — two structured records, same scope, different
        // severity. Per §6.1 *Template-key composition* they belong
        // in different buckets and get distinct template_ids.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act
        let id_info = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id_error = cluster.ingest(&structured_record(&t, 17, Some("lib.auth")));

        // Assert
        assert_ne!(id_info, id_error);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn structured_body_distinguishes_scope_within_one_severity() {
        // Arrange — two structured records, same severity,
        // different scope. Per §6.1 *Template-key composition* they
        // belong in different buckets and get distinct
        // template_ids.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act
        let id_a = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id_b = cluster.ingest(&structured_record(&t, 9, Some("lib.payments")));

        // Assert
        assert_ne!(id_a, id_b);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn structured_body_with_scope_none_is_its_own_bucket() {
        // Arrange — a record with `scope_name = None` must NOT
        // share a template_id with any record carrying
        // `scope_name = Some(_)`, even at the same severity. This
        // pins the §6.1 RFC0001.11 edge-case rule for the
        // structured branch (the String-body arm of the same rule
        // is locked elsewhere).
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act
        let id_none = cluster.ingest(&structured_record(&t, 9, None));
        let id_some = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));

        // Assert
        assert_ne!(id_none, id_some);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn structured_body_isolates_template_ids_across_tenants() {
        // Arrange — two tenants emit identical structured records
        // (same severity, same scope). The §3.7 invariant
        // ("template trees never cross-pollinate") applies to the
        // structured store too, even though §3.7.2's exemplar
        // scenario in RFC 0001 §5 uses a String body. This test
        // is the structured-branch regression for that invariant:
        // the per-tenant `structured_templates` maps must be
        // independent, AND the cluster-wide id allocator must
        // hand the second tenant a distinct id rather than
        // reusing the first tenant's.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let a = TenantId::new("tenant-a");
        let b = TenantId::new("tenant-b");

        // Act — same (severity, scope) tuple, different tenants.
        let id_a = cluster.ingest(&structured_record(&a, 9, Some("lib.auth")));
        let id_b = cluster.ingest(&structured_record(&b, 9, Some("lib.auth")));

        // Assert — distinct ids (no cross-tenant id reuse), and
        // both tenants count exactly one template (no
        // cross-pollination of the structured store).
        assert_ne!(
            id_a, id_b,
            "structured records with identical key tuple must get distinct template_ids across tenants",
        );
        assert_eq!(cluster.template_count(&a), 1);
        assert_eq!(cluster.template_count(&b), 1);
    }

    #[test]
    fn structured_and_string_share_no_template_ids_at_same_severity_scope() {
        // Arrange — same tenant, same (severity, scope), one
        // structured body and one string body. They live in
        // separate stores per §6.1 (BodyKind::Structured is part
        // of the structured branch's template key); the cluster
        // allocator is shared, so the two ids are distinct
        // monotonically.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act
        let id_struct = cluster.ingest(&structured_record(&t, 9, Some("lib.auth")));
        let id_string = cluster.ingest(&OtlpLogRecord {
            tenant_id: t.clone(),
            severity_number: 9,
            scope_name: Some("lib.auth".to_string()),
            body: Some(Body::String("hello".to_string())),
            ..Default::default()
        });

        // Assert — distinct ids, both counted.
        assert_ne!(id_struct, id_string);
        assert_eq!(cluster.template_count(&t), 2);
    }

    #[test]
    fn string_body_distinguishes_severity_within_one_scope() {
        // Arrange — same masked shape, same scope, different
        // severity. The §6.1 *Template-key composition* tuple
        // includes severity_number, so the leaf-list lookup must
        // refuse to coalesce.
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

        // Act
        let id_info = cluster.ingest(&info);
        let id_error = cluster.ingest(&error);

        // Assert
        assert_ne!(id_info, id_error);
        assert_eq!(cluster.template_count(&t), 2);
    }
}
