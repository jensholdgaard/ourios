//! Per-tenant template cluster.
//!
//! Holds one [`TenantState`] per [`TenantId`] (`[CLAUDE.md §3.7]`):
//! every ingested line is keyed on its tenant, and per-tenant
//! template *stores* are isolated — no template ever crosses
//! tenants. The `template_id` allocator, by contrast, is
//! **cluster-wide** so the same `u64` value never refers to two
//! different leaves (RFC 0001 §6.1, §5 §3.7.2); each tenant
//! sees a monotonic *subsequence* of the shared id space.
//!
//! What this module is and is not (yet):
//!
//! - The per-tenant store is now the Drain prefix [`Tree`]
//!   (root → length-N node → prefix-token nodes → leaf list),
//!   the data structure introduced in RFC 0001 §6.2 step 3.
//!   The attach decision is **exact-match only**: a candidate
//!   line attaches to a leaf when `sim_seq(line, leaf.template)
//!   == 1.0`; anything else creates a new leaf in the same
//!   `(length, prefix)` bucket. RFC §6.2 step 5b widening (and
//!   the §3.1 invariant requiring an audit event for every
//!   merge) is the focus of the **next** PR — and because no
//!   widening happens here, no merge happens, and §3.1 is
//!   preserved vacuously.
//! - It does not emit audit events, telemetry, body retention,
//!   or `lossy_flag` — all those follow once widening lands.
//! - It does not write Parquet records — the on-disk shape is
//!   `ourios-parquet`'s problem.
//!
//! [`Tree`]: crate::tree::Tree

use std::collections::HashMap;

use ourios_core::config::MinerConfig;
use ourios_core::tenant::TenantId;

use crate::mask::mask;
use crate::sim_seq::sim_seq;
use crate::tokenize::tokenize;
use crate::tree::{DEFAULT_PREFIX_DEPTH, Leaf, OwnedToken, Tree};

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
/// helpers below. `tree` is the Drain prefix tree; under the
/// current exact-match-only attach policy every stored template
/// is a `Vec<OwnedToken::Fixed(_)>` (no [`OwnedToken::Wildcard`]
/// positions until widening lands).
///
/// `template_id` allocation lives on [`MinerCluster`], not here
/// — see the `next_template_id` comment there for why.
struct TenantState {
    tree: Tree,
}

impl TenantState {
    fn new() -> Self {
        Self { tree: Tree::new() }
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
            // Start at 1 so 0 stays available as a sentinel for
            // "no template" if a future caller wants one.
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

    /// Ingest a raw line for the named tenant. Returns the
    /// `template_id` allocated (or reused) for the line's
    /// masked shape.
    ///
    /// On first sight of `tenant_id`, allocates a fresh
    /// per-tenant store. Lines whose masked shape matches an
    /// existing leaf at `sim_seq == 1.0` reuse that leaf's
    /// `template_id`; everything else pulls the next monotonic
    /// id from the cluster-wide allocator and creates a new
    /// leaf under the same `(length, prefix)` bucket.
    pub fn ingest(&mut self, tenant_id: &TenantId, raw: &str) -> u64 {
        let tokenized = tokenize(raw);
        let masked = mask(&tokenized.tokens);
        let masked_strs: Vec<&str> = masked.tokens.into_iter().collect();

        // Phase 1 — read-only lookup. Avoids the mutable-borrow
        // chain through `tenants.entry()` + `descend_mut` so we
        // can early-return without committing a `template_id`
        // allocation. RFC §6.2 step 4: candidate selection.
        let exact_match_id = self
            .tenants
            .get(tenant_id)
            .and_then(|state| state.tree.descend(&masked_strs, DEFAULT_PREFIX_DEPTH))
            .and_then(|parent| {
                parent
                    .leaves
                    .iter()
                    .find(|leaf| matches_exactly(&masked_strs, &leaf.template))
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
            .entry(tenant_id.clone())
            .or_insert_with(TenantState::new);
        let parent = state.tree.descend_mut(&masked_strs, DEFAULT_PREFIX_DEPTH);
        let new_template: Vec<OwnedToken> = masked_strs
            .iter()
            .map(|s| OwnedToken::Fixed((*s).to_string()))
            .collect();
        parent.leaves.push(Leaf {
            template: new_template,
            template_id: new_id,
        });
        new_id
    }

    /// Number of distinct templates this tenant has accumulated.
    /// Returns 0 for a tenant the cluster has never seen.
    #[must_use]
    pub fn template_count(&self, tenant_id: &TenantId) -> usize {
        self.tenants
            .get(tenant_id)
            .map_or(0, |s| s.tree.leaf_count())
    }

    /// Snapshot of `(masked_template, template_id)` pairs for
    /// one tenant. Returns an empty vec for unseen tenants.
    ///
    /// Order is not guaranteed (`HashMap` iteration). Callers
    /// that need a stable order should sort.
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
/// template. Equivalent to `sim_seq(...) == 1.0` for fixed-only
/// templates, but materialises no intermediate `Vec<Token<'_>>`
/// — the hot-path cost of the [`OwnedToken::as_borrowed`] +
/// `collect()` route would be paid on *every* leaf in the
/// candidate set on every line. The next PR will switch this
/// helper to a `sim_seq`-via-iterator path once widening makes
/// the wildcard branch necessary.
fn matches_exactly(line: &[&str], template: &[OwnedToken]) -> bool {
    if line.len() != template.len() {
        return false;
    }
    // Sanity: this helper's correctness is the same as
    // sim_seq(line, template) == 1.0 (where wildcards count
    // as matches). Assert in debug to catch divergence early.
    debug_assert_eq!(
        line.iter().zip(template.iter()).all(|(s, t)| match t {
            OwnedToken::Fixed(stored) => *s == stored.as_str(),
            OwnedToken::Wildcard => true,
        }),
        {
            let view: Vec<crate::sim_seq::Token<'_>> =
                template.iter().map(OwnedToken::as_borrowed).collect();
            (sim_seq(line, &view) - 1.0).abs() < f32::EPSILON
        },
        "matches_exactly diverged from sim_seq == 1.0",
    );
    line.iter().zip(template.iter()).all(|(s, t)| match t {
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

    #[test]
    fn ingest_returns_same_template_id_for_repeat_shape() {
        // Arrange — two lines with the same masked shape.
        let mut cluster = MinerCluster::new(MinerConfig::default());
        let t = TenantId::new("tenant-x");

        // Act
        let id1 = cluster.ingest(&t, "user 42 logged in");
        let id2 = cluster.ingest(&t, "user 17 logged in");

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
        let id1 = cluster.ingest(&t, "user 42 logged in");
        let id2 = cluster.ingest(&t, "GET /home 200");

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
        let _ = cluster.ingest(&t, "hello world");

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
        // contract. When widening lands in the next PR these two
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
        let id_in = cluster.ingest(&t, "user 42 logged in from 10.0.0.1");
        let id_out = cluster.ingest(&t, "user 42 logged out from 10.0.0.1");

        // Assert — distinct templates today; same `(length,
        // prefix)` bucket but different leaves.
        assert_ne!(id_in, id_out);
        assert_eq!(cluster.template_count(&t), 2);
    }
}
