//! The §3.1 audit-chain oracle for the miner target: every template an
//! ingest created, widened or type-expanded is accounted for by its audit
//! events, replayed on typed tokens and slot types against the tree before
//! and after. Shared as a source file (each target pulls it in with
//! `#[path]`, like `telemetry.rs`) so the fuzz package keeps no library API
//! (RFC 0015 §3.1).

#![deny(unsafe_code)]

use std::collections::HashMap;

use ourios_core::audit::{
    AuditEvent, AuditPayload, ParamType, SlotExpansion, SlotTypes, TemplateChange,
};
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::mask::mask;
use ourios_miner::tokenize::tokenize;
use ourios_miner::tree::{OwnedToken, format_template};

/// One leaf's typed state: its version, tokens, and the `ParamType`s seen in
/// each wildcard slot (indexed by wildcard ordinal, RFC 0001 §6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafState {
    pub version: u32,
    pub tokens: Vec<OwnedToken>,
    pub slot_types: Vec<SlotTypes>,
}

/// `template_id → state` for every leaf of one tenant.
pub type Leaves = HashMap<u64, LeafState>;

/// The tenant's leaves as they stand now, for [`AuditCtx`].
pub fn leaves(miner: &MinerCluster, tenant: &TenantId) -> Leaves {
    miner
        .templates_for(tenant)
        .into_iter()
        .map(|leaf| {
            let state = LeafState {
                version: leaf.template_version,
                tokens: leaf.template,
                slot_types: leaf.slot_types,
            };
            (leaf.template_id, state)
        })
        .collect()
}

/// What one ingest did to the tenant's tree: its line number and body, the
/// leaves before and after it. [`AuditCtx::assert_changes_audited`] checks the
/// audit events the ingest emitted against it.
pub struct AuditCtx<'a> {
    pub index: usize,
    pub tenant: &'a TenantId,
    pub line: &'a str,
    pub before: &'a Leaves,
    pub after: &'a Leaves,
}

impl AuditCtx<'_> {
    /// RFC0015.1 — `AGENTS.md` §3.1 "every merge emits an audit event with
    /// old + new templates" (RFC 0001 H1.3), checked against what one ingest
    /// actually did to the tree, given everything the audit sink received
    /// during it:
    ///
    /// - a new leaf has exactly one `Created` event carrying its tokens;
    /// - a leaf whose version rose from `v` to `w` has exactly the chain of
    ///   `Widened` / `TypeExpanded` events `v → v+1 → … → w`, each link's
    ///   templates continuing the previous one, starting at the tokens before
    ///   the ingest and ending at the tokens after it;
    /// - every link of that chain is a legal step on its own (RFC0015.10,
    ///   [`LeafAudit::assert_step_shape`]), so an intermediate version that
    ///   no row was stamped with is checked too;
    /// - no other leaf has any template event (a degenerate-widening
    ///   rejection, which changes nothing, is the exception).
    ///
    /// # Panics
    ///
    /// On any violation, with a message naming the invariant.
    pub fn assert_changes_audited(&self, events: Vec<AuditEvent>) {
        self.assert_events_name_tenant(&events);
        let mut by_id = template_changes_by_id(events);
        let line_types = line_types(self.line);
        for (&id, after) in self.after {
            let changes = by_id.remove(&id).unwrap_or_default();
            let leaf = LeafAudit {
                index: self.index,
                id,
                after,
                changes: &changes,
                line_types: &line_types,
            };
            leaf.assert_audited(self.before.get(&id));
        }
        let orphans: Vec<u64> = by_id.into_keys().collect();
        assert!(
            orphans.is_empty(),
            "RFC0015.1 / AGENTS.md §3.1: line {}: template events for ids {orphans:?}, which have no leaf",
            self.index,
        );
    }

    /// RFC0015.1 — `AGENTS.md` §3.1 / §3.7: an audit event names the tenant
    /// whose ingest caused it (RFC 0001 H1.3). Checked on every drained event,
    /// before any grouping drops the field.
    fn assert_events_name_tenant(&self, events: &[AuditEvent]) {
        for event in events {
            assert!(
                event.tenant_id == *self.tenant,
                "RFC0015.1 / AGENTS.md §3.1/§3.7: line {}: audit event {:?} names tenant {:?}, but the \
                 ingest that emitted it was tenant {:?}",
                self.index,
                event.payload,
                event.tenant_id,
                self.tenant,
            );
        }
    }
}

/// The template events of one ingest, per template id, in emission order.
/// Degenerate-widening rejections change no template and are dropped.
fn template_changes_by_id(events: Vec<AuditEvent>) -> HashMap<u64, Vec<TemplateChange>> {
    let mut by_id: HashMap<u64, Vec<TemplateChange>> = HashMap::new();
    for (id, change) in events.into_iter().filter_map(template_change) {
        by_id.entry(id).or_default().push(change);
    }
    by_id
}

fn template_change(event: AuditEvent) -> Option<(u64, TemplateChange)> {
    match event.payload {
        AuditPayload::Template {
            change: TemplateChange::RejectedDegenerate { .. },
            ..
        } => None,
        AuditPayload::Template {
            template_id,
            change,
            ..
        } => Some((template_id, change)),
        _ => None,
    }
}

/// The `ParamType` of each token of `line`, as the miner classifies it for a
/// widened slot's initial type set (RFC 0001 §6.2 step 5b, mirroring
/// `param_type_for_line_position`): the mask's type where it masked the
/// token, `Str` for a literal. Empty when the tokenizer rejects the line,
/// which can then widen nothing.
fn line_types(line: &str) -> Vec<ParamType> {
    let Ok(tokenized) = tokenize(line) else {
        return Vec::new();
    };
    let masked = mask(&tokenized.tokens);
    (0..tokenized.tokens.len())
        .map(|position| {
            masked
                .wildcard_positions
                .binary_search(&position)
                .ok()
                .and_then(|k| masked.typed_params.get(k))
                .map_or(ParamType::Str, |param| param.type_tag)
        })
        .collect()
}

/// One leaf after one ingest, with the template events the ingest emitted
/// for it and the ingested line's per-token types.
struct LeafAudit<'a> {
    index: usize,
    id: u64,
    after: &'a LeafState,
    changes: &'a [TemplateChange],
    line_types: &'a [ParamType],
}

impl LeafAudit<'_> {
    fn assert_audited(&self, before: Option<&LeafState>) {
        match before {
            None => self.assert_created(),
            Some(before) => self.assert_steps(before),
        }
    }

    fn assert_created(&self) {
        let LeafState {
            version, tokens, ..
        } = self.after;
        let version = *version;
        let expected = format_template(tokens);
        let audited = matches!(
            self.changes,
            [TemplateChange::Created { new_template }] if *new_template == expected
        );
        assert!(
            version == 1 && audited,
            "RFC0015.1 / AGENTS.md §3.1: line {}: new template {} (v{version}, {expected:?}) must be \
             audited by exactly one `Created` event with its tokens; got {:?}",
            self.index,
            self.id,
            self.changes,
        );
    }

    fn assert_steps(&self, before: &LeafState) {
        let old_version = before.version;
        let mut version = old_version;
        let mut template = format_template(&before.tokens);
        let mut typed = before.clone();
        for change in self.changes {
            let step = version_step(change).unwrap_or_else(|| {
                panic!(
                    "RFC0015.1 / AGENTS.md §3.1: line {}: template {} already existed but got {change:?}",
                    self.index, self.id,
                )
            });
            assert!(
                step.0 == version && step.1 == version + 1 && step.2 == template,
                "RFC0015.1 / AGENTS.md §3.1: line {}: template {} event {change:?} does not continue \
                 from v{version} {template:?}",
                self.index,
                self.id,
            );
            self.assert_step_shape(change);
            self.replay_typed_step(&mut typed, change);
            version = step.1;
            step.3.clone_into(&mut template);
        }
        let new_version = self.after.version;
        let new_tokens = &self.after.tokens;
        assert!(
            typed.tokens == *new_tokens,
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {}: replaying the declared widenings on the \
             leaf before the ingest gives {:?}, but the leaf after it is {new_tokens:?} — a token \
             changed that no audit event declares",
            self.index,
            self.id,
            typed.tokens,
        );
        assert!(
            typed.slot_types == self.after.slot_types,
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {}: replaying the declared widenings and \
             type expansions gives slot types {:?}, but the leaf after the ingest has {:?} — a slot \
             type changed that no audit event declares",
            self.index,
            self.id,
            typed.slot_types,
            self.after.slot_types,
        );
        let expected = format_template(new_tokens);
        assert!(
            version == new_version && template == expected,
            "RFC0015.1 / AGENTS.md §3.1: line {}: template {} moved v{old_version}→v{new_version} \
             ({expected:?}) but its audit events only account for v{old_version}→v{version} \
             ({template:?}) — a merge without its audit event",
            self.index,
            self.id,
        );
    }

    /// RFC0015.10 on typed tokens. The events carry templates only as
    /// canonical strings, where a literal `<*>` token and a wildcard print
    /// the same, but the chain's ends are typed leaves. So the typed template
    /// is carried through the chain: from the leaf before the ingest, each
    /// `Widened` step turns exactly its declared positions (strictly
    /// increasing, in range, each a literal until then) into wildcards, each
    /// `TypeExpanded` step changes nothing, and every intermediate must print
    /// as the event says. [`Self::assert_steps`] then requires the result to
    /// equal the typed leaf after the ingest, so a position widened without
    /// being declared, a literal `<*>` included, cannot pass.
    ///
    /// Slot types are replayed the same way, per RFC 0001 §6.1/§6.4: a
    /// widening opens one slot per declared position, seeded `{Str, type of
    /// the line's token there}` and not audited as an expansion; a type
    /// expansion's `slots_expanded` names existing slots by wildcard ordinal,
    /// each at most once, with types new to that slot. The replayed sets must
    /// equal the leaf's after the ingest, so a slot-type change no event
    /// declares, or a declaration the tree does not show, cannot pass.
    fn replay_typed_step(&self, typed: &mut LeafState, change: &TemplateChange) {
        let new_template = match change {
            TemplateChange::Widened {
                positions_widened,
                new_template,
                ..
            } => {
                self.replay_widening(typed, positions_widened);
                new_template
            }
            TemplateChange::TypeExpanded {
                slots_expanded,
                new_template,
                ..
            } => {
                self.replay_type_expansion(typed, slots_expanded);
                new_template
            }
            _ => return,
        };
        let printed = format_template(&typed.tokens);
        assert_eq!(
            &printed, new_template,
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {} event {change:?} does not match its declared \
             positions applied to the typed template",
            self.index, self.id,
        );
    }

    fn replay_widening(&self, typed: &mut LeafState, positions: &[u16]) {
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {} declares positions_widened {positions:?}, \
             which is not strictly increasing",
            self.index,
            self.id,
        );
        for &position in positions {
            self.widen_typed(&mut typed.tokens, usize::from(position));
        }
        for &position in positions {
            self.seed_widened_slot(typed, usize::from(position));
        }
    }

    /// The new slot's ordinal is its wildcard count on the post-widening
    /// template, walked in ascending position order as the miner does.
    fn seed_widened_slot(&self, typed: &mut LeafState, position: usize) {
        let ordinal = typed.tokens[..position]
            .iter()
            .filter(|token| matches!(token, OwnedToken::Wildcard))
            .count();
        let line_type = self.line_types.get(position).copied().unwrap_or_else(|| {
            panic!(
                "RFC0015.10 / AGENTS.md §3.1: line {}: template {} widened position {position}, which the \
                 line has no token at",
                self.index, self.id,
            )
        });
        assert!(
            ordinal <= typed.slot_types.len(),
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {} has {} slot types for more wildcards — \
             slot types out of step with the template",
            self.index,
            self.id,
            typed.slot_types.len(),
        );
        typed.slot_types.insert(
            ordinal,
            SlotTypes::singleton(ParamType::Str).insert(line_type),
        );
    }

    fn replay_type_expansion(&self, typed: &mut LeafState, slots: &[SlotExpansion]) {
        let mut indices: Vec<u16> = slots.iter().map(|slot| slot.slot_index).collect();
        indices.sort_unstable();
        indices.dedup();
        assert!(
            !slots.is_empty() && indices.len() == slots.len(),
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {} type expansion declares {slots:?}: it must \
             name at least one slot, each at most once",
            self.index,
            self.id,
        );
        for slot in slots {
            self.expand_slot(typed, slot);
        }
    }

    fn expand_slot(&self, typed: &mut LeafState, expansion: &SlotExpansion) {
        let SlotExpansion {
            slot_index,
            added_types,
        } = expansion;
        let slot = typed
            .slot_types
            .get_mut(usize::from(*slot_index))
            .unwrap_or_else(|| {
                panic!(
                    "RFC0015.10 / AGENTS.md §3.1: line {}: template {} type expansion names slot \
                     {slot_index}, which does not exist",
                    self.index, self.id,
                )
            });
        assert!(
            !added_types.is_empty(),
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {} type expansion of slot {slot_index} adds \
             no type",
            self.index,
            self.id,
        );
        for &added in added_types {
            assert!(
                !slot.contains(added),
                "RFC0015.10 / AGENTS.md §3.1: line {}: template {} type expansion of slot {slot_index} \
                 adds {added:?}, which the slot already had",
                self.index,
                self.id,
            );
            *slot = slot.insert(added);
        }
    }

    fn widen_typed(&self, typed: &mut [OwnedToken], position: usize) {
        let token = typed.get_mut(position).unwrap_or_else(|| {
            panic!(
                "RFC0015.10 / AGENTS.md §3.1: line {}: template {} declares widened position {position} \
                 past its end",
                self.index, self.id,
            )
        });
        assert!(
            matches!(token, OwnedToken::Fixed(_)),
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {} declares position {position} widened, \
             but it was already a wildcard",
            self.index,
            self.id,
        );
        *token = OwnedToken::Wildcard;
    }

    /// RFC0015.10 — `AGENTS.md` §3.1 for every link of the chain, including
    /// versions no emitted row was stamped with: a widening turns exactly its
    /// declared positions into wildcards and changes nothing else; a type
    /// expansion changes no token at all.
    fn assert_step_shape(&self, change: &TemplateChange) {
        match change {
            TemplateChange::Widened {
                old_template,
                new_template,
                positions_widened,
                ..
            } => self.assert_widened_shape(old_template, new_template, positions_widened),
            TemplateChange::TypeExpanded {
                old_template,
                new_template,
                ..
            } => assert_eq!(
                old_template, new_template,
                "RFC0015.10 / AGENTS.md §3.1: line {}: template {} type expansion changed its tokens",
                self.index, self.id,
            ),
            _ => {}
        }
    }

    /// Compared on the canonical strings token by token, not on
    /// `parse_template`: a literal `<*>` token is legal input and would parse
    /// as a wildcard.
    fn assert_widened_shape(&self, old: &str, new: &str, positions: &[u16]) {
        let old_tokens: Vec<&str> = old.split(' ').collect();
        let new_tokens: Vec<&str> = new.split(' ').collect();
        assert_eq!(
            old_tokens.len(),
            new_tokens.len(),
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {} widening {old:?} → {new:?} changed length",
            self.index,
            self.id,
        );
        let declared: Vec<usize> = positions.iter().map(|&p| usize::from(p)).collect();
        assert!(
            !declared.is_empty() && declared.iter().all(|&p| new_tokens.get(p) == Some(&"<*>")),
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {} widening {old:?} → {new:?} declares \
             positions {positions:?} that are not all wildcards afterwards",
            self.index,
            self.id,
        );
        let changed: Vec<usize> = old_tokens
            .iter()
            .zip(&new_tokens)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(position, _)| position)
            .collect();
        assert!(
            changed.iter().all(|p| declared.contains(p)),
            "RFC0015.10 / AGENTS.md §3.1: line {}: template {} widening {old:?} → {new:?} changed \
             positions {changed:?}, outside its declared {positions:?} — a merge that rewrites a literal",
            self.index,
            self.id,
        );
    }
}

/// `(old_version, new_version, old_template, new_template)` of a change that
/// moves an existing template forward one version.
fn version_step(change: &TemplateChange) -> Option<(u32, u32, &str, &str)> {
    match change {
        TemplateChange::Widened {
            old_version,
            new_version,
            old_template,
            new_template,
            ..
        }
        | TemplateChange::TypeExpanded {
            old_version,
            new_version,
            old_template,
            new_template,
            ..
        } => Some((*old_version, *new_version, old_template, new_template)),
        _ => None,
    }
}
