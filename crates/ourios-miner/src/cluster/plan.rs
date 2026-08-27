//! The pure attach/widening algebra (RFC 0001 §6.2 step 5): mask→param
//! materialisation, slot-type bookkeeping, [`AttachPlan`] and the
//! candidate/widening primitives. Every item is `self`-free — moved
//! verbatim from the flat `cluster.rs` (epic #745 wave 2).

// The parent scope IS this module's import surface: the split was
// mechanical code motion (epic #745 wave 2), and gluing back through
// `super` keeps every pre-split path resolving unchanged.
#[allow(clippy::wildcard_imports)]
use super::*;

/// Free helper: clone tokenize's borrowed-from-input separators
/// into the `Vec<String>` shape `MinedRecord::separators`
/// requires. RFC §6.6 "capture, always": the order and length
/// invariants (`separators.len() == tokens.len() + 1` on
/// `BodyKind::String`) are upheld by `tokenize` itself; this
/// just owns the bytes.
pub(super) fn separators_to_owned(separators: &[&str]) -> Vec<String> {
    separators.iter().map(|s| (*s).to_string()).collect()
}

/// Free helper: lift `mask`'s typed-params output into the
/// `Vec<Param>` shape `MinedRecord::params` carries, applying the
/// §6.5 per-parameter byte-limit check.
///
/// Any value whose UTF-8 byte length exceeds `byte_limit` is
/// replaced by an `Overflow` marker (RFC §6.5); the caller is
/// responsible for setting `body = Some(raw)` on the emitted
/// record when [`crate::overflow::any_overflow`] returns true for
/// the resulting params vector.
pub(super) fn params_from_mask(
    typed_params: &[crate::mask::TypedParam<'_>],
    byte_limit: u32,
) -> Vec<Param> {
    typed_params
        .iter()
        .map(|p| crate::overflow::cap_param_value(p.type_tag, p.value.to_string(), byte_limit))
        .collect()
}

/// The record's `log.record.template` value, when it is a non-empty
/// string attribute (RFC 0050 §3.1). First match wins; a non-string
/// or empty value reads as absent — never coerced.
pub(super) fn upstream_template_of(record: &OtlpLogRecord) -> Option<&str> {
    record
        .attributes
        .iter()
        .find(|kv| kv.key == LOG_RECORD_TEMPLATE_ATTR)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|av| av.value.as_ref())
        .and_then(|v| match v {
            any_value::Value::StringValue(s) if !s.is_empty() => Some(s.as_str()),
            _ => None,
        })
}

/// Look up the `ParamType` at a line position from `mask()`'s
/// classification — the authoritative source.
///
/// `wildcard_positions` is ascending (single forward pass over the
/// input tokens), so a binary search is `O(log n)` per call.
/// Returns `Str` for positions `mask()` did not classify (the
/// original token wasn't a numeric, UUID, or IPv4 literal); per
/// RFC §6.2 step 5 the literal value is captured as
/// `ParamType::Str`.
///
/// **Why not match the masked-token string content.** An input log
/// line that contains the literal token `"<NUM>"` / `"<IP>"` /
/// `"<UUID>"` passes through `mask()` unchanged because the rules
/// (digits / IPv4 / UUID) don't fire on it. The masked-token at
/// that position is therefore the literal string, indistinguishable
/// by content from a mask-emitted tag. String-shape inference
/// would mis-classify the literal as the corresponding `ParamType`
/// and corrupt `slot_types` / suppress `TemplateTypeExpanded`
/// audits. The `wildcard_positions` array does not include the
/// position for the literal case, so this lookup gives the right
/// answer in both.
pub(super) fn param_type_for_line_position(
    p: usize,
    wildcard_positions: &[usize],
    typed_params: &[crate::mask::TypedParam<'_>],
) -> ParamType {
    debug_assert_eq!(
        wildcard_positions.len(),
        typed_params.len(),
        "mask invariant: typed_params parallel to wildcard_positions",
    );
    match wildcard_positions.binary_search(&p) {
        Ok(k) => typed_params[k].type_tag,
        Err(_) => ParamType::Str,
    }
}

/// On widening, seed [`Leaf::slot_types`] for each newly-introduced
/// `Wildcard` position. The initial type set captures both
/// observations the widening witnessed:
///
/// - the pre-widen `Fixed` token — under PR-B-1's model this is
///   always a *literal* token (mask-emitted positions enter the
///   leaf as `Wildcard` from creation in [`MinerCluster::
///   create_new_leaf`]), so its `ParamType` is unconditionally
///   `Str` per RFC §6.2 step 5b.
/// - the line's token at that position (the value that triggered
///   the widening), classified from mask's output by
///   [`param_type_for_line_position`].
///
/// Neither observation counts as a `TemplateTypeExpanded` — the
/// slot didn't exist before this attach, so there's no "expansion"
/// to audit; the `TemplateWidened` event covers the structural
/// change. Subsequent attaches with a `ParamType` not in this
/// initial set are what trigger `TemplateTypeExpanded` events
/// later in the same attach (or in future attaches).
///
/// Ordinal alignment: positions are inserted into `slot_types` at
/// the post-widen wildcard-ordinal of each newly-widened position,
/// so the post-call invariant `slot_types.len() == count(template
/// Wildcards)` holds. `positions_widened` is ascending (by the
/// [`find_widening_positions`] contract), so a single forward walk
/// over the post-widen template is enough.
pub(super) fn update_slot_types_on_widening(
    slot_types: &mut Vec<SlotTypes>,
    post_widen_template: &[OwnedToken],
    line_wildcard_positions: &[usize],
    line_typed_params: &[crate::mask::TypedParam<'_>],
    positions_widened: &[usize],
) {
    debug_assert!(
        positions_widened.windows(2).all(|w| w[0] < w[1]),
        "positions_widened must be sorted ascending",
    );

    let mut ordinal = 0usize;
    let mut widen_iter = positions_widened.iter().copied().peekable();
    for (p, tok) in post_widen_template.iter().enumerate() {
        if matches!(tok, OwnedToken::Wildcard) {
            if widen_iter.peek().copied() == Some(p) {
                // Line side: authoritative classification from
                // mask. Leaf side: PR-B-1 invariant — the
                // pre-widen Fixed at a widened position is always
                // a literal (mask-emit positions enter as
                // Wildcard), so the initial slot type is
                // {Str, line_type}.
                let line_type =
                    param_type_for_line_position(p, line_wildcard_positions, line_typed_params);
                let initial = SlotTypes::singleton(ParamType::Str).insert(line_type);
                slot_types.insert(ordinal, initial);
                widen_iter.next();
            }
            ordinal += 1;
        }
    }
    debug_assert!(
        widen_iter.peek().is_none(),
        "every widened position must have a matching Wildcard in the post-widen template",
    );
}

/// Walk the post-widen template's `Wildcard` slots and collect any
/// `ParamType`s the current line introduces that aren't already in
/// the slot's `slot_types` entry. Each addition becomes one
/// [`SlotExpansion`] in the returned vector; an empty result means
/// the attach is a no-op for the type-expansion path.
///
/// `skip_positions` lists positions that were newly created by the
/// same attach's widening step — their `slot_types` entries were
/// just initialised in [`update_slot_types_on_widening`], so we do
/// **not** treat the initial state as an "expansion" (the
/// `TemplateWidened` event already covers the slot's existence,
/// and the initial type set is its first state, not an addition).
pub(super) fn collect_type_expansions(
    template: &[OwnedToken],
    line_wildcard_positions: &[usize],
    line_typed_params: &[crate::mask::TypedParam<'_>],
    slot_types: &[SlotTypes],
    skip_positions: &[usize],
) -> Vec<SlotExpansion> {
    debug_assert!(
        skip_positions.windows(2).all(|w| w[0] < w[1]),
        "skip_positions must be sorted ascending",
    );

    let mut out: Vec<SlotExpansion> = Vec::new();
    let mut ordinal: u16 = 0;
    let mut skip_iter = skip_positions.iter().copied().peekable();
    for (p, tok) in template.iter().enumerate() {
        if matches!(tok, OwnedToken::Wildcard) {
            let is_freshly_widened = skip_iter.peek().copied() == Some(p);
            if is_freshly_widened {
                skip_iter.next();
            } else {
                // Line side: authoritative classification from
                // mask, not from the masked-token string.
                let line_type =
                    param_type_for_line_position(p, line_wildcard_positions, line_typed_params);
                let current_set = slot_types[ordinal as usize];
                if !current_set.contains(line_type) {
                    out.push(SlotExpansion {
                        slot_index: ordinal,
                        added_types: vec![line_type],
                    });
                }
            }
            ordinal += 1;
        }
    }
    out
}

/// Apply the expansions returned by [`collect_type_expansions`] to
/// the leaf's `slot_types`. Idempotent under
/// [`SlotTypes::insert`] (re-applying the same expansion is a
/// no-op).
pub(super) fn apply_type_expansions(slot_types: &mut [SlotTypes], expansions: &[SlotExpansion]) {
    for exp in expansions {
        let s = &mut slot_types[exp.slot_index as usize];
        for &t in &exp.added_types {
            *s = s.insert(t);
        }
    }
}

/// RFC §6.6 alignment: build the `params` vector with one entry
/// per `Wildcard` slot in the leaf template, left-to-right.
///
/// For each wildcard at template position `p`:
///
/// - If the line had a mask emit at the same position
///   (`p ∈ line_wildcard_positions`), use that `TypedParam`
///   verbatim — the original token bytes and its `ParamType`.
/// - Else (the line had a literal at `p` but the leaf carries a
///   `Wildcard` there — either from a past widening of a
///   literal-token mismatch, or because this attach just
///   freshly-widened a literal at `p`), fall back to
///   `{ type_tag: Str, value: masked_strs[p] }`. `masked_strs[p]`
///   for a non-mask position is the original literal token (mask
///   leaves unclassified tokens unchanged), so the STR fallback
///   captures the bytes reconstruction will need.
///
/// This is the contract [`crate::reconstruct::reconstruct`] reads
/// against. Producers
/// for fresh-leaf paths (`None` / `Lossy` zones in
/// [`MinerCluster::ingest_string`]) build params via the simpler
/// [`params_from_mask`] because their template Wildcards align
/// 1:1 with the line's mask positions by construction; everything
/// else routes through this helper.
///
/// **Scope boundary.** STR fallback is invoked only after a leaf
/// has been *found*. The Drain tree's prefix routing keys each
/// level by the concrete masked token, so a leaf with a
/// `Wildcard` slot inside `prefix_depth` is structurally
/// unreachable from a line whose prefix masks to a different
/// concrete token at that position (e.g. a literal `abc` at
/// position 1 cannot find a leaf whose position-1 prefix key is
/// the mask-emitted `<NUM>`). This is a property of the Drain
/// tree (paper §3.2, RFC 0001 §6.1), not a bug in this helper;
/// any change to make wildcards reachable from divergent prefix
/// tokens (multi-bucket lookup, wildcard-aware re-bucketing) is
/// its own RFC-level decision.
pub(super) fn build_record_params(
    template: &[OwnedToken],
    masked_strs: &[&str],
    line_wildcard_positions: &[usize],
    line_typed_params: &[crate::mask::TypedParam<'_>],
    byte_limit: u32,
) -> Vec<Param> {
    debug_assert_eq!(
        line_wildcard_positions.len(),
        line_typed_params.len(),
        "mask invariant: typed_params parallel to wildcard_positions",
    );
    debug_assert_eq!(
        template.len(),
        masked_strs.len(),
        "sim_seq precondition: template and line are the same length",
    );

    let wildcard_count = template
        .iter()
        .filter(|t| matches!(t, OwnedToken::Wildcard))
        .count();
    let mut out = Vec::with_capacity(wildcard_count);
    let mut k = 0usize;
    for (p, tok) in template.iter().enumerate() {
        if !matches!(tok, OwnedToken::Wildcard) {
            continue;
        }
        let (type_tag, value) =
            if k < line_wildcard_positions.len() && line_wildcard_positions[k] == p {
                let entry = (
                    line_typed_params[k].type_tag,
                    line_typed_params[k].value.to_string(),
                );
                k += 1;
                entry
            } else {
                // STR fallback for an existing-Wildcard / freshly-
                // widened-literal slot — see helper docstring.
                (ParamType::Str, masked_strs[p].to_string())
            };
        // RFC §6.5 byte-limit check at the param boundary: an
        // over-cap value becomes an Overflow marker.
        out.push(crate::overflow::cap_param_value(
            type_tag, value, byte_limit,
        ));
    }
    debug_assert_eq!(
        k,
        line_wildcard_positions.len(),
        "every mask emit position must coincide with a template Wildcard",
    );
    out
}

/// Outcome of the leaf-mutating phase of `attach_and_maybe_widen`.
///
/// Phase 1 borrows the leaf and produces this enum; phase 2 drops
/// the leaf borrow and emits audit events / the data record using
/// the extracted data. The split keeps `&mut self.audit_sink` and
/// `&mut self.tenants` from clashing on the borrow checker — every
/// audit emit happens after the leaf borrow ends.
pub(super) enum AttachPlan {
    /// No mutation: similarity 1.0 with no new types at any slot.
    /// Reuse `(template_id, template_version)` verbatim. `params`
    /// is aligned with the leaf's wildcard slots per RFC §6.6 —
    /// see [`build_record_params`].
    CleanReuse {
        template_id: u64,
        template_version: u32,
        params: Vec<Param>,
    },
    /// Degenerate widening rejected per §6.4. Leaf untouched.
    Rejected {
        template_id: u64,
        version: u32,
        current_template: String,
        would_be_template: String,
        would_be_positions: Vec<u16>,
    },
    /// Leaf mutated (widened and/or type-expanded). `events` is the
    /// template-change payload in emission order: `Widened` before
    /// `TypeExpanded` per RFC §6.2's combined-attach contract.
    /// `params` is aligned with the post-widen template's wildcard
    /// slots.
    Mutated {
        template_id: u64,
        events: Vec<TemplateChange>,
        final_version: u32,
        params: Vec<Param>,
    },
}

/// RFC §6.2 step 5 — compute the structural mutations and the
/// resulting audit-event payloads for a candidate-attach decision.
/// Mutates `leaf.template`, `leaf.template_version`, and
/// `leaf.slot_types` in place when widening or type-expansion
/// fires; the caller drops the leaf borrow before draining the
/// returned `events`.
//
// This function maps 1:1 onto the RFC §6.2 step 5 algorithm:
// (clean reuse / type-expansion-only / degenerate rejection /
// widening + optional expansion). Each branch reads and mutates
// the same `leaf` state, so factoring branches into helpers would
// require shuttling the leaf back and forth (or returning partial
// `AttachPlan`s and re-entering). The current single-function
// shape keeps the RFC mapping line-for-line and the locking-tests
// in `cluster::tests` against this function direct; the
// too_many_lines lint is silenced here rather than fragmenting
// the algorithm for the lint's sake.
#[allow(clippy::too_many_lines)]
pub(super) fn plan_attach(
    leaf: &mut Leaf,
    masked_strs: &[&str],
    line_wildcard_positions: &[usize],
    line_typed_params: &[crate::mask::TypedParam<'_>],
    byte_limit: u32,
) -> AttachPlan {
    let positions_widened =
        find_widening_positions(masked_strs, &leaf.template, line_wildcard_positions);

    if positions_widened.is_empty() {
        // No Fixed mismatch — check for a type-expansion-only
        // attach (a known wildcard slot seeing a new ParamType).
        let expansions = collect_type_expansions(
            &leaf.template,
            line_wildcard_positions,
            line_typed_params,
            &leaf.slot_types,
            &[],
        );
        if expansions.is_empty() {
            return AttachPlan::CleanReuse {
                template_id: leaf.template_id,
                template_version: leaf.template_version,
                params: build_record_params(
                    &leaf.template,
                    masked_strs,
                    line_wildcard_positions,
                    line_typed_params,
                    byte_limit,
                ),
            };
        }
        apply_type_expansions(&mut leaf.slot_types, &expansions);
        let old_version = leaf.template_version;
        let new_version = leaf
            .template_version
            .checked_add(1)
            .expect("template_version overflow: 2^32 expansions on one leaf is implausible");
        leaf.template_version = new_version;
        let template_str = format_template(&leaf.template);
        let params = build_record_params(
            &leaf.template,
            masked_strs,
            line_wildcard_positions,
            line_typed_params,
            byte_limit,
        );
        return AttachPlan::Mutated {
            template_id: leaf.template_id,
            final_version: new_version,
            params,
            events: vec![TemplateChange::TypeExpanded {
                old_version,
                new_version,
                // Structure is unchanged by type expansion; both
                // fields carry the same canonical-form string per
                // RFC §6.4 (the expansion lives in `slots_expanded`).
                old_template: template_str.clone(),
                new_template: template_str,
                slots_expanded: expansions,
            }],
        };
    }

    if would_be_degenerate(&leaf.template, &positions_widened) {
        let current_template = format_template(&leaf.template);
        let mut new_template_tokens = leaf.template.clone();
        apply_widening(&mut new_template_tokens, &positions_widened);
        let would_be_template = format_template(&new_template_tokens);
        let would_be_positions = positions_to_u16(&positions_widened);
        return AttachPlan::Rejected {
            template_id: leaf.template_id,
            version: leaf.template_version,
            current_template,
            would_be_template,
            would_be_positions,
        };
    }

    // Widening path. PR-B-1 invariant: every Fixed token in a
    // leaf is a literal (mask-emit positions enter as Wildcard),
    // so the pre-widen template doesn't need to be snapshot for
    // slot seeding — the seed is always `{Str, line_type}`.
    let template_id = leaf.template_id;
    let old_version = leaf.template_version;
    let old_template_str = format_template(&leaf.template);
    let positions_u16 = positions_to_u16(&positions_widened);

    apply_widening(&mut leaf.template, &positions_widened);
    update_slot_types_on_widening(
        &mut leaf.slot_types,
        &leaf.template,
        line_wildcard_positions,
        line_typed_params,
        &positions_widened,
    );
    let version_after_widen = old_version
        .checked_add(1)
        .expect("template_version overflow: 2^32 widenings on one leaf is implausible");
    leaf.template_version = version_after_widen;
    let template_after_widen = format_template(&leaf.template);

    let mut events: Vec<TemplateChange> = Vec::with_capacity(2);
    events.push(TemplateChange::Widened {
        old_version,
        new_version: version_after_widen,
        old_template: old_template_str,
        new_template: template_after_widen.clone(),
        positions_widened: positions_u16,
    });

    // Pre-existing wildcards may also see a new ParamType from
    // this same line (RFC §6.2: a single attach can trigger both
    // widening and type-expansion; events emit in this order).
    let expansions = collect_type_expansions(
        &leaf.template,
        line_wildcard_positions,
        line_typed_params,
        &leaf.slot_types,
        &positions_widened,
    );
    let final_version = if expansions.is_empty() {
        version_after_widen
    } else {
        apply_type_expansions(&mut leaf.slot_types, &expansions);
        let version_after_expand = version_after_widen
            .checked_add(1)
            .expect("template_version overflow: 2^32 expansions on one leaf is implausible");
        leaf.template_version = version_after_expand;
        events.push(TemplateChange::TypeExpanded {
            old_version: version_after_widen,
            new_version: version_after_expand,
            old_template: template_after_widen.clone(),
            new_template: template_after_widen,
            slots_expanded: expansions,
        });
        version_after_expand
    };

    // Build params aligned to the post-widen template's wildcard
    // slots. §6.6 reconstruction reads against this alignment.
    // §6.5 cap applied per-slot inside `build_record_params`.
    let params = build_record_params(
        &leaf.template,
        masked_strs,
        line_wildcard_positions,
        line_typed_params,
        byte_limit,
    );

    AttachPlan::Mutated {
        template_id,
        events,
        final_version,
        params,
    }
}

/// One leaf considered as the best match in RFC §6.2 step 4.
/// `leaf_idx` is the index into `parent.leaves`; the
/// `template_id` is read off the leaf in Phase 2 (no need to
/// duplicate it on the candidate).
#[derive(Debug, Clone, Copy)]
pub(super) struct Candidate {
    pub(super) leaf_idx: usize,
    pub(super) similarity: f32,
}

/// Token positions where the leaf's template is `Fixed(_)` and
/// the candidate line has a different value. Per RFC §6.2 step
/// 5, these are exactly the positions that would become `<*>` if
/// the line attached to the leaf.
///
/// Returns `usize` positions; `MinerCluster::ingest_string` has
/// already capped `line.len()` at `u16::MAX`, so a downstream
/// `u16` conversion at audit-construction time is infallible.
/// Using `usize` here avoids the silent-drop hazard a `u16` return
/// type carried (a missed mismatch position would have produced
/// an empty `positions_widened` → clean attach → silent merge).
pub(super) fn find_widening_positions(
    line: &[&str],
    template: &[OwnedToken],
    line_wildcard_positions: &[usize],
) -> Vec<usize> {
    debug_assert_eq!(line.len(), template.len());
    line.iter()
        .zip(template.iter())
        .enumerate()
        .filter_map(|(i, (l, t))| match t {
            OwnedToken::Fixed(s) => {
                // Symmetric with `sim_seq_owned`'s Fixed-match
                // rule: a leaf `Fixed` matches `line[i]` only when
                // the strings agree AND the line at `i` is *not*
                // a mask-emit. The literal-tag collision
                // (`Fixed("<NUM>")` ≡ a literal `<NUM>` user
                // input, line at `i` is a real numeric → masked
                // string also `"<NUM>"`) must widen so the line's
                // typed value lands in `params` rather than being
                // silently absorbed by string equality.
                let line_is_mask_emit = line_wildcard_positions.binary_search(&i).is_ok();
                if !line_is_mask_emit && s.as_str() == *l {
                    None
                } else {
                    Some(i)
                }
            }
            OwnedToken::Wildcard => None,
        })
        .collect()
}

/// RFC §6.4 degenerate-template guard. Returns `true` iff
/// applying `positions_widened` to `template` would leave the
/// template with zero `OwnedToken::Fixed(_)` positions.
///
/// `positions_widened` is sorted ascending by construction
/// ([`find_widening_positions`] walks indices left-to-right), so
/// we lockstep-walk both sequences in `O(N + M)` with no
/// allocation. The previous `.contains()`-inside-`.all()` shape
/// was `O(N · M)`.
pub(super) fn would_be_degenerate(template: &[OwnedToken], positions_widened: &[usize]) -> bool {
    debug_assert!(
        positions_widened.windows(2).all(|w| w[0] < w[1]),
        "positions_widened must be sorted ascending (find_widening_positions invariant)",
    );
    let mut widen_iter = positions_widened.iter().copied();
    let mut next_widen = widen_iter.next();
    for (i, tok) in template.iter().enumerate() {
        match tok {
            OwnedToken::Wildcard => {} // already wildcard, doesn't contribute
            OwnedToken::Fixed(_) => {
                if next_widen == Some(i) {
                    // About to become wildcard via this widening.
                    next_widen = widen_iter.next();
                } else {
                    // A Fixed token survives this widening →
                    // not degenerate.
                    return false;
                }
            }
        }
    }
    true
}

/// Replace `Fixed` tokens at the given positions with
/// `Wildcard`, in place. Positions that are already `Wildcard`
/// are no-ops; positions not in the list are unchanged.
pub(super) fn apply_widening(template: &mut [OwnedToken], positions: &[usize]) {
    for &pos in positions {
        if pos < template.len() {
            template[pos] = OwnedToken::Wildcard;
        }
    }
}

/// Convert `usize` positions to the `Vec<u16>` shape RFC §6.4
/// requires for `AuditEvent` payloads. Infallible: callers must
/// have already enforced the line-length cap so every position
/// fits.
pub(super) fn positions_to_u16(positions: &[usize]) -> Vec<u16> {
    positions
        .iter()
        .map(|&p| {
            u16::try_from(p)
                .expect("line length capped at u16::MAX in ingest_string; positions fit")
        })
        .collect()
}
