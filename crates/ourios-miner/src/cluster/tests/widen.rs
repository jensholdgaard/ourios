use super::*;
use crate::cluster::plan::{apply_widening, find_widening_positions, would_be_degenerate};

// ---------- widen behaviour (this PR's main story) ----------

/// Replaces the pre-widen `ingest_creates_separate_leaves_for_
/// near_match_under_same_parent` test, which locked in the
/// no-widening contract. With `sim_seq >= threshold` widening
/// now active, the two lines that previously created two
/// distinct leaves collapse to a single template with a
/// `<*>` at position 3.
///
/// Per `CLAUDE.md` §6.2 ("Tests are specifications") this
/// contract change is explicit, not silent: the old test
/// asserted *distinct ids*, the new one asserts *same id +
/// audit event*. PR review must acknowledge the swap.
#[test]
fn near_match_under_same_parent_widens_into_single_template() {
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    // Both lines mask to length-6 templates differing at
    // position 3 ("in" vs "out"). sim_seq = 5/6 ≈ 0.833 ≥
    // the default 0.7 threshold, so the second line widens
    // the first leaf rather than creating a new one.
    let id_in = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));
    let id_out = cluster.ingest(&string_record(&t, "user 42 logged out from 10.0.0.1"));

    // Same template, one widening event, count stays at 1.
    assert_eq!(id_in, id_out);
    assert_eq!(cluster.template_count(&t), 1);
    assert_eq!(cluster.merges_total(), 1);

    let events = drain_changes(&sink);
    assert_eq!(events.len(), 1);
    let AuditPayload::Template {
        template_id,
        change:
            TemplateChange::Widened {
                old_version,
                new_version,
                positions_widened,
                old_template,
                new_template,
            },
        ..
    } = &events[0].payload
    else {
        panic!("expected Template/Widened, got {:?}", events[0].payload);
    };
    assert_eq!(*template_id, id_in);
    assert_eq!(*old_version, 1);
    assert_eq!(*new_version, 2);
    assert_eq!(*positions_widened, vec![3]);
    // PR-B-1: mask-emit positions (`<NUM>` at index 1, `<IP>`
    // at index 5) enter the leaf as `Wildcard` from creation,
    // so the canonical-form template renders them as `<*>`,
    // not as the tag string. The audit-event shape is
    // unchanged — only the rendered template strings differ
    // because the type information now lives in the parallel
    // `slot_types` vector rather than encoded in the template
    // (RFC 0001 §6.6 reconstruction substitutes back via
    // `params`, not the template string).
    assert_eq!(old_template, "user <*> logged in from <*>");
    assert_eq!(new_template, "user <*> logged <*> from <*>");
}

#[test]
fn fresh_leaf_emits_created_event() {
    // RFC 0017 §3.1 overturns the former "fresh leaf emits nothing"
    // contract: leaf allocation now emits a `template_created` audit
    // event (so a read-time registry can recover v1 tokens), while
    // still NOT counting as a merge. Two distinct fresh leaves →
    // exactly two `Created` events, `merges_total` still 0.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let id_a = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let id_b = cluster.ingest(&string_record(&t, "GET /home 200"));

    assert_eq!(cluster.template_count(&t), 2);
    assert_eq!(cluster.merges_total(), 0, "creation is not a merge");

    let events = sink.drain();
    assert_eq!(events.len(), 2, "one Created event per fresh leaf");
    for (event, id) in events.iter().zip([id_a, id_b]) {
        let AuditPayload::Template {
            template_id,
            change: TemplateChange::Created { new_template },
            ..
        } = &event.payload
        else {
            panic!("expected Template/Created, got {:?}", event.payload);
        };
        assert_eq!(*template_id, id);
        assert!(
            !new_template.is_empty(),
            "creation carries the initial tokens",
        );
    }
}

#[test]
fn exact_sim_seq_match_attaches_without_widening_or_audit() {
    // A line whose mask matches an existing leaf exactly (no
    // mismatched Fixed positions, no new ParamType at any
    // existing wildcard) reuses the leaf with no version bump
    // and no audit event.
    //
    // PR-B-1 locking-test update: under the new leaf model
    // mask-emit positions enter the leaf as `Wildcard` from
    // creation (with `slot_types[0] = {Num}` for the `<NUM>`
    // at position 1). The relevant contract is therefore
    // "no version bump, no audit, slot_types unchanged" — not
    // "no wildcards in the template at all". Both lines mask
    // to the same shape (`<NUM>` at position 1), and the
    // second line's `<NUM>` is already in `slot_types[0]`'s
    // set, so type-expansion doesn't fire either. The leaf's
    // wildcard set is asserted explicitly so a future bug
    // that accidentally widened position 2 or 3 still fails.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let id1 = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let id2 = cluster.ingest(&string_record(&t, "user 17 logged in"));

    assert_eq!(id1, id2);
    assert_eq!(cluster.merges_total(), 0);
    assert!(drain_changes(&sink).is_empty());

    let templates = cluster.templates_for(&t);
    assert_eq!(templates.len(), 1);
    assert_eq!(
        templates[0].template_version, 1,
        "no version bump on same-shape clean attach",
    );
    let wildcard_positions: Vec<usize> = templates[0]
        .template
        .iter()
        .enumerate()
        .filter_map(|(i, t)| matches!(t, OwnedToken::Wildcard).then_some(i))
        .collect();
    assert_eq!(
        wildcard_positions,
        vec![1],
        "leaf's wildcard set must match the line's mask set: {:?}",
        templates[0].template,
    );
    // The slot's type set stayed at {Num} — the second `<NUM>`
    // line is already in the set, so no expansion fired.
    assert_eq!(templates[0].slot_types.len(), 1);
    let types: Vec<_> = templates[0].slot_types[0].iter().collect();
    assert_eq!(types, vec![ParamType::Num]);
}

#[test]
fn widening_increments_template_version() {
    // H5.1 — the version stamp on the leaf bumps from 1 to 2.
    // The first ingest creates the leaf at version 1; the
    // second triggers a widening that bumps to version 2.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));
    let _ = cluster.ingest(&string_record(&t, "user 42 logged out from 10.0.0.1"));

    let events = drain_changes(&sink);
    assert_eq!(events.len(), 1);
    let AuditPayload::Template {
        change:
            TemplateChange::Widened {
                old_version,
                new_version,
                ..
            },
        ..
    } = &events[0].payload
    else {
        panic!("expected Template/Widened, got {:?}", events[0].payload);
    };
    assert_eq!(*old_version, 1);
    assert_eq!(*new_version, 2);
}

#[test]
fn second_widening_at_different_position_increments_version_again() {
    // Three lines, two widening events. After the second
    // widening, version is 3.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    // L1: "user 42 alpha logged in from 10.0.0.1" — fresh leaf, v=1.
    // L2: "user 42 alpha logged out from 10.0.0.1" — widens
    //     position 4 ("in" → "out") to <*>, v=2.
    // L3: "user 42 alpha logged out to 10.0.0.1" — widens
    //     position 5 ("from" → "to") to <*>, v=3. The
    //     position-4 wildcard already matches "out", so only
    //     position 5 widens this round.
    let _ = cluster.ingest(&string_record(&t, "user 42 alpha logged in from 10.0.0.1"));
    let _ = cluster.ingest(&string_record(&t, "user 42 alpha logged out from 10.0.0.1"));
    let _ = cluster.ingest(&string_record(&t, "user 42 alpha logged out to 10.0.0.1"));

    assert_eq!(cluster.template_count(&t), 1);
    assert_eq!(cluster.merges_total(), 2);

    let events = drain_changes(&sink);
    assert_eq!(events.len(), 2);
    let AuditPayload::Template {
        change:
            TemplateChange::Widened {
                old_version: ov0,
                new_version: nv0,
                positions_widened: p0,
                ..
            },
        ..
    } = &events[0].payload
    else {
        panic!(
            "event 0: expected Template/Widened, got {:?}",
            events[0].payload
        );
    };
    assert_eq!((*ov0, *nv0, p0.clone()), (1, 2, vec![4]));
    let AuditPayload::Template {
        change:
            TemplateChange::Widened {
                old_version: ov1,
                new_version: nv1,
                positions_widened: p1,
                ..
            },
        ..
    } = &events[1].payload
    else {
        panic!(
            "event 1: expected Template/Widened, got {:?}",
            events[1].payload
        );
    };
    assert_eq!((*ov1, *nv1, p1.clone()), (2, 3, vec![5]));
}

#[test]
fn fresh_leaf_carries_wildcard_at_mask_positions_with_seeded_slot_types() {
    // PR-B-1 contract: at fresh-leaf creation, `mask()`'s
    // wildcard_positions feed directly into the leaf's
    // template (Wildcard at those positions, Fixed elsewhere)
    // and `slot_types` is seeded from `typed_params` in
    // ordinal order (one entry per masked position).
    //
    // This is the structural prerequisite for §6.6
    // reconstruction: the template Wildcard slots and the
    // `params` vector now align position-for-ordinal.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    // Mask emits at positions 1 (`<NUM>`) and 5 (`<IP>`).
    let _ = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));

    // Fresh-leaf creation emits a `Created` event now (RFC 0017 §3.1),
    // but no *widening / type-expansion* — even with non-empty
    // slot_types. `drain_changes` filters the Created event out.
    assert!(drain_changes(&sink).is_empty());

    let templates = cluster.templates_for(&t);
    assert_eq!(templates.len(), 1);
    let snap = &templates[0];

    // Template shape: Wildcard at mask positions, Fixed
    // elsewhere.
    assert_eq!(snap.template.len(), 6);
    assert!(matches!(snap.template[0], OwnedToken::Fixed(ref s) if s == "user"));
    assert!(matches!(snap.template[1], OwnedToken::Wildcard));
    assert!(matches!(snap.template[2], OwnedToken::Fixed(ref s) if s == "logged"));
    assert!(matches!(snap.template[3], OwnedToken::Fixed(ref s) if s == "in"));
    assert!(matches!(snap.template[4], OwnedToken::Fixed(ref s) if s == "from"));
    assert!(matches!(snap.template[5], OwnedToken::Wildcard));

    // slot_types seeded from typed_params in ordinal order.
    assert_eq!(snap.slot_types.len(), 2);
    assert_eq!(
        snap.slot_types[0].iter().collect::<Vec<_>>(),
        vec![ParamType::Num],
    );
    assert_eq!(
        snap.slot_types[1].iter().collect::<Vec<_>>(),
        vec![ParamType::Ip],
    );
}

#[test]
fn audit_event_carries_triggering_line_hash_and_sample() {
    // RFC §6.4 fields: triggering_line_hash (truncated blake3)
    // and triggering_line_sample (first 256 B at char
    // boundary) must reflect the line that triggered the
    // widening — i.e., L2, not L1.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "user 42 logged in from 10.0.0.1"));
    let l2 = "user 42 logged out from 10.0.0.1";
    let _ = cluster.ingest(&string_record(&t, l2));

    let events = drain_changes(&sink);
    assert_eq!(events.len(), 1);
    let AuditPayload::Template {
        triggering_line_hash,
        triggering_line_sample,
        ..
    } = &events[0].payload
    else {
        panic!("expected Template, got {:?}", events[0].payload);
    };
    assert_eq!(*triggering_line_hash, hash_triggering_line(l2.as_bytes()));
    assert_eq!(triggering_line_sample.as_deref(), Some(l2));
}

#[test]
fn below_threshold_creates_separate_leaf_no_widening() {
    // The H1.1 invariant: lines with `sim_seq < threshold`
    // remain distinct templates. "user logged in" vs
    // "user logged out" mask to two length-3 templates
    // differing at position 2; sim_seq = 2/3 ≈ 0.667 <
    // default 0.7, so no widening.
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let id1 = cluster.ingest(&string_record(&t, "user logged in"));
    let id2 = cluster.ingest(&string_record(&t, "user logged out"));

    assert_ne!(id1, id2);
    assert_eq!(cluster.template_count(&t), 2);
    assert_eq!(cluster.merges_total(), 0);
    assert!(drain_changes(&sink).is_empty());
}

#[test]
fn degenerate_widening_is_rejected_and_emits_rejection_event() {
    // RFC0001.2 — a widening that would leave zero Fixed
    // tokens is rejected:
    //  - returns NO_TEMPLATE
    //  - emits TemplateWideningRejectedDegenerate
    //  - increments parse_failures_total (not merges_total)
    //  - increments body_retentions_total — §6.4 "treated as
    //    a parse failure ... retain body"
    //  - does NOT bump template_version or modify the leaf
    //
    // Construction notes:
    //
    // - We use a `with_prefix_depth(0)` cluster so all length-3
    //   lines share one leaf list. The default prefix-tree
    //   shape partitions on the first two tokens, which makes
    //   the degenerate path structurally unreachable (the
    //   prefix-path tokens are always Fixed in any reachable
    //   leaf).
    //
    // - Threshold of 0.3 so a 1/3-similar attach still
    //   triggers widening instead of creating a fresh leaf.
    //
    //   L1 = ["alpha", "beta", "gamma"] — fresh leaf v=1.
    //   L2 = ["alpha", "xxx", "yyy"] — sim with L1 = 1/3 ≥ 0.3
    //        → widens positions 1, 2 → template
    //        ["alpha", <*>, <*>], v=2. 1 Fixed left, NOT
    //        degenerate.
    //   L3 = ["zzz", "qqq", "rrr"] — sim with the widened
    //        template = 2/3 (the two wildcards match) ≥ 0.3
    //        → would widen position 0 (the last Fixed)
    //        → fully degenerate → rejected.
    let config = MinerConfig::try_new(0.3, 256).expect("valid config");
    let sink = SharedAuditSink::new();
    let mut cluster =
        MinerCluster::with_audit_sink(config, Box::new(sink.clone())).with_prefix_depth(0);
    let t = TenantId::new("tenant-x");

    // Construct records bypassing masking by using single-letter
    // tokens that no mask rule fires on.
    let l1 = cluster.ingest(&string_record(&t, "alpha beta gamma"));
    let _l2 = cluster.ingest(&string_record(&t, "alpha xxx yyy"));
    let l3 = cluster.ingest(&string_record(&t, "zzz qqq rrr"));

    // L1 created the leaf, L2 widened it, L3 was rejected.
    assert_ne!(l1, NO_TEMPLATE);
    assert_eq!(l3, NO_TEMPLATE);
    assert_eq!(cluster.merges_total(), 1, "only L2's widening counts");
    assert_eq!(cluster.parse_failures_total(), 1, "L3 was rejected");
    assert_eq!(
        cluster.body_retentions_total(),
        1,
        "§6.4 says degenerate-rejected lines retain body",
    );

    let events = drain_changes(&sink);
    assert_eq!(events.len(), 2);
    assert!(
        matches!(
            events[0].payload,
            AuditPayload::Template {
                change: TemplateChange::Widened { .. },
                ..
            }
        ),
        "event 0: expected Template/Widened, got {:?}",
        events[0].payload,
    );
    // Rejection variant carries no version bump and surfaces
    // the would-be template the operator was protected from.
    let AuditPayload::Template {
        change: TemplateChange::RejectedDegenerate {
            would_be_template, ..
        },
        ..
    } = &events[1].payload
    else {
        panic!(
            "event 1: expected Template/RejectedDegenerate, got {:?}",
            events[1].payload,
        );
    };
    assert_eq!(would_be_template, "<*> <*> <*>");

    // Leaf state was not mutated by the rejection — still has
    // its post-widening template (1 Fixed at position 0).
    let templates = cluster.templates_for(&t);
    assert_eq!(templates.len(), 1);
    let leaf_template = &templates[0].template;
    assert_eq!(leaf_template.len(), 3);
    assert!(matches!(leaf_template[0], OwnedToken::Fixed(ref s) if s == "alpha"));
    assert!(matches!(leaf_template[1], OwnedToken::Wildcard));
    assert!(matches!(leaf_template[2], OwnedToken::Wildcard));
}

#[test]
fn ingest_returns_no_template_sentinel_for_empty_string_body() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let id_empty = cluster.ingest(&string_record(&t, ""));
    let id_blank = cluster.ingest(&string_record(&t, "   \t\n"));

    assert_eq!(id_empty, NO_TEMPLATE);
    assert_eq!(id_blank, NO_TEMPLATE);
    assert_eq!(cluster.template_count(&t), 0);
    assert_eq!(
        cluster.body_retentions_total(),
        2,
        "empty input is still a parse failure that retains body \
         (RFC §6.3: every parse-failure path bumps both counters)",
    );
    assert_eq!(
        cluster.parse_failures_total(),
        2,
        "empty input is the parse-failure floor's simplest case",
    );
}

#[test]
fn template_count_grows_with_each_distinct_template() {
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let _ = cluster.ingest(&string_record(&t, "user 42 logged in"));
    let _ = cluster.ingest(&string_record(&t, "user 17 logged in"));
    let _ = cluster.ingest(&string_record(&t, "GET /home 200"));

    let cached = cluster.template_count(&t);
    assert_eq!(cached, 2);
}

#[test]
fn best_candidate_selection_picks_highest_similarity_in_parent_leaf_list() {
    // Two leaves under the same parent, one matches the line
    // better than the other. The miner must pick the higher-
    // similarity leaf for the widen target — not the first or
    // the one it happens to encounter.
    //
    // L1 = "alpha beta gamma delta epsilon" (length 5)
    // L2 = "alpha beta gamma zeta epsilon" (length 5, also
    //       under the same length-5/prefix-"alpha beta" bucket
    //       — but it must be a distinct leaf, so we force
    //       distinctness by ingesting under a tweaked
    //       threshold-disabling config)
    //
    // Forcing two leaves under one parent requires that at
    // least the second ingest land below threshold. Use a
    // threshold of 1.0 so any mismatch makes a new leaf, then
    // drop to a threshold-allowing scenario for the third.
    //
    // Simpler: build via a two-stage config swap is hard since
    // MinerCluster owns the config. Instead, push leaves
    // directly through `templates_for` is read-only.
    //
    // Real-world simpler: under a threshold of 1.0, every
    // distinct mask creates its own leaf. Then we can't widen
    // anything (no merges happen). That doesn't test the
    // best-candidate logic.
    //
    // Under the default 0.7 threshold + 0.5 floor:
    //
    //   L1 = "alpha beta gamma delta epsilon"  → leaf A.
    //   L2 = "alpha beta gamma rho sigma"      → sim with A = 3/5
    //                                            = 0.6 ∈ [0.5, 0.7)
    //                                            → lossy zone →
    //                                            new leaf B (same
    //                                            `(length, prefix)`
    //                                            bucket).
    //   L3 = "alpha beta gamma delta zeta"     → sim with A = 4/5
    //                                            = 0.8 (clean), sim
    //                                            with B = 3/5 = 0.6
    //                                            (lossy). Best
    //                                            candidate is A;
    //                                            widens A at
    //                                            position 4.
    //
    // (Pre-three-zone this test had L2 at sim 0.4 — that's
    // now a parse failure rather than a leaf, so L2 was
    // rewritten to land in the lossy zone where the
    // best-candidate-selection question still makes sense.)
    let (mut cluster, sink) = cluster_with_observable_sink();
    let t = TenantId::new("tenant-x");

    let id_a = cluster.ingest(&string_record(&t, "alpha beta gamma delta epsilon"));
    let id_b = cluster.ingest(&string_record(&t, "alpha beta gamma rho sigma"));
    assert_ne!(id_a, id_b, "leaves are distinct after L2");
    assert_eq!(cluster.template_count(&t), 2);
    assert!(
        drain_changes(&sink).is_empty(),
        "L2 fell into the lossy zone → fresh leaf, no widening",
    );
    assert_eq!(
        cluster.body_retentions_total(),
        1,
        "L2's lossy attach is one body retention",
    );

    let id_c = cluster.ingest(&string_record(&t, "alpha beta gamma delta zeta"));
    // Must widen leaf A (sim 0.8, clean), not B (sim 0.6, lossy).
    assert_eq!(
        id_c, id_a,
        "best-candidate selection must pick the higher-similarity leaf",
    );
    assert_eq!(cluster.merges_total(), 1);
    let events = drain_changes(&sink);
    assert_eq!(events.len(), 1);
    let AuditPayload::Template {
        template_id,
        change: TemplateChange::Widened {
            positions_widened, ..
        },
        ..
    } = &events[0].payload
    else {
        panic!("expected Template/Widened, got {:?}", events[0].payload);
    };
    assert_eq!(*template_id, id_a);
    assert_eq!(*positions_widened, vec![4]);
}

// ---------- helper-function unit tests ----------

#[test]
fn find_widening_positions_returns_only_mismatched_fixed_positions() {
    let template = vec![
        OwnedToken::Fixed("user".to_string()),
        OwnedToken::Fixed("42".to_string()),
        OwnedToken::Wildcard,
        OwnedToken::Fixed("in".to_string()),
    ];
    let line = ["user", "17", "anything", "out"];
    let positions = find_widening_positions(&line, &template, &[]);
    // Position 0: Fixed "user" == "user" → no widening.
    // Position 1: Fixed "42" != "17" → widen.
    // Position 2: Wildcard → never in the widening set.
    // Position 3: Fixed "in" != "out" → widen.
    assert_eq!(positions, vec![1, 3]);
}

#[test]
fn would_be_degenerate_only_when_no_fixed_remain() {
    let template = vec![
        OwnedToken::Fixed("a".to_string()),
        OwnedToken::Wildcard,
        OwnedToken::Fixed("c".to_string()),
    ];
    // Widening only position 0 leaves position 2 Fixed → not degenerate.
    assert!(!would_be_degenerate(&template, &[0]));
    // Widening position 2 leaves position 0 Fixed → not degenerate.
    assert!(!would_be_degenerate(&template, &[2]));
    // Widening positions 0 AND 2 leaves nothing Fixed → degenerate.
    assert!(would_be_degenerate(&template, &[0, 2]));
    // Widening no positions on a template with Fixed left → not degenerate.
    assert!(!would_be_degenerate(&template, &[]));
}

#[test]
fn format_template_renders_canonical_form() {
    let template = vec![
        OwnedToken::Fixed("user".to_string()),
        OwnedToken::Wildcard,
        OwnedToken::Fixed("logged".to_string()),
        OwnedToken::Wildcard,
    ];
    assert_eq!(format_template(&template), "user <*> logged <*>");
}

#[test]
fn apply_widening_replaces_only_listed_positions() {
    let mut template = vec![
        OwnedToken::Fixed("a".to_string()),
        OwnedToken::Fixed("b".to_string()),
        OwnedToken::Fixed("c".to_string()),
    ];
    apply_widening(&mut template, &[1]);
    assert!(matches!(template[0], OwnedToken::Fixed(ref s) if s == "a"));
    assert!(matches!(template[1], OwnedToken::Wildcard));
    assert!(matches!(template[2], OwnedToken::Fixed(ref s) if s == "c"));
}

#[test]
fn ingest_string_routes_lines_above_u16_max_tokens_to_parse_failure() {
    // The cap defends `positions_widened: Vec<u16>` (RFC §6.4)
    // from a silent-merge bug: if the helper had to drop
    // out-of-range positions, an attach with no surviving
    // mismatches would have looked like a clean match
    // (no widening, no audit, no `merges_total` bump).
    // Producing a 65 537-token line here is the smallest input
    // that exercises the cap.
    let mut cluster = MinerCluster::new(MinerConfig::default());
    let t = TenantId::new("tenant-x");

    let n: usize = (u16::MAX as usize) + 2;
    let mut text = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            text.push(' ');
        }
        text.push('x');
    }

    let id = cluster.ingest(&string_record(&t, &text));

    assert_eq!(id, NO_TEMPLATE);
    assert_eq!(cluster.template_count(&t), 0);
    assert_eq!(cluster.parse_failures_total(), 1);
    assert_eq!(
        cluster.body_retentions_total(),
        1,
        "RFC §6.3: over-cap lines retain body alongside the parse-failure count",
    );
    assert_eq!(cluster.merges_total(), 0);
}
