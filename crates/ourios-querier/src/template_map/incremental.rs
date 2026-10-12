//! The guarded incremental fold (RFC 0033 §3.3 amendment 2026-10-12): a
//! stale artifact is extended by only the audit files listed since it was
//! folded, when that provably equals a full fold over the same listing,
//! and otherwise the derive falls back to a full fold.
//!
//! The guard rows, first match wins: no fold state, a folded file gone
//! from the listing, the depth cap, then a fold-relevant delta event at or
//! before the artifact's high-water. The last one is only known after the
//! delta is read, so the delta's seeded fold runs while it is checked and
//! is discarded on a rejection.

use std::time::{Duration, UNIX_EPOCH};

use ourios_core::tenant::TenantId;

use super::{DerivePath, Derived, FallbackReason, FoldPass, TemplateMap, add_bytes, full_fold};
use crate::fold_state::{FoldState, INCREMENTAL_DEPTH_CAP, is_fold_relevant, nanos};
use crate::template_registry::{RegistryFold, TemplateRegistry};
use crate::{QueryError, audit_scan::ResolvedAuditSet};

/// Derive `resolved`'s map from the stale artifact `base`: incrementally
/// when the guard admits it, else a full fold labelled with the guard row
/// that declined it.
pub(super) fn derive_stale(
    base: TemplateMap,
    resolved: ResolvedAuditSet<'_>,
    tenant: &TenantId,
) -> Result<Derived, QueryError> {
    let TemplateMap {
        folded_files,
        registry,
        aliases,
        fold_state,
        ..
    } = base;
    let Some(state) = fold_state else {
        return fallback(resolved, tenant, FallbackReason::NoFoldState, Fetched::NONE);
    };
    let Some(delta) = delta_over(&folded_files, resolved.frontier()) else {
        return fallback(
            resolved,
            tenant,
            FallbackReason::FilesRemoved,
            Fetched::NONE,
        );
    };
    if state.incremental_depth() >= INCREMENTAL_DEPTH_CAP {
        return fallback(resolved, tenant, FallbackReason::DepthCap, Fetched::NONE);
    }
    let delta_set = resolved.restricted_to(tenant, &delta);
    let base = Base {
        registry,
        aliases,
        state,
    };
    match fold_delta(base, delta_set, resolved.frontier().to_vec(), tenant)? {
        DeltaFold::Folded(derived) => Ok(*derived),
        DeltaFold::Rejected(fetched) => {
            fallback(resolved, tenant, FallbackReason::OutOfOrder, fetched)
        }
    }
}

/// The parts of the stale artifact an incremental fold extends.
struct Base {
    registry: TemplateRegistry,
    aliases: ourios_core::alias::AliasMap,
    state: FoldState,
}

/// Audit objects and bytes already fetched before a fallback.
#[derive(Clone, Copy)]
struct Fetched {
    files: u64,
    bytes: u64,
}

impl Fetched {
    const NONE: Self = Self { files: 0, bytes: 0 };
}

enum DeltaFold {
    Folded(Box<Derived>),
    /// A fold-relevant delta event at or before the high-water: what the
    /// rejected delta read cost.
    Rejected(Fetched),
}

/// The listed keys not in `folded`, or `None` when a folded key is no
/// longer listed. Both slices are sorted and unique.
fn delta_over(folded: &[String], live: &[String]) -> Option<Vec<String>> {
    let mut folded = folded.iter().peekable();
    let mut delta = Vec::new();
    for key in live {
        if folded.next_if(|f| *f == key).is_none() {
            delta.push(key.clone());
        }
        if folded.peek().is_some_and(|f| *f < key) {
            return None;
        }
    }
    folded.peek().is_none().then_some(delta)
}

/// Fold the delta onto the base, checking every fold-relevant delta event
/// against the base's high-water as it streams through the same validated
/// scan a full fold uses.
fn fold_delta(
    base: Base,
    delta_set: ResolvedAuditSet<'_>,
    live: Vec<String>,
    tenant: &TenantId,
) -> Result<DeltaFold, QueryError> {
    let high_water = base.state.high_water_ns();
    let held_at = UNIX_EPOCH + Duration::from_nanos(high_water.unwrap_or(0));
    let mut pass = FoldPass {
        registry: RegistryFold::seeded(&base.registry, held_at, base.state.binding_conflicts()),
        ..FoldPass::default()
    };
    let mut rejected = false;
    let scan = delta_set.for_each_event(tenant, |event| {
        let out_of_order =
            high_water.is_some_and(|hw| is_fold_relevant(&event) && nanos(event.timestamp) <= hw);
        rejected |= out_of_order;
        if !rejected {
            pass.visit(event);
        }
    })?;
    let fetched = Fetched {
        files: scan.frontier.len() as u64,
        bytes: scan.bytes_read,
    };
    if rejected {
        return Ok(DeltaFold::Rejected(fetched));
    }
    let depth = base.state.incremental_depth() + 1;
    let (map, conflicts) = pass.finish(tenant, live, base.aliases, high_water, depth);
    Ok(DeltaFold::Folded(Box::new(Derived {
        map,
        fetched_files: fetched.files,
        fetched_bytes: fetched.bytes,
        conflicts,
        path: DerivePath::Incremental,
    })))
}

/// A full fold over `resolved` after the guard declined, its cost
/// including whatever the rejected delta read `already` fetched.
fn fallback(
    resolved: ResolvedAuditSet<'_>,
    tenant: &TenantId,
    reason: FallbackReason,
    already: Fetched,
) -> Result<Derived, QueryError> {
    let mut derived = full_fold(resolved, tenant, DerivePath::Fallback(reason))?;
    derived.fetched_files = add_bytes(derived.fetched_files, already.files)?;
    derived.fetched_bytes = add_bytes(derived.fetched_bytes, already.bytes)?;
    Ok(derived)
}

#[cfg(test)]
mod tests {
    use super::delta_over;

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn delta_is_the_listed_keys_not_folded() {
        assert_eq!(
            delta_over(&keys(&["b", "d"]), &keys(&["a", "b", "c", "d", "e"])),
            Some(keys(&["a", "c", "e"])),
        );
        assert_eq!(delta_over(&[], &keys(&["a"])), Some(keys(&["a"])));
        assert_eq!(delta_over(&keys(&["a"]), &keys(&["a"])), Some(Vec::new()));
    }

    #[test]
    fn a_folded_key_missing_from_the_listing_is_no_delta() {
        assert_eq!(delta_over(&keys(&["a", "c"]), &keys(&["b", "c"])), None);
        assert_eq!(delta_over(&keys(&["c"]), &keys(&["a", "b"])), None);
        assert_eq!(delta_over(&keys(&["a", "z"]), &keys(&["a", "b"])), None);
    }
}
