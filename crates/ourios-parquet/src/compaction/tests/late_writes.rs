//! RFC 0009 §3.4 late-write note: a flush file the manifest does not consume
//! is live, so a late write is read, compacted later and never deleted as an
//! orphan (RFC0009.13–.15, .19; #1026).

use std::panic::{AssertUnwindSafe, catch_unwind};

use proptest::prelude::*;

use super::*;

/// The template ids [`partition`] reads, sorted.
fn live_templates(store: &Store) -> Vec<u64> {
    let mut ids: Vec<u64> = read_partition_rows(store)
        .iter()
        .map(|r| r.template_id)
        .collect();
    ids.sort_unstable();
    ids
}

/// The template ids `visit_partition_rows` yields for [`partition`], sorted.
fn visited_templates(store: &Store) -> Vec<u64> {
    let mut ids = Vec::new();
    visit_partition_rows(store, &partition(), |batch| {
        ids.extend(batch.iter().map(|r| r.template_id));
    })
    .expect("visit");
    ids.sort_unstable();
    ids
}

/// RFC0009.13 — the #1026 repro: a late write into a compacted partition
/// is read, re-flags the partition, survives GC and is consumed by the next
/// compaction.
#[test]
fn rfc0009_13_a_late_write_into_a_compacted_partition_is_read_and_compacted() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0)]);
    write_file(&store, &[rec(2, TS0 + 1_000_000)]);
    compact_partition(&store, &part).expect("compact");
    write_file(&store, &[rec(3, TS0 + 2_000_000)]);

    assert_eq!(visited_templates(&store), [1, 2, 3], "the late row is read");
    assert_eq!(
        plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default()).expect("plan"),
        vec![part.clone()],
        "the late file re-flags the partition"
    );
    assert_eq!(
        gc_orphans(&store, &part).expect("gc"),
        OrphanGc::default(),
        "GC keeps the late file"
    );
    let second = compact_partition(&store, &part)
        .expect("compact again")
        .committed
        .expect("committed");
    assert_eq!(
        second.input_files.len(),
        2,
        "the compacted file and the late one"
    );
    assert_eq!(live_keys(&store, &part).len(), 1, "one consolidated file");
    assert_eq!(
        visited_templates(&store),
        [1, 2, 3],
        "every row exactly once"
    );
}

/// RFC0009.15 — a flush file that lands after a compaction listed its inputs
/// is not consumed by its commit and stays live.
#[test]
fn rfc0009_15_a_flush_between_listing_and_commit_stays_live() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0)]);
    write_file(&store, &[rec(2, TS0 + 1_000_000)]);
    let mut landed = false;
    let mut land_once = |_: &[MinedRecord]| {
        if !landed {
            landed = true;
            write_file(&store, &[rec(3, TS0 + 2_000_000)]);
        }
    };
    let mut hooks = RowHooks {
        observe: Some(&mut land_once),
        ..RowHooks::default()
    };

    let committed =
        compact_partition_hooked(&store, &part, &PromotedAttributes::default(), &mut hooks)
            .expect("compact")
            .committed
            .expect("committed");

    assert_eq!(
        committed.input_files.len(),
        2,
        "the late file was not an input"
    );
    let manifest = read_manifest(&store, &part)
        .expect("read")
        .expect("manifest");
    assert_eq!(manifest.consumed.len(), 2, "only the inputs are consumed");
    assert_eq!(
        live_templates(&store),
        [1, 2, 3],
        "the late row is still read"
    );
    gc_orphans(&store, &part).expect("gc");
    assert_eq!(live_templates(&store), [1, 2, 3], "and survives GC");
}

/// RFC0009.19 — the planner, `compact_partition` and GC reject a manifest
/// written before the late-write amendment, and change nothing in the store.
#[test]
fn rfc0009_19_an_old_format_manifest_is_rejected_by_compaction() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[rec(1, TS0)]);
    write_file(&store, &[rec(2, TS0 + 1_000_000)]);
    let old = br#"{"generation":1,"files":["a.parquet"]}"#.to_vec();
    store
        .put_blocking(&manifest_key(&part), old)
        .expect("old manifest");
    let before = store.list_blocking(None).expect("list");
    let unsupported = |e: CompactionError| {
        matches!(
            e,
            CompactionError::Manifest(ManifestError::UnsupportedFormat(0))
        )
    };

    let plan = plan_candidates(&store, "a", NOW_SEALED, &CompactionPolicy::default());
    let compact = compact_partition(&store, &part);
    let gc = gc_orphans(&store, &part);

    assert!(plan.is_err_and(unsupported), "planner rejects it");
    assert!(compact.is_err_and(unsupported), "compaction rejects it");
    assert!(gc.is_err_and(unsupported), "GC rejects it");
    assert_eq!(
        store.list_blocking(None).expect("list"),
        before,
        "nothing changed"
    );
}

/// One step of the RFC0009.14 interleaving.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// A writer publishes a flush file with one new row.
    Flush,
    /// A compaction that runs to completion.
    Compact,
    /// A compactor dies after writing its output, before the commit.
    CrashBeforeCommit,
    /// A compactor dies after its commit, before deleting its inputs.
    CrashAfterCommit,
    /// A GC pass.
    Gc,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => Just(Step::Flush),
        2 => Just(Step::Compact),
        1 => Just(Step::CrashBeforeCommit),
        1 => Just(Step::CrashAfterCommit),
        2 => Just(Step::Gc),
    ]
}

/// Run `step` against [`partition`]; `flushed` holds the template ids of
/// every completed flush put.
fn run_step(store: &Store, step: Step, flushed: &mut Vec<u64>) {
    let part = partition();
    match step {
        Step::Flush => {
            let id = flushed.len() as u64 + 1;
            write_file(store, &[rec(id, TS0 + id * 1_000)]);
            flushed.push(id);
        }
        Step::Compact => {
            compact_partition(store, &part).expect("compact");
        }
        Step::CrashBeforeCommit => {
            let next = read_manifest(store, &part)
                .expect("read")
                .map_or(1, |m| m.generation + 1);
            let rows = read_partition_rows(store);
            if !rows.is_empty() {
                write_uncommitted_output(store, &rows, next);
            }
        }
        Step::CrashAfterCommit => {
            let mut die = |_: &CompactionOutcome| panic!("killed after the commit");
            let mut hooks = RowHooks {
                on_commit: Some(&mut die),
                ..RowHooks::default()
            };
            let promoted = PromotedAttributes::default();
            let _ = catch_unwind(AssertUnwindSafe(|| {
                compact_partition_hooked(store, &part, &promoted, &mut hooks)
            }));
        }
        Step::Gc => {
            gc_orphans(store, &part).expect("gc");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// RFC0009.14 — under any interleaving of flush puts, compactions,
    /// crashes before and after the commit, and GC passes, every row of every
    /// completed flush put is read exactly once after every step.
    #[test]
    fn rfc0009_14_no_unconsumed_flush_file_is_deleted(
        steps in prop::collection::vec(step(), 1..14),
    ) {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let mut flushed = Vec::new();
        for step in steps {
            run_step(&store, step, &mut flushed);
            prop_assert_eq!(live_templates(&store), flushed.clone(), "after {:?}", step);
        }
    }
}
