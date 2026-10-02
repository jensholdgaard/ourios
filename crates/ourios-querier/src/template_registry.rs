//! v1 reader-side template-registry derivation (RFC 0017 §3.2).
//!
//! The audit stream *is* the registry: the querier derives the requesting
//! tenant's `(template_id, template_version) → tokens` map at query time
//! by folding the tenant's `template_created` / `template_widened` /
//! `template_type_expanded` events (RFC 0001 §6.4, RFC 0017 §3.1) off the
//! RFC 0005 §3.7 audit Parquet stream. The RFC 0033 cached template map
//! (`template_map.v2.json.zst`, discharging the RFC 0005 §3.7.1 deferral)
//! accelerates this derivation at the query call sites; this fold remains
//! the source-of-truth derivation and the fallback for every non-hit
//! disposition.
//!
//! This mirrors [`crate::alias_store::derive_alias_map`] exactly — the same
//! shared [`crate::audit_scan`] walk and the same total, deterministic fold
//! order `(timestamp, file path lexicographic, within-file row index)` (RFC
//! 0005 §3.7.1). Keying by the `(template_id, version)` pair is what lets a
//! row stamped `template_version = N` render against the N-version tokens
//! rather than the latest (RFC 0017 §3.5): each version is its own key, so a
//! later widening never clobbers an earlier version's tokens.

use ourios_core::audit::{AuditEvent, AuditPayload, TEMPLATE_INITIAL_VERSION, TemplateChange};
use ourios_core::tenant::TenantId;
use ourios_miner::tree::{OwnedToken, parse_template};
use std::collections::HashMap;
use std::time::SystemTime;

use crate::{QueryError, StoreRef, audit_scan};

/// Read-time map from a leaf's `(template_id, template_version)` to the
/// canonical tokens of that version (RFC 0017 §3.2). The value is parsed
/// from the audit stream's stored template string via
/// [`ourios_miner::tree::parse_template`].
pub type TemplateRegistry = HashMap<(u64, u32), Vec<OwnedToken>>;

/// Fold `tenant`'s template registry from its audit stream per RFC 0017 §3.2.
/// A tenant with no audit files (or none carrying template events) derives the
/// empty registry.
///
/// `backend` selects the hybrid scan (RFC 0019 §3.3): [`StoreRef::Local`] reads
/// local audit files, [`StoreRef::Remote`] lists keys + reads bytes through the
/// S3 store.
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
    backend: StoreRef<'_>,
    tenant: &TenantId,
) -> Result<TemplateRegistry, QueryError> {
    derive_template_registry_measured(backend, tenant).map(|(registry, _bytes)| registry)
}

/// [`derive_template_registry`] plus the **bytes fetched** from the audit
/// stream deriving it — the registry component of a query's total IO
/// (RFC 0031 §3.6). The byte figure is the audit scan's whole read (the
/// stream is fetched before template events are filtered out), which is
/// exactly what answering the query cost.
pub(crate) fn derive_template_registry_measured(
    backend: StoreRef<'_>,
    tenant: &TenantId,
) -> Result<(TemplateRegistry, u64), QueryError> {
    // The shared reader gives the §3.7.1 file/row order and the row-level
    // tenant backstop; the fold keeps only the template events (no window —
    // the registry folds the tenant's whole template history).
    let mut fold = RegistryFold::default();
    let scan = audit_scan::for_each_event(backend, tenant, |event| fold.push(event))?;
    Ok((fold.finish(), scan.bytes_read))
}

/// Fold template audit `events`, given in SCAN order, into the registry
/// (RFC 0017 §3.2) — the pure core of [`derive_template_registry`], split
/// out so the version-keying logic is unit-testable without the audit-file
/// I/O. See [`RegistryFold`] for the order it completes.
#[cfg(test)]
pub(crate) fn fold_registry(events: Vec<AuditEvent>) -> TemplateRegistry {
    let mut fold = RegistryFold::default();
    for event in events {
        fold.push(event);
    }
    fold.finish()
}

/// The streaming registry fold (RFC 0017 §3.2): events arrive in SCAN order
/// — (file path, row index) — and only the per-key winner is retained, so a
/// fold over a tenant's whole audit history holds O(live `(template_id,
/// version)` keys), not O(events).
///
/// The winner per key is the event a stable timestamp sort would place
/// last: the greatest timestamp, ties going to the later scan position.
/// Replacing on `>=` while consuming in scan order is exactly that, so the
/// result equals sort-then-last-insert-wins over the full event list — the
/// §3.7.1 total order. Each event keys by its version — `template_created`
/// at [`TEMPLATE_INITIAL_VERSION`], widening / type-expansion at
/// `new_version`, adoption at its `template_version`; rejections and
/// non-template events contribute nothing.
#[derive(Default)]
pub(crate) struct RegistryFold {
    latest: HashMap<(u64, u32), (SystemTime, String)>,
}

impl RegistryFold {
    pub(crate) fn push(&mut self, event: AuditEvent) {
        let AuditPayload::Template {
            template_id,
            change,
            ..
        } = event.payload
        else {
            return;
        };
        let Some((version, template)) = keyed_template(change) else {
            return;
        };
        match self.latest.get_mut(&(template_id, version)) {
            Some(held) if event.timestamp < held.0 => {}
            Some(held) => *held = (event.timestamp, template),
            None => {
                self.latest
                    .insert((template_id, version), (event.timestamp, template));
            }
        }
    }

    pub(crate) fn finish(self) -> TemplateRegistry {
        self.latest
            .into_iter()
            .map(|(key, (_, template))| (key, parse_template(&template)))
            .collect()
    }
}

/// The `(version, template)` a template change binds, or `None` for a
/// rejection, which bumps no version and changes no tokens.
fn keyed_template(change: TemplateChange) -> Option<(u32, String)> {
    match change {
        TemplateChange::Created { new_template } => Some((TEMPLATE_INITIAL_VERSION, new_template)),
        TemplateChange::Widened {
            new_version,
            new_template,
            ..
        }
        | TemplateChange::TypeExpanded {
            new_version,
            new_template,
            ..
        } => Some((new_version, new_template)),
        // RFC 0050 §3.3 — adoption binds rows to
        // `(template_id, template_version)` exactly like a
        // creation; for an adoption riding an existing mined
        // leaf the pair is already interned and this insert is
        // an idempotent overwrite with the same tokens.
        TemplateChange::Adopted {
            template_version,
            new_template,
        } => Some((template_version, new_template)),
        TemplateChange::RejectedDegenerate { .. } => None,
    }
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
    fn adopted_keys_at_its_bound_version() {
        // RFC 0050 §3.3 — an adoption-interned template keys at v1;
        // an adoption riding an existing mined leaf keys at the
        // matched version, idempotently re-asserting the same
        // tokens the creation/widening already interned.
        let registry = fold_registry(vec![
            event(
                9,
                10,
                TemplateChange::Adopted {
                    template_version: 1,
                    new_template: "copy <*> done".to_owned(),
                },
            ),
            created(3, 20, "user <*>"),
            event(
                3,
                30,
                TemplateChange::Adopted {
                    template_version: 1,
                    new_template: "user <*>".to_owned(),
                },
            ),
        ]);
        assert_eq!(
            registry.get(&(9, 1)),
            Some(&vec![fixed("copy"), OwnedToken::Wildcard, fixed("done")]),
        );
        assert_eq!(
            registry.get(&(3, 1)),
            Some(&vec![fixed("user"), OwnedToken::Wildcard]),
            "tree-backed adoption re-asserts the mined tokens unchanged",
        );
        assert_eq!(registry.len(), 2);
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

    /// The §3.7.1 order as a materialized fold: stable sort by timestamp,
    /// then insert every event, last insert winning.
    fn sort_then_insert(mut events: Vec<AuditEvent>) -> super::TemplateRegistry {
        events.sort_by_key(|e| e.timestamp);
        let mut registry = super::TemplateRegistry::new();
        for event in events {
            if let AuditPayload::Template {
                template_id,
                change,
                ..
            } = event.payload
                && let Some((version, template)) = super::keyed_template(change)
            {
                registry.insert((template_id, version), super::parse_template(&template));
            }
        }
        registry
    }

    proptest::proptest! {
        /// The streaming fold equals sorting the whole history and
        /// inserting in order, same-timestamp ties included.
        #[test]
        fn streaming_fold_equals_sort_then_insert(
            history in proptest::collection::vec(
                (1u64..4, 0u64..4, 1u32..3, "[a-z]{1,3}"),
                0..40,
            ),
        ) {
            let events: Vec<AuditEvent> = history
                .into_iter()
                .map(|(id, secs, version, word)| {
                    widened(id, secs, version + 1, &format!("{word} <*>"))
                })
                .collect();
            proptest::prop_assert_eq!(fold_registry(events.clone()), sort_then_insert(events));
        }
    }
}
