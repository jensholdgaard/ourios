//! Rebuilding one tree leaf from its snapshot record: the slot types,
//! and the descent path that puts the leaf back where live ingest left it.

use ourios_core::audit::{ParamType, SlotTypes};

use super::RestoreError;
use crate::cluster::TenantState;
use crate::mask::tag_str_for;
use crate::tree::{Leaf, OwnedToken, UpstreamAssociations, WILDCARD_CHILD, format_template};

/// Rebuild one tree leaf from its snapshot record during
/// [`MinerCluster::restore_tenant`] — template tokens, slot types,
/// the RFC 0050 provenance/association fields, and the masked
/// descent path.
///
/// Under a keyed branch every line reaching the leaf carries the
/// identical masked token at that path position, so a path-position
/// wildcard there came from mask emission and its recorded slot set is
/// a singleton mask-emitted type, whose tag string is the path
/// component. Under RFC 0023 §3.1's wildcard child lines differ there,
/// so the leaf may widen or type-expand at that position; the snapshot
/// records those positions and the path takes the wildcard child.
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
    let masked = descent_path(record, &template, prefix_depth.min(template.len()))?;

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

/// The masked descent path that re-creates `record`'s leaf position:
/// [`WILDCARD_CHILD`] at each recorded wildcard-routed position, the
/// template's own token or mask tag at every other one. Positions past
/// the walk only fix the length bucket, so they take a filler.
fn descent_path<'r>(
    record: &crate::snapshot::LeafRecord,
    template: &'r [OwnedToken],
    walk_depth: usize,
) -> Result<Vec<&'r str>, RestoreError> {
    check_routes(record, template, walk_depth)?;
    let mut masked = Vec::with_capacity(template.len());
    let mut slot = 0usize;
    for (position, token) in template.iter().enumerate() {
        masked.push(match token {
            _ if record.wildcard_routed.contains(&position) => WILDCARD_CHILD,
            OwnedToken::Fixed(s) => s.as_str(),
            OwnedToken::Wildcard if position < walk_depth => path_tag(record, slot, position)?,
            OwnedToken::Wildcard => "<*>",
        });
        slot += usize::from(matches!(token, OwnedToken::Wildcard));
    }
    Ok(masked)
}

/// The recorded wildcard-routed positions must be ascending positions on
/// the walk. A literal `<*>` token on the walk keys the wildcard child
/// itself, so capture always records its position as routed; a record
/// that does not is inconsistent.
fn check_routes(
    record: &crate::snapshot::LeafRecord,
    template: &[OwnedToken],
    walk_depth: usize,
) -> Result<(), RestoreError> {
    let routed = &record.wildcard_routed;
    let ascending = routed.windows(2).all(|pair| pair[0] < pair[1]);
    if !ascending || routed.last().is_some_and(|&last| last >= walk_depth) {
        return Err(RestoreError::Inconsistent {
            detail: format!(
                "template_id {}: wildcard-routed positions {routed:?} are not ascending \
                 positions below the walk depth {walk_depth}",
                record.template_id,
            ),
        });
    }
    let unrouted_literal =
        template
            .iter()
            .take(walk_depth)
            .enumerate()
            .find(|(position, token)| {
                matches!(token, OwnedToken::Fixed(s) if s == WILDCARD_CHILD)
                    && !routed.contains(position)
            });
    match unrouted_literal {
        Some((position, _)) => Err(RestoreError::Inconsistent {
            detail: format!(
                "template_id {}: literal {WILDCARD_CHILD} at path position {position} \
                 is not recorded as wildcard-routed",
                record.template_id,
            ),
        }),
        None => Ok(()),
    }
}

/// Rebuild a leaf's per-slot type sets from the recorded snapshot
/// during [`MinerCluster::restore_tenant`], rejecting a set count
/// that disagrees with the wildcard count or an empty recorded set
/// (live ingest produces neither).
fn restore_slot_types(
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
        // `SlotTypes` has no bit for a reader-side `Unknown` ordinal, so
        // restoring one would silently drop it, and a slot of only
        // unknowns would come back empty: the next snapshot would carry
        // that empty set and the restore after it reject it. The miner
        // never mints `Unknown`, so the snapshot is not from a tree this
        // build can rebuild.
        if let Some(unknown) = recorded
            .iter()
            .find(|t| matches!(t, crate::snapshot::ParamTypeRecord::Unknown(_)))
        {
            return Err(RestoreError::Inconsistent {
                detail: format!(
                    "template_id {} slot {slot}: recorded type {unknown:?} is not one this \
                     build tracks",
                    record.template_id,
                ),
            });
        }
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
fn path_tag(
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
