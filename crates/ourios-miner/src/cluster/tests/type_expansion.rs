use super::*;

// ---------- §6.4 type expansion (PR-B-0) ----------

#[test]
fn literal_widening_seeds_slot_types_with_str_for_pre_widen_and_line() {
    // A literal-vs-literal widening at a position that wasn't
    // previously a wildcard. Under PR-B-1 the fresh leaf also
    // carries a Wildcard at position 1 (the `<NUM>` from the
    // mask emit at creation), so after widening position 3
    // there are TWO wildcard slots: ordinal 0 = the mask-emit
    // wildcard (slot_types = {Num}), ordinal 1 = the literal
    // widening (slot_types = {Str}).
    //
    // PR-B-1 locking-test update (was
    // `literal_widening_seeds_slot_types_with_str_for_both_
    // observations` under PR-B-0, when fresh leaves had no
    // wildcards yet). The contract being pinned now is:
    //   - literal widening produces exactly one
    //     `TemplateWidened` event (no type expansion at
    //     position 1 — the existing `<NUM>` slot already
    //     contains Num, and the second line's `<NUM>` doesn't
    //     trigger an expansion).
    //   - The newly-widened slot at ordinal 1 contains {Str}
    //     (the pre-widen literal "in" and the triggering "out"
    //     both classify as Str).
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let _ = cluster.ingest(&string_record(&t, "user 42 logged out"));

    let events = drain_changes(&sink);
    assert_eq!(events.len(), 1, "literal widening: one event only");
    assert!(matches!(
        events[0].payload,
        AuditPayload::Template {
            change: TemplateChange::Widened { .. },
            ..
        }
    ));

    let templates = cluster.templates_for(&t);
    assert_eq!(templates.len(), 1);
    assert_eq!(
        templates[0].slot_types.len(),
        2,
        "two wildcard slots: ordinal 0 from mask emit, ordinal 1 from widening",
    );
    let ordinal_0: Vec<_> = templates[0].slot_types[0].iter().collect();
    assert_eq!(ordinal_0, vec![ParamType::Num]);
    let ordinal_1: Vec<_> = templates[0].slot_types[1].iter().collect();
    assert_eq!(ordinal_1, vec![ParamType::Str]);
}

#[test]
fn mask_tag_transition_at_typed_wildcard_emits_type_expanded() {
    // CLAUDE.md §3.1 regression for mask-tag type transitions
    // at a wildcard slot.
    //
    // Setup: the fresh leaf carries a Wildcard at position 2
    // (from the `<NUM>` mask emit at creation) with
    // `slot_types[0] = {Num}`. The second line lands `<UUID>`
    // at the same position. Under PR-B-1 the leaf position is
    // already a Wildcard, so `find_widening_positions` returns
    // empty (no Fixed mismatch); the §3.1 signal moves to the
    // type-expansion path instead, which fires
    // `TemplateTypeExpanded` and grows the slot's type set.
    //
    // PR-B-1 locking-test update (was
    // `fixed_mask_tag_widening_captures_both_param_types_in_
    // slot` under PR-B-0, when fresh leaves stored
    // `Fixed("<NUM>")` and the same case fired
    // `TemplateWidened`). The §3.1 invariant — every mask-tag
    // transition at a tree-routed slot must produce an audit
    // signal — is preserved end-to-end; only the *event kind*
    // changes.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    // Prefix ["user", "logged"] shared; mask-tag divergence
    // at position 2 (Num vs Uuid).
    let _ = cluster.ingest(&string_record(&t, "user logged 42 in"));
    let _ = cluster.ingest(&string_record(
        &t,
        "user logged 550e8400-e29b-41d4-a716-446655440000 in",
    ));

    let events = drain_changes(&sink);
    assert_eq!(events.len(), 1, "single TemplateTypeExpanded, no widening");
    let AuditPayload::Template {
        change:
            TemplateChange::TypeExpanded {
                old_version,
                new_version,
                slots_expanded,
                ..
            },
        ..
    } = &events[0].payload
    else {
        panic!(
            "expected Template/TypeExpanded, got {:?}",
            events[0].payload
        );
    };
    assert_eq!((*old_version, *new_version), (1, 2));
    assert_eq!(slots_expanded.len(), 1);
    assert_eq!(slots_expanded[0].slot_index, 0);
    assert_eq!(slots_expanded[0].added_types, vec![ParamType::Uuid]);

    let templates = cluster.templates_for(&t);
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].slot_types.len(), 1);
    let types: Vec<_> = templates[0].slot_types[0].iter().collect();
    assert_eq!(
        types,
        vec![ParamType::Uuid, ParamType::Num],
        "slot must record both Num (from creation) and Uuid (from this attach)",
    );
}

#[test]
fn clean_attach_at_typed_wildcard_with_known_type_emits_no_audit() {
    // After a widening that seeds slot_types[0] = {Num, Uuid}
    // (one Num line, one Uuid line), a third line with `<NUM>`
    // at the same position attaches cleanly with no events.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "user logged 42 in"));
    let _ = cluster.ingest(&string_record(
        &t,
        "user logged 550e8400-e29b-41d4-a716-446655440000 in",
    ));
    let _ = sink.drain();

    // L3 — Num at position 2 (the wildcard) is already in the
    // slot's set, so no expansion event fires.
    let _ = cluster.ingest(&string_record(&t, "user logged 99 in"));

    assert!(
        drain_changes(&sink).is_empty(),
        "known type at typed wildcard must not emit",
    );
    let templates = cluster.templates_for(&t);
    assert_eq!(
        templates[0].template_version, 2,
        "version stays at 2 — no expansion",
    );
}

#[test]
fn clean_attach_at_typed_wildcard_with_new_type_emits_type_expanded() {
    // Seed slot_types[0] = {Str} via literal widening
    // ("in"/"out"), then ingest a `<NUM>` at the same position
    // — Num is not in {Str}, so TemplateTypeExpanded fires.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    // 5-token lines so the single-position widening keeps
    // sim_seq at 4/5 = 0.8 ≥ default threshold 0.7. Shorter
    // lines drop the similarity below threshold and split
    // into a fresh leaf via the Lossy zone (RFC §6.2 step 5b).
    let _ = cluster.ingest(&string_record(&t, "user logged at hour in"));
    let _ = cluster.ingest(&string_record(&t, "user logged at hour out"));
    let _ = sink.drain();

    // "13" masks to "<NUM>"; it lands at position 4 where the
    // leaf has a Wildcard with slot_types[0] = {Str}.
    let _ = cluster.ingest(&string_record(&t, "user logged at hour 13"));

    let events = drain_changes(&sink);
    assert_eq!(events.len(), 1, "exactly one TemplateTypeExpanded");
    let AuditPayload::Template {
        change:
            TemplateChange::TypeExpanded {
                old_version,
                new_version,
                slots_expanded,
                ..
            },
        ..
    } = &events[0].payload
    else {
        panic!(
            "expected Template/TypeExpanded, got {:?}",
            events[0].payload
        );
    };
    assert_eq!((*old_version, *new_version), (2, 3));
    assert_eq!(slots_expanded.len(), 1);
    assert_eq!(slots_expanded[0].slot_index, 0);
    assert_eq!(slots_expanded[0].added_types, vec![ParamType::Num]);

    let templates = cluster.templates_for(&t);
    let types: Vec<_> = templates[0].slot_types[0].iter().collect();
    assert_eq!(types, vec![ParamType::Num, ParamType::Str]);
}

#[test]
fn type_expansion_only_attach_counts_toward_merges_total() {
    // RFC §6.4 — `merges_total` counts both `TemplateWidened`
    // and `TemplateTypeExpanded` (see
    // `TemplateChange::counts_as_merge`). A pure type-expansion
    // attach must therefore bump the counter.
    let (mut cluster, _sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "user logged at hour in"));
    let _ = cluster.ingest(&string_record(&t, "user logged at hour out"));
    let merges_after_widen = cluster.merges_total();

    let _ = cluster.ingest(&string_record(&t, "user logged at hour 13"));
    assert_eq!(
        cluster.merges_total(),
        merges_after_widen + 1,
        "type-expansion-only attach must bump merges_total",
    );
}

#[test]
fn combined_widening_and_type_expansion_emits_two_events_in_order() {
    // RFC §6.2: a single attach can trigger BOTH structural
    // widening (Fixed mismatch) AND type expansion (new
    // ParamType at a pre-existing wildcard). In that case
    // template_version increments twice and two events emit
    // in widening-then-expansion order.
    //
    // Setup: leaf with one pre-existing Wildcard whose
    // slot_types = {Str}, plus a Fixed literal at another
    // position. The triggering line widens the literal
    // (literal-vs-literal Fixed mismatch) AND brings a `<NUM>`
    // to the pre-existing wildcard.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    // L1: fresh 5-token leaf [Fixed("user"), Fixed("logged"),
    //     Fixed("at"), Fixed("hour"), Fixed("NOW")], slots=[].
    //     Prefix ["user", "logged"] is shared.
    let _ = cluster.ingest(&string_record(&t, "user logged at hour NOW"));
    // L2: literal "NOW" → "LATER" widens position 4. sim_seq
    //     = 4/5 = 0.8 ≥ threshold. slot_types = [{Str}], v=2.
    let _ = cluster.ingest(&string_record(&t, "user logged at hour LATER"));
    let _ = sink.drain();

    // L3: Fixed mismatch at position 3 ("hour" → "minute")
    //     AND position 4's pre-existing wildcard sees "<NUM>"
    //     from "13". sim_seq = 4/5 = 0.8 → Clean. After
    //     widening pos 3 the slot ordinals are: position 3 →
    //     ordinal 0 (fresh, slot_types=[{Str}]), position 4
    //     → ordinal 1 (existing, slot_types was [{Str}]).
    //     Expansion fires at ordinal 1, adding Num.
    let _ = cluster.ingest(&string_record(&t, "user logged at minute 13"));

    let events = drain_changes(&sink);
    assert_eq!(events.len(), 2, "combined widening + type expansion");
    let AuditPayload::Template {
        change:
            TemplateChange::Widened {
                old_version: w_old,
                new_version: w_new,
                positions_widened,
                ..
            },
        ..
    } = &events[0].payload
    else {
        panic!(
            "event 0 must be Template/Widened (widening fires before expansion), got {:?}",
            events[0].payload,
        );
    };
    assert_eq!((*w_old, *w_new), (2, 3));
    assert_eq!(*positions_widened, vec![3]);

    let AuditPayload::Template {
        change:
            TemplateChange::TypeExpanded {
                old_version: e_old,
                new_version: e_new,
                slots_expanded,
                ..
            },
        ..
    } = &events[1].payload
    else {
        panic!(
            "event 1 must be Template/TypeExpanded, got {:?}",
            events[1].payload,
        );
    };
    assert_eq!((*e_old, *e_new), (3, 4));
    // Post-widen template: [Fixed, Fixed, Fixed, Wildcard,
    // Wildcard]. The freshly-widened slot at position 3 is
    // ordinal 0; the pre-existing slot at position 4 is
    // ordinal 1 — that's the one expanding.
    assert_eq!(slots_expanded.len(), 1);
    assert_eq!(slots_expanded[0].slot_index, 1);
    assert_eq!(slots_expanded[0].added_types, vec![ParamType::Num]);

    let templates = cluster.templates_for(&t);
    assert_eq!(
        templates[0].template_version, 4,
        "version is 4 after both bumps",
    );
    assert_eq!(templates[0].slot_types.len(), 2, "two wildcard slots");
}

#[test]
fn slot_types_are_aligned_by_wildcard_ordinal_not_template_position() {
    // The leaf's `slot_types[k]` is the type set for the k-th
    // Wildcard from the left (ordinal), not for template
    // position k. Pin the invariant by widening positions
    // out-of-order across multiple attaches and checking the
    // resulting alignment.
    let (mut cluster, _sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    // L1: 5-token leaf [Fixed("user"), Fixed("logged"),
    //     Fixed("in"), Fixed("fast"), Fixed("NOW")], v=1.
    //     Prefix ["user", "logged"] is shared.
    let _ = cluster.ingest(&string_record(&t, "user logged in fast NOW"));
    // L2: widen position 3 ("fast" → "slow"). sim_seq = 4/5
    //     ≥ threshold. slot_types = [{Str}] (one wildcard at
    //     ordinal 0).
    let _ = cluster.ingest(&string_record(&t, "user logged in slow NOW"));
    // L3: widen position 2 ("in" → "out"). Position 3 is a
    //     pre-existing Wildcard matching "slow". sim_seq =
    //     4/5 ≥ threshold. The new wildcard at template
    //     position 2 is inserted at ordinal 0 (it sits left
    //     of position 3's existing wildcard, now ordinal 1),
    //     so slot_types = [{Str (new at pos 2)}, {Str (old
    //     at pos 3)}].
    let _ = cluster.ingest(&string_record(&t, "user logged out slow NOW"));

    let templates = cluster.templates_for(&t);
    assert_eq!(templates[0].slot_types.len(), 2);
    // Both slots are Str-only (literal widenings).
    for (i, st) in templates[0].slot_types.iter().enumerate() {
        let types: Vec<_> = st.iter().collect();
        assert_eq!(
            types,
            vec![ParamType::Str],
            "slot {i}: literal widening seeds only Str",
        );
    }
}

#[test]
fn silent_merge_across_mask_tag_types_is_audited_not_silent() {
    // Regression for the CLAUDE.md §3.1 violation that closed
    // PR #32. Two lines differing only by mask-tag type at a
    // position beyond `prefix_depth` (default 2) MUST produce
    // an audit signal, not a silent merge.
    //
    // - Line A: "GET /home 42 ok" — masks <NUM> at position 2.
    // - Line B: "GET /home <UUID-string> ok" — masks <UUID>.
    //
    // Under PR-B-1's leaf model the §3.1 audit signal moves
    // from `TemplateWidened` to `TemplateTypeExpanded`:
    // masked positions enter the leaf as `Wildcard` from
    // creation, so the second line doesn't trigger a Fixed
    // mismatch; the divergence surfaces as the slot's type
    // set growing to {Num, Uuid}. The §3.1 contract — every
    // mask-tag transition at a tree-routed slot audits — is
    // preserved; the event kind changes.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "GET /home 42 ok"));
    let _ = cluster.ingest(&string_record(
        &t,
        "GET /home 550e8400-e29b-41d4-a716-446655440000 ok",
    ));

    let events = drain_changes(&sink);
    assert!(
        !events.is_empty(),
        "§3.1: mask-tag type change at a tree-routed wildcard slot must audit",
    );
    let AuditPayload::Template {
        change: TemplateChange::TypeExpanded { slots_expanded, .. },
        ..
    } = &events[0].payload
    else {
        panic!(
            "expected Template/TypeExpanded, got {:?}",
            events[0].payload
        );
    };
    assert_eq!(slots_expanded.len(), 1);
    assert_eq!(slots_expanded[0].slot_index, 0);
    assert_eq!(slots_expanded[0].added_types, vec![ParamType::Uuid]);

    // Confirm the slot's type set captures the divergence so a
    // *third* mask-tag type at the same position would emit
    // another TemplateTypeExpanded.
    let templates = cluster.templates_for(&t);
    let types: Vec<_> = templates[0].slot_types[0].iter().collect();
    assert_eq!(types, vec![ParamType::Uuid, ParamType::Num]);
}

#[test]
fn literal_mask_tag_token_in_line_classifies_as_str_not_num() {
    // Regression for PR #33's review feedback. If a log line
    // literally contains the token `"<NUM>"` (e.g., a
    // placeholder a developer wrote into the message),
    // `mask()` does NOT classify it (the digit rule doesn't
    // fire on non-digit strings). The cluster's per-position
    // type classification therefore reports `Str` for that
    // position, NOT `Num`.
    //
    // Setup at position 3: literal widening seeds
    // slot_types[0] = {Str}. Then a third line brings the
    // literal `"<NUM>"` at the same position. The classifier
    // must read mask's `wildcard_positions` (empty for this
    // position, since the rule didn't fire) and conclude
    // `Str`, which is already in the slot's set — no audit
    // event. The pre-fix code would have inferred `Num` from
    // the masked-token string content and incorrectly fired
    // `TemplateTypeExpanded`, corrupting slot_types.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "user logged at hour in"));
    let _ = cluster.ingest(&string_record(&t, "user logged at hour out"));
    let _ = sink.drain();

    // The third line's position-4 token is the literal
    // string "<NUM>". mask() leaves it alone (not all-digits).
    let _ = cluster.ingest(&string_record(&t, "user logged at hour <NUM>"));

    assert!(
        drain_changes(&sink).is_empty(),
        "literal `<NUM>` must be Str (already in slot's set), not a spurious Num expansion",
    );
    let templates = cluster.templates_for(&t);
    assert_eq!(
        templates[0].template_version, 2,
        "no version bump: the literal `<NUM>` did not introduce a new type",
    );
    let types: Vec<_> = templates[0].slot_types[0].iter().collect();
    assert_eq!(
        types,
        vec![ParamType::Str],
        "slot_types stays at {{Str}} — Num must not leak in from a literal-tag token",
    );
}

#[test]
fn literal_mask_tag_in_leaf_vs_real_mask_emit_on_line_does_not_silently_merge() {
    // Symmetric regression for the PR #35 review concern: a
    // leaf with `Fixed("<NUM>")` (because the *first* line
    // carried the literal token `<NUM>`) MUST NOT merge with a
    // later line that puts a real numeric value at the same
    // position. The masked-line token at that position is also
    // `"<NUM>"`, so a naive string-equality match in
    // `sim_seq_owned` / `find_widening_positions` would mark
    // it as a Fixed match — silently merging the two log
    // shapes AND dropping the numeric's value from `params`
    // (§3.1 + §3.3 violation; reconstruct would render
    // `<NUM>` literally instead of recovering `42`).
    //
    // `line_wildcard_positions` plumbed through sim_seq +
    // find_widening fixes both: the line at position 1 is a
    // mask emit (in `line_wildcard_positions`), so it does
    // NOT match the leaf's literal `Fixed("<NUM>")`. sim_seq
    // returns 2/3 ≈ 0.667 < 0.7 threshold → Lossy zone →
    // fresh leaf. Two templates result, body retained on the
    // Lossy line.
    let (mut cluster, audit_sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    // L1: literal `<NUM>` token. mask() doesn't classify it.
    let raw_l1 = "value <NUM> ok";
    let id1 = cluster.ingest(&string_record(&t, raw_l1));
    // L2: real numeric at the same position. mask() emits
    // `<NUM>` here.
    let raw_l2 = "value 42 ok";
    let id2 = cluster.ingest(&string_record(&t, raw_l2));

    // Distinct template ids — no silent merge.
    assert_ne!(
        id1, id2,
        "leaf `Fixed(\"<NUM>\")` (literal) must not absorb a real mask-emit `<NUM>`",
    );
    assert_eq!(cluster.template_count(&t), 2);
    // No widening fired (the Lossy zone created a new leaf rather than
    // widening). The two leaf creations each emit a `Created` event
    // (RFC 0017 §3.1), which `drain_changes` filters out — so there are
    // no widening / type-expansion / rejection events.
    assert!(drain_changes(&audit_sink).is_empty());
    // The §6.3 lossy zone bumped body_retentions for L2 (and
    // retained its body on the emitted record).
    assert_eq!(cluster.body_retentions_total(), 1);
}

#[test]
fn existing_wildcard_receives_literal_emits_str_param_and_reconstructs() {
    // PR-B-2 STR-fallback regression for the "existing wildcard
    // receives a literal observation" path (distinct from the
    // freshly-widened-literal-slot case covered by H7.4).
    //
    // Setup: a prior `<NUM>` mask emit creates a Wildcard at
    // position 2 with `slot_types[0] = {Num}`. A later line at
    // the same position carries a literal whose mask does not
    // classify (e.g. "abc-def-1234" — neither digits nor UUID
    // nor IPv4). sim_seq still matches (Wildcard matches
    // anything), so the attach is Clean — and `build_record_
    // params` must emit `{Str, "abc-def-1234"}` for the slot
    // so reconstruct round-trips the literal verbatim.
    //
    // The pre-PR-B-2 code (params_from_mask) would have emitted
    // params=[] for this attach because mask emitted nothing —
    // reconstruct would have produced no bytes at the wildcard
    // position. PR-B-2's `build_record_params` walks the
    // leaf's wildcards and inserts the STR fallback.
    //
    // **Scope note.** This test exercises a wildcard at
    // template position **2** — that is, *beyond* the default
    // `prefix_depth = 2`, so both ingests share the same
    // tree parent (positions 0–1 = `["user", "logged"]` for
    // both). The STR-fallback path is structurally
    // unreachable for wildcards INSIDE the prefix depth: the
    // tree partitions by the concrete masked token at each
    // prefix level, so a line whose prefix masks to a
    // different concrete token (e.g. literal `abc` vs mask-
    // emitted `<NUM>` at position 1 under default
    // `prefix_depth = 2`) ends up in a different parent and
    // finds no candidate to attach to. That is a property of
    // the Drain tree's prefix-routing scheme (paper §3.2,
    // RFC 0001 §6.1), not a bug in PR-B-2's STR fallback;
    // future work to make wildcard slots reachable from
    // diverging prefix tokens (multi-bucket lookup or
    // wildcard-aware re-bucketing) is its own RFC-level
    // change. The test deliberately stays inside the
    // structurally-reachable case.
    let (mut cluster, _audit, records) = cluster_with_observable_sinks();
    let t = TenantId::new("tenant-x");
    let make = |raw: &str| string_record(&t, raw);

    // L1: creates the wildcard at position 2 with
    // slot_types[0] = {Num} (mask emit).
    let _ = cluster.ingest(&make("user logged 42 in"));
    let l1_emit = records.drain();
    assert_eq!(l1_emit.len(), 1);

    // L2: literal at position 2 lands on the existing
    // wildcard. {Str} expands the slot's type set → emits
    // TemplateTypeExpanded, but the record's params must
    // carry the literal so reconstruct works.
    let raw_l2 = "user logged abc-def-1234 in";
    let _ = cluster.ingest(&make(raw_l2));

    let l2_emit = records.drain();
    assert_eq!(l2_emit.len(), 1);
    let rec = &l2_emit[0];

    // params has exactly one entry for the one wildcard slot,
    // and it's a STR fallback carrying the literal verbatim.
    assert_eq!(rec.params.len(), 1, "one wildcard → one param");
    assert_eq!(
        rec.params[0].type_tag,
        ParamType::Str,
        "literal at an existing wildcard → STR fallback",
    );
    assert_eq!(rec.params[0].value, "abc-def-1234");

    // End-to-end: reconstruct round-trips the original bytes.
    let snapshots = cluster.templates_for(&t);
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        crate::reconstruct::reconstruct(rec, &snapshots[0].template),
        raw_l2.as_bytes().to_vec(),
        "STR-fallback alignment must let reconstruct recover the literal byte-for-byte",
    );
}
