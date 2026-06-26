//! v1 reader-side template-registry derivation (RFC 0017 §3.2).
//!
//! There is **no persisted per-tenant template-registry artifact** in v1
//! (the cached map is the deferred RFC 0005 §3.7.1 / manifest-fork
//! optimisation): the audit stream *is* the registry, and the querier
//! derives the requesting tenant's `(template_id, template_version) →
//! tokens` map at query time by folding the tenant's `template_created` /
//! `template_widened` / `template_type_expanded` events (RFC 0001 §6.4,
//! RFC 0017 §3.1) off the RFC 0005 §3.7 audit Parquet stream.
//!
//! This mirrors [`crate::alias_store::derive_alias_map`] exactly — the same
//! shared [`crate::audit_scan`] walk and the same total, deterministic fold
//! order `(timestamp, file path lexicographic, within-file row index)` (RFC
//! 0005 §3.7.1). Keying by the `(template_id, version)` pair is what lets a
//! row stamped `template_version = N` render against the N-version tokens
//! rather than the latest (RFC 0017 §3.5): each version is its own key, so a
//! later widening never clobbers an earlier version's tokens.

use std::collections::HashMap;

use ourios_core::audit::{AuditEvent, AuditPayload, TEMPLATE_INITIAL_VERSION, TemplateChange};
use ourios_core::tenant::TenantId;
use ourios_miner::tree::{OwnedToken, parse_template};
use ourios_parquet::{AuditReader, Store};

use crate::{QueryError, audit_scan};

/// Read-time map from a leaf's `(template_id, template_version)` to the
/// canonical tokens of that version (RFC 0017 §3.2). The value is parsed
/// from the audit stream's stored template string via
/// [`ourios_miner::tree::parse_template`].
pub type TemplateRegistry = HashMap<(u64, u32), Vec<OwnedToken>>;

/// Fold `tenant`'s template registry from its audit stream in `store`
/// per RFC 0017 §3.2. A tenant with no audit files (or none carrying
/// template events) derives the empty registry.
///
/// Each `template_created` event keys at [`TEMPLATE_INITIAL_VERSION`] (the
/// variant omits the version — a leaf is always born at v1); each
/// `template_widened` / `template_type_expanded` event keys at its
/// `new_version`. `template_widening_rejected_degenerate` events carry no
/// version bump or new tokens, so they contribute nothing.
///
/// # Errors
///
/// [`QueryError::Storage`] if the audit subtree cannot be listed, an audit
/// file cannot be read, or a row claims a tenant other than the one whose
/// partition root it lives under (the RFC 0005 §3.9 row-vs-path backstop).
pub fn derive_template_registry(
    store: &Store,
    tenant: &TenantId,
) -> Result<TemplateRegistry, QueryError> {
    // Lexicographic key order from the shared listing + in-file row order from
    // the reader give the §3.7.1 tiebreak components; no window — the
    // registry folds the tenant's whole template history.
    let keys = audit_scan::audit_files(store, tenant, None)?;
    let mut events: Vec<AuditEvent> = Vec::new();
    for key in &keys {
        let bytes = store.get_blocking(key).map_err(|e| QueryError::Storage {
            detail: format!("audit file {key}: {e}"),
        })?;
        let read = AuditReader::open_bytes(bytes::Bytes::from(bytes))
            .and_then(AuditReader::read_all)
            .map_err(|e| QueryError::Storage {
                detail: format!("audit file {key}: {e}"),
            })?;
        for event in read {
            // Row-level tenant backstop (`CLAUDE.md` §3.7 / RFC 0005 §3.9
            // row-vs-path): the listing is scoped to the tenant's partition, so
            // a row claiming another tenant is a corrupt or foreign file —
            // fail loudly rather than fold (or silently drop) it.
            if event.tenant_id != *tenant {
                return Err(QueryError::Storage {
                    detail: format!(
                        "audit file {key} carries a row for tenant {} under tenant {}'s \
                         partition root",
                        event.tenant_id.as_str(),
                        tenant.as_str(),
                    ),
                });
            }
            if matches!(event.payload, AuditPayload::Template { .. }) {
                events.push(event);
            }
        }
    }
    Ok(fold_registry(events))
}

/// Fold template audit `events` into the registry (RFC 0017 §3.2) — the pure
/// core of [`derive_template_registry`], split out so the version-keying logic
/// is unit-testable without the audit-file I/O.
///
/// Sorts by timestamp first: the stable sort completes the §3.7.1 total order,
/// keeping same-timestamp events in their (file path, row index) input order.
/// Each event keys by its version — `template_created` at
/// [`TEMPLATE_INITIAL_VERSION`], widening / type-expansion at `new_version`,
/// rejections contribute nothing — so distinct versions never collide and a
/// later widening cannot clobber an earlier version's tokens.
fn fold_registry(mut events: Vec<AuditEvent>) -> TemplateRegistry {
    events.sort_by_key(|e| e.timestamp);

    let mut registry = TemplateRegistry::new();
    for event in events {
        let AuditPayload::Template {
            template_id,
            change,
            ..
        } = event.payload
        else {
            continue;
        };
        let (version, template) = match change {
            TemplateChange::Created { new_template } => (TEMPLATE_INITIAL_VERSION, new_template),
            TemplateChange::Widened {
                new_version,
                new_template,
                ..
            }
            | TemplateChange::TypeExpanded {
                new_version,
                new_template,
                ..
            } => (new_version, new_template),
            // A rejection bumps no version and changes no tokens.
            TemplateChange::RejectedDegenerate { .. } => continue,
        };
        registry.insert((template_id, version), parse_template(&template));
    }
    registry
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use ourios_core::audit::hash_triggering_line;

    use super::{
        AuditEvent, AuditPayload, OwnedToken, TEMPLATE_INITIAL_VERSION, TemplateChange, TenantId,
        fold_registry,
    };

    fn event(template_id: u64, secs: u64, change: TemplateChange) -> AuditEvent {
        AuditEvent {
            tenant_id: TenantId::new("t"),
            timestamp: UNIX_EPOCH + Duration::from_secs(secs),
            payload: AuditPayload::Template {
                template_id,
                triggering_line_hash: hash_triggering_line(b"line"),
                triggering_line_sample: None,
                change,
            },
        }
    }

    fn created(template_id: u64, secs: u64, new_template: &str) -> AuditEvent {
        event(
            template_id,
            secs,
            TemplateChange::Created {
                new_template: new_template.to_owned(),
            },
        )
    }

    fn widened(template_id: u64, secs: u64, new_version: u32, new_template: &str) -> AuditEvent {
        event(
            template_id,
            secs,
            TemplateChange::Widened {
                old_version: new_version - 1,
                new_version,
                old_template: "user <*>".to_owned(),
                new_template: new_template.to_owned(),
                positions_widened: vec![2],
            },
        )
    }

    fn fixed(s: &str) -> OwnedToken {
        OwnedToken::Fixed(s.to_owned())
    }

    #[test]
    fn created_keys_at_initial_version_widened_at_new_version() {
        let registry = fold_registry(vec![
            created(1, 10, "user <*>"),
            widened(1, 20, 2, "user <*> <*>"),
        ]);
        assert_eq!(
            registry.get(&(1, TEMPLATE_INITIAL_VERSION)),
            Some(&vec![fixed("user"), OwnedToken::Wildcard]),
        );
        assert_eq!(
            registry.get(&(1, 2)),
            Some(&vec![
                fixed("user"),
                OwnedToken::Wildcard,
                OwnedToken::Wildcard
            ]),
            "later version is a distinct key — v1 not clobbered",
        );
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn type_expanded_keys_at_new_version() {
        let registry = fold_registry(vec![event(
            5,
            10,
            TemplateChange::TypeExpanded {
                old_version: 1,
                new_version: 2,
                old_template: "GET <*>".to_owned(),
                new_template: "GET <*>".to_owned(),
                slots_expanded: Vec::new(),
            },
        )]);
        assert_eq!(
            registry.get(&(5, 2)),
            Some(&vec![fixed("GET"), OwnedToken::Wildcard]),
        );
    }

    #[test]
    fn rejection_contributes_nothing() {
        let registry = fold_registry(vec![event(
            1,
            10,
            TemplateChange::RejectedDegenerate {
                version: 2,
                current_template: "user <*> <*>".to_owned(),
                would_be_template: "<*> <*> <*>".to_owned(),
                would_be_positions: vec![0],
            },
        )]);
        assert!(registry.is_empty(), "a rejection adds no registry entry");
    }

    #[test]
    fn same_key_resolves_in_timestamp_order_last_wins() {
        // Two events for the same (id, version) — the stable sort by timestamp
        // makes the later one authoritative regardless of input order.
        let registry = fold_registry(vec![
            widened(7, 30, 2, "late <*>"),
            widened(7, 20, 2, "early <*>"),
        ]);
        assert_eq!(
            registry.get(&(7, 2)),
            Some(&vec![fixed("late"), OwnedToken::Wildcard]),
            "the later-timestamp event wins the (id, version) key",
        );
    }
}
