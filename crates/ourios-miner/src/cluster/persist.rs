//! The cluster's read/snapshot/restore surface: the per-tenant read
//! API, snapshot flattening, and validated restore. Moved verbatim
//! from the flat `cluster.rs` (epic #745 wave 2).

// The parent scope IS this module's import surface: the split was
// mechanical code motion (epic #745 wave 2), and gluing back through
// `super` keeps every pre-split path resolving unchanged.
#[allow(clippy::wildcard_imports)]
use super::*;

impl MinerCluster {
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
                    provenance: leaf.provenance,
                    upstream_associations: leaf
                        .upstream_associations
                        .strings()
                        .map(str::to_string)
                        .collect(),
                    upstream_association_overflow: leaf.upstream_associations.overflow(),
                })
                .collect()
        })
    }

    /// One tenant's adopted-template map (RFC 0050 §3.3), sorted by
    /// the full map key — `(canonical, severity, scope)` — for
    /// determinism. Both entry kinds appear: tree-backed rows carry
    /// no provenance of their own (it lives on the leaf, visible
    /// via [`Self::templates_for`]).
    #[must_use]
    pub fn adopted_templates_for(&self, tenant_id: &TenantId) -> Vec<AdoptedSnapshot> {
        self.tenants.get(tenant_id).map_or_else(Vec::new, |s| {
            let mut out: Vec<AdoptedSnapshot> = s
                .adopted_templates
                .iter()
                .map(|((canonical, severity, scope), entry)| {
                    let (template_id, template_version, owned, provenance, associations, overflow) =
                        match entry {
                            AdoptedEntry::TreeBacked {
                                template_id,
                                template_version,
                            } => (*template_id, *template_version, false, None, Vec::new(), 0),
                            AdoptedEntry::Owned(o) => (
                                o.template_id,
                                1,
                                true,
                                Some(o.provenance),
                                o.associations.strings().map(str::to_string).collect(),
                                o.associations.overflow(),
                            ),
                        };
                    AdoptedSnapshot {
                        canonical: canonical.clone(),
                        severity_number: *severity,
                        scope_name: scope.clone(),
                        template_id,
                        template_version,
                        owned,
                        provenance,
                        upstream_associations: associations,
                        upstream_association_overflow: overflow,
                    }
                })
                .collect();
            out.sort_by(|a, b| {
                (&a.canonical, a.severity_number, &a.scope_name).cmp(&(
                    &b.canonical,
                    b.severity_number,
                    &b.scope_name,
                ))
            });
            out
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
            AdoptedTemplateRecord, LeafRecord, SnapshotState, StructuredTemplateRecord,
            TokenRecord, provenance_set_to_record, slot_types_vec_to_record,
        };

        let Some(state) = self.tenants.get(tenant_id) else {
            return SnapshotState {
                leaves: Vec::new(),
                structured_templates: Vec::new(),
                wal_high_water: None,
                adopted_templates: Vec::new(),
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
                provenance: provenance_set_to_record(leaf.provenance),
                upstream_associations: leaf
                    .upstream_associations
                    .strings()
                    .map(str::to_string)
                    .collect(),
                upstream_association_overflow: leaf.upstream_associations.overflow(),
            })
            .collect();
        leaves.sort_by_key(|leaf| leaf.template_id);

        let mut structured_templates: Vec<StructuredTemplateRecord> = state
            .structured_templates
            .iter()
            .map(|((severity_number, scope_name, event_name), template_id)| {
                StructuredTemplateRecord {
                    severity_number: *severity_number,
                    scope_name: scope_name.clone(),
                    event_name: event_name.clone(),
                    template_id: *template_id,
                }
            })
            .collect();
        structured_templates.sort_by_key(|record| record.template_id);

        let mut adopted_templates: Vec<AdoptedTemplateRecord> = state
            .adopted_templates
            .iter()
            .map(
                |((canonical, severity_number, scope_name), entry)| match entry {
                    AdoptedEntry::TreeBacked {
                        template_id,
                        template_version,
                    } => AdoptedTemplateRecord {
                        canonical: canonical.clone(),
                        severity_number: *severity_number,
                        scope_name: scope_name.clone(),
                        template_id: *template_id,
                        template_version: *template_version,
                        owned: false,
                        provenance: Vec::new(),
                        upstream_associations: Vec::new(),
                        upstream_association_overflow: 0,
                    },
                    AdoptedEntry::Owned(o) => AdoptedTemplateRecord {
                        canonical: canonical.clone(),
                        severity_number: *severity_number,
                        scope_name: scope_name.clone(),
                        template_id: o.template_id,
                        template_version: 1,
                        owned: true,
                        provenance: provenance_set_to_record(o.provenance),
                        upstream_associations: o
                            .associations
                            .strings()
                            .map(str::to_string)
                            .collect(),
                        upstream_association_overflow: o.associations.overflow(),
                    },
                },
            )
            .collect();
        adopted_templates
            .sort_by(|a, b| (a.template_id, &a.canonical).cmp(&(b.template_id, &b.canonical)));

        SnapshotState {
            leaves,
            structured_templates,
            wal_high_water: None,
            adopted_templates,
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
        let max_node_children = config.max_node_children;
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
            restore_leaf_into(&mut tenant, record, prefix_depth, max_node_children)?;
        }

        for record in &state.structured_templates {
            if !seen_ids.insert(record.template_id) {
                return Err(RestoreError::Inconsistent {
                    detail: format!("template_id {} appears more than once", record.template_id),
                });
            }
            let key = (
                record.severity_number,
                record.scope_name.clone(),
                record.event_name.clone(),
            );
            if tenant
                .structured_templates
                .insert(key, record.template_id)
                .is_some()
            {
                return Err(RestoreError::Inconsistent {
                    detail: format!(
                        "structured key (severity {}, scope {:?}, event {:?}) appears more than once",
                        record.severity_number, record.scope_name, record.event_name,
                    ),
                });
            }
        }
        // RFC 0050 §3.3 adopted-template map. Owned entries own
        // their id (unique like any other); tree-backed entries
        // reference a leaf id restored above, so they stay out of
        // `seen_ids`. An owned entry written with an empty
        // provenance list restores as `{UpstreamDerived}` — the
        // only origin an owned entry can carry without having
        // converged (a converged one is tree-backed by
        // construction).
        // Prepass for tree-backed reference validation below: the
        // snapshot's own leaf records are exactly what was restored.
        let leaf_by_id: HashMap<u64, &crate::snapshot::LeafRecord> =
            state.leaves.iter().map(|l| (l.template_id, l)).collect();
        for record in &state.adopted_templates {
            if record.owned {
                if !seen_ids.insert(record.template_id) {
                    return Err(RestoreError::Inconsistent {
                        detail: format!(
                            "template_id {} appears more than once",
                            record.template_id
                        ),
                    });
                }
                tenant.owned_adopted_count += 1;
            } else {
                validate_tree_backed_adoption(record, &leaf_by_id)?;
            }
            let key = (
                record.canonical.clone(),
                record.severity_number,
                record.scope_name.clone(),
            );
            if tenant
                .adopted_templates
                .insert(key, restore_adopted_entry(record))
                .is_some()
            {
                return Err(RestoreError::Inconsistent {
                    detail: format!(
                        "adopted canonical {:?} (severity {}, scope {:?}) appears more than once",
                        record.canonical, record.severity_number, record.scope_name,
                    ),
                });
            }
        }

        // Mirror live ingest's cache invariant: every fresh
        // allocation — tree leaf, structured-map entry, or owned
        // adopted entry — counts. `leaf_count` plus
        // `owned_adopted_count` is the RFC 0023/0050 ceiling basis.
        tenant.template_count =
            state.leaves.len() + state.structured_templates.len() + tenant.owned_adopted_count;
        tenant.leaf_count = state.leaves.len();

        // The id allocator is cluster-wide; without this bump a
        // post-restore allocation would collide with a restored id.
        let max_restored = state
            .leaves
            .iter()
            .map(|l| l.template_id)
            .chain(state.structured_templates.iter().map(|s| s.template_id))
            .chain(state.adopted_templates.iter().map(|a| a.template_id))
            .max();
        if let Some(max_restored) = max_restored {
            self.next_template_id = self.next_template_id.max(max_restored + 1);
        }

        self.tenants.insert(tenant_id.clone(), tenant);
        Ok(())
    }
}

/// Rebuild one tree leaf from its snapshot record during
/// [`MinerCluster::restore_tenant`] — template tokens, slot types,
/// the RFC 0050 provenance/association fields, and the masked
/// descent path.
///
/// `descend_mut` reads the slice length as the length bucket and
/// only the first `min(prefix_depth, len)` entries as the prefix
/// path, so positions past the path take a filler. A path-position
/// wildcard can only arise from mask emission at leaf creation, and
/// every line reaching the leaf carries the identical masked tag
/// there — widening and type-expansion are impossible at path
/// positions because tree candidates share their first walk-depth
/// masked tokens by construction. Its recorded slot set is
/// therefore a singleton of a mask-emitted type, whose tag string
/// is the path component.
pub(super) fn restore_leaf_into(
    tenant: &mut TenantState,
    record: &crate::snapshot::LeafRecord,
    prefix_depth: usize,
    max_node_children: u16,
) -> Result<(), RestoreError> {
    use crate::snapshot::record_to_provenance_set;

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

    // Index the restored canonical for the RFC0050.6 convergence
    // guard, before `template` moves into the leaf.
    tenant.mined_canonicals.insert((
        format_template(&template),
        record.severity_number,
        record.scope_name.clone(),
    ));
    let parent = tenant
        .tree
        .descend_mut(&masked, prefix_depth, usize::from(max_node_children));
    parent.leaves.push(Leaf {
        template,
        template_id: record.template_id,
        template_version: record.template_version,
        severity_number: record.severity_number,
        scope_name: record.scope_name.clone(),
        slot_types,
        provenance: record_to_provenance_set(&record.provenance),
        upstream_associations: UpstreamAssociations::from_parts(
            record.upstream_associations.iter().cloned(),
            record.upstream_association_overflow,
        ),
    });
    Ok(())
}

/// Reject a tree-backed adopted-template record whose leaf
/// reference does not hold in the same snapshot (RFC 0050 §3.3):
/// the referenced leaf must exist with the same `(severity, scope)`
/// key, the bound version must not exceed the leaf's, and when the
/// bind is to the leaf's *current* version the canonicals must
/// match. (A bind to an older version is legitimate — the leaf may
/// have widened after the adoption — and its tokens live in the
/// audit-derived registry, not the snapshot, so only the current
/// version is canonical-checkable here.)
pub(super) fn validate_tree_backed_adoption(
    record: &crate::snapshot::AdoptedTemplateRecord,
    leaf_by_id: &HashMap<u64, &crate::snapshot::LeafRecord>,
) -> Result<(), RestoreError> {
    let Some(leaf) = leaf_by_id.get(&record.template_id) else {
        return Err(RestoreError::Inconsistent {
            detail: format!(
                "adopted canonical {:?} references template_id {} with no restored leaf",
                record.canonical, record.template_id,
            ),
        });
    };
    if leaf.severity_number != record.severity_number || leaf.scope_name != record.scope_name {
        return Err(RestoreError::Inconsistent {
            detail: format!(
                "adopted canonical {:?} references template_id {} under a different \
                 (severity, scope) key",
                record.canonical, record.template_id,
            ),
        });
    }
    if record.template_version > leaf.template_version {
        return Err(RestoreError::Inconsistent {
            detail: format!(
                "adopted canonical {:?} binds template_id {} at version {} beyond the \
                 leaf's version {}",
                record.canonical,
                record.template_id,
                record.template_version,
                leaf.template_version,
            ),
        });
    }
    if record.template_version == leaf.template_version {
        let leaf_tokens: Vec<OwnedToken> = leaf.template.iter().map(OwnedToken::from).collect();
        if format_template(&leaf_tokens) != record.canonical {
            return Err(RestoreError::Inconsistent {
                detail: format!(
                    "adopted canonical {:?} disagrees with template_id {}'s tokens at \
                     version {}",
                    record.canonical, record.template_id, record.template_version,
                ),
            });
        }
    }
    Ok(())
}

/// Rebuild one adopted-template map entry from its snapshot record
/// (RFC 0050 §3.3). An owned entry written with an empty provenance
/// list restores as `{UpstreamDerived}` — the only origin an owned
/// entry can carry without having converged (a converged one is
/// tree-backed by construction).
pub(super) fn restore_adopted_entry(
    record: &crate::snapshot::AdoptedTemplateRecord,
) -> AdoptedEntry {
    if !record.owned {
        return AdoptedEntry::TreeBacked {
            template_id: record.template_id,
            template_version: record.template_version,
        };
    }
    let provenance = if record.provenance.is_empty() {
        ProvenanceSet::singleton(Provenance::UpstreamDerived)
    } else {
        record
            .provenance
            .iter()
            .map(|p| Provenance::from(*p))
            .collect()
    };
    AdoptedEntry::Owned(OwnedAdopted {
        template_id: record.template_id,
        provenance,
        associations: UpstreamAssociations::from_parts(
            record.upstream_associations.iter().cloned(),
            record.upstream_association_overflow,
        ),
    })
}

/// Rebuild a leaf's per-slot type sets from the recorded snapshot
/// during [`MinerCluster::restore_tenant`], rejecting a set count
/// that disagrees with the wildcard count or an empty recorded set
/// (live ingest produces neither).
pub(super) fn restore_slot_types(
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
pub(super) fn path_tag(
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
    /// RFC 0050 §3.3 origin set for this template.
    pub provenance: ProvenanceSet,
    /// RFC 0050 §3.2 `observe` associations: stored upstream
    /// strings (lexicographic) and the count of observations past
    /// the bound.
    pub upstream_associations: Vec<String>,
    pub upstream_association_overflow: u64,
}

/// Read-out view of one adopted-template entry (RFC 0050 §3.3),
/// from [`MinerCluster::adopted_templates_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedSnapshot {
    /// Canonical shape — the map key's template half.
    pub canonical: String,
    pub severity_number: u8,
    pub scope_name: Option<String>,
    pub template_id: u64,
    pub template_version: u32,
    /// `true` for an adoption-interned entry (owns its id, no tree
    /// leaf, counted against the ceiling); `false` when the
    /// adoption rides a mined leaf.
    pub owned: bool,
    /// Owned entries only — a tree-backed entry's provenance lives
    /// on its leaf.
    pub provenance: Option<ProvenanceSet>,
    pub upstream_associations: Vec<String>,
    pub upstream_association_overflow: u64,
}
