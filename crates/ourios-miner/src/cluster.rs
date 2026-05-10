//! Per-tenant template cluster.
//!
//! Holds one [`TenantState`] per [`TenantId`] (`[CLAUDE.md §3.7]`):
//! every ingested line is keyed on its tenant, and per-tenant
//! state is intrinsically isolated — no shared template store,
//! no shared `template_id` allocator. RFC 0001 §6.1's
//! per-tenant monotonic `template_id` falls out of construction.
//!
//! What this module is NOT (yet):
//!
//! - It is **not** Drain. The per-tenant store is a
//!   [`HashMap`] keyed on the masked-token sequence: lines that
//!   produce structurally identical masked sequences share a
//!   `template_id`, lines that differ in any position get
//!   distinct ids. This is exact-match templating; future PRs
//!   replace the `HashMap` with `simSeq` + the depth-bounded
//!   tree + widening (RFC 0001 §6.2 steps 3–5). The §3.7
//!   isolation invariant is testable at this layer because
//!   isolation is about *who owns which store*, not about how
//!   the store clusters.
//! - It does not emit audit events, telemetry, body retention,
//!   or `lossy_flag` — all those follow once the tree exists.
//! - It does not write Parquet records — the on-disk shape is
//!   `ourios-parquet`'s problem.

use std::collections::HashMap;

use ourios_core::config::MinerConfig;
use ourios_core::tenant::TenantId;

use crate::mask::mask;
use crate::tokenize::tokenize;

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
/// helpers below. Future PRs will give this struct real Drain
/// machinery; the current `HashMap<Vec<String>, u64>` is the
/// simplest representation that satisfies §3.7's isolation
/// contract.
///
/// `template_id` allocation lives on [`MinerCluster`], not here
/// — see the `next_template_id` comment there for why.
struct TenantState {
    templates: HashMap<Vec<String>, u64>,
}

impl TenantState {
    fn new() -> Self {
        Self {
            templates: HashMap::new(),
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
    /// per-tenant store. Lines whose masked-token sequence has
    /// been seen before reuse the existing `template_id`; new
    /// shapes pull the next monotonic id from the cluster-wide
    /// allocator.
    pub fn ingest(&mut self, tenant_id: &TenantId, raw: &str) -> u64 {
        let tokenized = tokenize(raw);
        let masked = mask(&tokenized.tokens);
        let masked_owned: Vec<String> = masked.tokens.into_iter().map(String::from).collect();

        // Two-phase to keep the borrow checker happy: lookup
        // borrows self.tenants immutably for the early-return,
        // then the allocate-and-insert path borrows self twice
        // (next_template_id mutably, tenants mutably).
        if let Some(state) = self.tenants.get(tenant_id) {
            if let Some(&id) = state.templates.get(&masked_owned) {
                return id;
            }
        }

        let new_id = self.next_template_id;
        self.next_template_id += 1;
        let state = self
            .tenants
            .entry(tenant_id.clone())
            .or_insert_with(TenantState::new);
        state.templates.insert(masked_owned, new_id);
        new_id
    }

    /// Number of distinct templates this tenant has accumulated.
    /// Returns 0 for a tenant the cluster has never seen.
    #[must_use]
    pub fn template_count(&self, tenant_id: &TenantId) -> usize {
        self.tenants.get(tenant_id).map_or(0, |s| s.templates.len())
    }

    /// Snapshot of `(masked_template, template_id)` pairs for
    /// one tenant. Returns an empty vec for unseen tenants.
    ///
    /// Order is not guaranteed (`HashMap` iteration). Callers
    /// that need a stable order should sort.
    #[must_use]
    pub fn templates_for(&self, tenant_id: &TenantId) -> Vec<(Vec<String>, u64)> {
        self.tenants.get(tenant_id).map_or_else(Vec::new, |s| {
            s.templates.iter().map(|(t, id)| (t.clone(), *id)).collect()
        })
    }
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

        // Assert — exact-match templating: <NUM> abstracts the
        // user id, so both lines mask to the same shape and
        // share a template_id.
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
}
