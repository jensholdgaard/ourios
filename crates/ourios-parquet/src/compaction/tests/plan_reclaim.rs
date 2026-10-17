//! RFC 0009 §3.6 orphan-GC amendment: besides its candidates, planning picks
//! the sealed partitions whose listing shows an orphan, from the same listing
//! and manifest reads (RFC0009.26).

use super::*;

/// Plan tenant `a` at `now`: the plan and the store work it did.
fn plan_at(store: &Store, now: u64) -> (CompactionPlan, PlanCounts) {
    let (plan, counts) = plan_compaction_counted(store, "a", now, &CompactionPolicy::default());
    (plan.expect("plan"), counts)
}

fn reclaimable(partitions: Vec<PartitionKey>) -> CompactionPlan {
    CompactionPlan {
        candidates: Vec::new(),
        reclaimable: partitions,
    }
}

/// Put an interrupted publish into [`partition`].
fn put_tmp(store: &Store) {
    let key = format!("{}/stray.parquet.tmp", partition_data_prefix(&partition()));
    store.put_blocking(&key, b"torn".to_vec()).expect("tmp");
}

/// RFC0009.26 — a compacted partition left with a dead output (its sweep
/// stopped between the commit and the GC) is no candidate but is planned
/// for GC, from the one manifest read the listing already called for. Once
/// reclaimed it is settled and costs nothing.
#[test]
fn rfc0009_26_a_settled_partition_with_a_dead_output_is_reclaimable() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let rows = [rec(1, TS0), rec(2, TS0 + 1_000_000)];
    write_file(&store, &rows[..1]);
    write_file(&store, &rows[1..]);
    let generation = commit_compaction(&store).generation;
    write_uncommitted_output(&store, &rows, generation);

    let (plan, counts) = plan_at(&store, NOW_SEALED);

    assert_eq!(
        (plan, counts.manifest_reads),
        (reclaimable(vec![partition()]), 1)
    );
    assert_gc_reclaims(&store, 1, "the dead output is reclaimed");
    let (plan, counts) = plan_at(&store, NOW_SEALED);
    assert_eq!(
        (plan, counts.manifest_reads),
        (CompactionPlan::default(), 0),
        "settled: one listed file, no manifest read"
    );
}

/// RFC0009.26 — an interrupted publish makes a sealed partition reclaimable
/// without a manifest read; in an unsealed one it is a writer's, never
/// planned.
#[test]
fn rfc0009_26_a_stray_tmp_makes_only_a_sealed_partition_reclaimable() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_file(&store, &[rec(1, TS0)]);
    put_tmp(&store);

    let (sealed, counts) = plan_at(&store, NOW_SEALED);
    let (unsealed, _) = plan_at(&store, NOW_UNSEALED);

    assert_eq!(
        (sealed, counts.manifest_reads),
        (reclaimable(vec![partition()]), 0)
    );
    assert_eq!(unsealed, CompactionPlan::default());
}

/// A candidate is planned once, as a candidate, whatever else is listed
/// beside it; its GC runs after its rewrite.
#[test]
fn a_candidate_with_an_orphan_is_planned_only_as_a_candidate() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    write_file(&store, &[rec(1, TS0)]);
    write_file(&store, &[rec(2, TS0 + 1_000_000)]);
    put_tmp(&store);

    let (plan, _) = plan_at(&store, NOW_SEALED);

    assert_eq!(
        plan,
        CompactionPlan {
            candidates: vec![partition()],
            reclaimable: Vec::new(),
        }
    );
}
