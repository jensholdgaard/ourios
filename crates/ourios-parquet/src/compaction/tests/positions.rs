//! RFC0065.33: compaction keeps the RFC 0065 §6.6 ingest position columns,
//! keeps one row per `(tenant, ingest position)`, and keeps every row of a
//! file without the columns, in memory and through spilled runs alike.

use std::collections::{BTreeMap, BTreeSet};

use ourios_core::record::IngestPosition;

use crate::compaction::repeats::Repeats;

use super::*;

fn at(seq: u64, index: u32) -> IngestPosition {
    IngestPosition {
        group: 7,
        seq,
        index,
    }
}

/// `rec` at `ts_ns`, mined from the replicated log at `position`.
fn positioned(template_id: u64, ts_ns: u64, position: IngestPosition) -> MinedRecord {
    MinedRecord {
        ingest_position: Some(position),
        ..rec(template_id, ts_ns)
    }
}

/// Write `recs`, each with an ingest position, as one flushed file.
fn write_positioned(store: &Store, recs: &[MinedRecord]) {
    let promoted = PromotedAttributes::default();
    let mut w =
        Writer::open_in_with_ingest_positions(store, partition(), DEFAULT_ZSTD_LEVEL, promoted)
            .expect("open writer");
    w.append_records(recs).expect("append");
    w.close().expect("close");
}

/// Whether the live compacted file declares the three ingest columns.
fn declares_positions(store: &Store, key: &str) -> bool {
    use parquet::file::reader::{FileReader as _, SerializedFileReader};
    let bytes = bytes::Bytes::from(store.get_blocking(key).expect("get"));
    let reader = SerializedFileReader::new(bytes).expect("parquet");
    let schema = reader.metadata().file_metadata().schema_descr();
    let names: BTreeSet<_> = schema
        .columns()
        .iter()
        .map(|c| c.name().to_owned())
        .collect();
    let declared = crate::columns::INGEST_POSITION.map(|c| names.contains(c));
    assert!(
        declared.iter().all(|d| *d) || declared.iter().all(|d| !*d),
        "all three columns or none: {declared:?}"
    );
    declared[0]
}

/// `rows`, sorted by the fields these tests vary.
fn sorted_rows(rows: &[MinedRecord]) -> Vec<MinedRecord> {
    let mut rows = rows.to_vec();
    rows.sort_by(|a, b| row_key(a).cmp(&row_key(b)));
    rows
}

/// The rows of `rows` without a position, sorted as [`sorted_rows`] does.
fn unpositioned(rows: &[MinedRecord]) -> Vec<MinedRecord> {
    let local: Vec<_> = rows
        .iter()
        .filter(|r| r.ingest_position.is_none())
        .cloned()
        .collect();
    sorted_rows(&local)
}

/// How a test compacts: wholly in memory, or spilling every input as a
/// run and merging two at a time.
#[derive(Debug, Clone, Copy)]
enum Sorted {
    InMemory,
    Spilled,
}

impl Sorted {
    const BOTH: [Self; 2] = [Self::InMemory, Self::Spilled];

    fn tuning(self) -> SortTuning {
        match self {
            Self::InMemory => SortTuning::default(),
            Self::Spilled => SortTuning {
                in_memory_max_bytes: 0,
                fan_in: 2,
                ..SortTuning::default()
            },
        }
    }

    fn compact(
        self,
        store: &Store,
        part: &PartitionKey,
        drop: Option<&RowFilter<'_>>,
    ) -> CompactionOutcome {
        let promoted = PromotedAttributes::default();
        let keys = ClusterKeys::for_promoted(&promoted);
        let mut hooks = RowHooks {
            drop,
            ..RowHooks::default()
        };
        compact_sorted_hooked(store, part, &promoted, keys, self.tuning(), &mut hooks)
            .expect("compact")
    }
}

/// The rows a reassignment's two flushers published, at the same
/// positions, a later file repeating two of them beside a new one, and a
/// local-backend file, one of whose rows shares a timestamp with a
/// positioned row.
fn mixed_partition(store: &Store) -> (Vec<MinedRecord>, Vec<MinedRecord>) {
    let flushed: Vec<_> = (0..4)
        .map(|i| positioned(1, TS0 + u64::from(i) * 1_000, at(10, i)))
        .collect();
    write_positioned(store, &flushed);
    write_positioned(store, &flushed);
    let mut later = vec![flushed[1].clone(), flushed[3].clone()];
    later.push(positioned(2, TS0 + 9_000, at(11, 0)));
    write_positioned(store, &later);
    let local = vec![rec(3, TS0), rec(3, TS0 + 5_000), rec(3, TS0 + 6_000)];
    write_file(store, &local);
    let mut kept = flushed;
    kept.push(later[2].clone());
    (kept, local)
}

/// Scenario RFC0065.33: the compacted file declares the columns, holds
/// each position once and every local row with a null position, and the
/// repeats dropped are reported.
#[test]
fn rfc0065_33_compaction_keeps_one_row_per_position_and_every_row_without_one() {
    for sorted in Sorted::BOTH {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        let (kept, local) = mixed_partition(&store);

        let outcome = sorted.compact(&store, &part, None);

        let live = live_keys(&store, &part);
        assert_eq!(live.len(), 1, "{sorted:?}");
        assert!(declares_positions(&store, &live[0]), "{sorted:?}");
        let rows = read_key(&store, &part, &live[0]);
        let positions: Vec<_> = rows.iter().filter_map(|r| r.ingest_position).collect();
        let distinct: BTreeSet<_> = positions.iter().copied().collect();
        assert_eq!(
            positions.len(),
            distinct.len(),
            "{sorted:?}: a position twice"
        );
        assert_eq!(distinct.len(), kept.len(), "{sorted:?}");
        assert_eq!(
            unpositioned(&rows),
            sorted_rows(&local),
            "{sorted:?}: every local row kept, value-equal"
        );
        assert_eq!(
            outcome.rows,
            (kept.len() + local.len()) as u64,
            "{sorted:?}"
        );
        assert_eq!(outcome.rows_deduplicated, 6, "{sorted:?}: 4 + 2 repeats");
    }
}

/// Scenario RFC0065.33, the erasure arm: an erasure over the compacted
/// file removes exactly the rows it matches, the positioned and the local
/// ones alike, and every surviving row keeps its position.
#[test]
fn rfc0065_33_an_erasure_keeps_every_surviving_rows_position() {
    for sorted in Sorted::BOTH {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        mixed_partition(&store);
        sorted.compact(&store, &part, None);
        let before = read_partition_rows(&store);

        let erased = |r: &MinedRecord| r.time_unix_nano == TS0 + 1_000 || r.template_id == 3;
        let outcome = sorted.compact(&store, &part, Some(&erased));

        let after = read_partition_rows(&store);
        let expected: Vec<_> = before.iter().filter(|r| !erased(r)).cloned().collect();
        assert_eq!(outcome.rows_dropped, (before.len() - expected.len()) as u64);
        assert_eq!(outcome.rows_deduplicated, 0, "{sorted:?}");
        let key = |r: &MinedRecord| (r.time_unix_nano, r.template_id, r.ingest_position);
        let mut after_keys: Vec<_> = after.iter().map(key).collect();
        let mut expected_keys: Vec<_> = expected.iter().map(key).collect();
        after_keys.sort();
        expected_keys.sort();
        assert_eq!(after_keys, expected_keys, "{sorted:?}");
    }
}

/// Scenario RFC0065.33, the unpositioned arm: no input declares the
/// columns, so neither does the output, and nothing is deduplicated.
#[test]
fn rfc0065_33_a_partition_without_positions_compacts_without_them() {
    for sorted in Sorted::BOTH {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        write_file(&store, &[rec(1, TS0), rec(1, TS0)]);
        write_file(&store, &[rec(1, TS0)]);

        let outcome = sorted.compact(&store, &part, None);

        let live = live_keys(&store, &part);
        assert!(!declares_positions(&store, &live[0]), "{sorted:?}");
        assert_eq!(outcome.rows, 3, "{sorted:?}: identical local rows all kept");
        assert_eq!(outcome.rows_deduplicated, 0, "{sorted:?}");
    }
}

/// Within a run of equal keys a repeat is dropped wherever it falls, and
/// once the key moves on, positions are forgotten, so the set stays the
/// size of one run of equal keys.
#[test]
fn a_repeat_is_dropped_anywhere_among_its_equal_keys() {
    let bucket = tempfile::tempdir().expect("temp");
    let (store, part, promoted) = (
        store_at(bucket.path()),
        partition(),
        PromotedAttributes::default(),
    );
    let plan = SortPlan {
        store: &store,
        partition: &part,
        promoted: &promoted,
        keys: ClusterKeys::TimeOnly,
        tuning: SortTuning::default(),
        gauges: Gauges(None),
    };
    let charge = WriteCharge::new(plan.gauges);
    let mut repeats = Repeats::new(plan, &charge);
    let mut rows = vec![
        positioned(1, TS0, at(1, 0)),
        positioned(1, TS0, at(1, 1)),
        rec(1, TS0),
        positioned(1, TS0, at(1, 0)),
        rec(1, TS0),
        positioned(1, TS0 + 1, at(2, 0)),
        positioned(1, TS0 + 1, at(2, 0)),
    ];
    repeats.drop_from(&mut rows).expect("dropped");
    let kept: Vec<_> = rows
        .iter()
        .map(|r| (r.time_unix_nano, r.ingest_position))
        .collect();
    let expected = [
        (TS0, Some(at(1, 0))),
        (TS0, Some(at(1, 1))),
        (TS0, None),
        (TS0, None),
        (TS0 + 1, Some(at(2, 0))),
    ];
    assert_eq!(kept, expected);
    assert_eq!(repeats.dropped, 2);
    assert_eq!(repeats.held(), 1, "only the last key's positions are held");
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

    /// RFC0009.2 as RFC 0065 §6.6 restates it: for any split of
    /// positioned rows across files, some files repeating others' rows
    /// as two flushers would, and a local file beside them, the
    /// compacted file holds one row per distinct position plus every
    /// local row, each value-equal to what was written.
    #[test]
    fn compaction_keeps_each_position_once_and_every_local_row(
        rows in proptest::collection::vec((0u64..1_000_000, 0u32..4), 1..=20),
        repeated in proptest::collection::vec(proptest::prelude::any::<proptest::sample::Index>(), 0..=10),
        local in 0usize..=5,
    ) {
        let bucket = tempfile::tempdir().expect("temp");
        let store = store_at(bucket.path());
        let part = partition();
        let mut written: BTreeMap<IngestPosition, MinedRecord> = BTreeMap::new();
        let originals: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, (offset, template))| {
                let position = at(i as u64, 0);
                positioned(u64::from(*template), TS0 + offset, position)
            })
            .collect();
        for row in &originals {
            written.insert(row.ingest_position.expect("positioned"), row.clone());
        }
        write_positioned(&store, &originals);
        let repeats: Vec<_> = repeated.iter().map(|i| originals[i.index(originals.len())].clone()).collect();
        if !repeats.is_empty() {
            write_positioned(&store, &repeats);
        }
        let locals: Vec<_> = (0..local).map(|i| rec(9, TS0 + i as u64)).collect();
        if !locals.is_empty() {
            write_file(&store, &locals);
        }
        if repeats.is_empty() && locals.is_empty() {
            write_positioned(&store, &originals[..1]);
        }

        let outcome = compact_partition(&store, &part).expect("compact");

        let got = read_partition_rows(&store);
        let mut kept: BTreeMap<IngestPosition, MinedRecord> = BTreeMap::new();
        for row in got.iter().filter(|r| r.ingest_position.is_some()) {
            let position = row.ingest_position.expect("positioned");
            proptest::prop_assert!(kept.insert(position, row.clone()).is_none(), "{position:?} twice");
        }
        proptest::prop_assert_eq!(kept, written);
        proptest::prop_assert_eq!(unpositioned(&got), sorted_rows(&locals));
        proptest::prop_assert_eq!(outcome.rows, got.len() as u64);
    }
}

/// RFC 0036 §3.2: the spilled and in-memory paths write the same bytes
/// with repeats dropped too, row groups cut at the same rows, whatever
/// share of each merge chunk the repeats take.
#[test]
fn spilled_and_in_memory_write_the_same_bytes_with_repeats_dropped() {
    let rows: Vec<_> = (0..3_000u32)
        .map(|i| positioned(1, TS0 + u64::from(i), at(1, i)))
        .collect();
    let bytes: Vec<Vec<u8>> = Sorted::BOTH
        .iter()
        .map(|sorted| {
            let bucket = tempfile::tempdir().expect("temp");
            let store = store_at(bucket.path());
            let part = partition();
            write_positioned(&store, &rows);
            write_positioned(&store, &rows);
            let tuning = SortTuning {
                compacted_flush_bytes: Some(16 * 1024),
                ..sorted.tuning()
            };
            let promoted = PromotedAttributes::default();
            let keys = ClusterKeys::for_promoted(&promoted);
            let outcome = compact_sorted(&store, &part, &promoted, keys, tuning).expect("compact");
            assert_eq!(outcome.rows_deduplicated, 3_000, "{sorted:?}");
            let committed = outcome.committed.expect("committed");
            let key = format!("{}/{}", partition_data_prefix(&part), committed.file);
            store.get_blocking(&key).expect("get")
        })
        .collect();
    assert!(bytes[0] == bytes[1], "the two paths wrote different bytes");
}

/// One run of equal keys larger than its share of the budget, as rows
/// without a timestamp make (they all sort at `(service, 0)`): its
/// positions spill to scratch rather than grow in memory, and every
/// repeat is still dropped.
#[test]
fn a_run_of_equal_keys_past_its_share_spills_its_positions_and_stays_exact() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    let rows: Vec<_> = (0..200u32).map(|i| positioned(1, TS0, at(5, i))).collect();
    write_positioned(&store, &rows);
    write_positioned(&store, &rows[..150]);
    write_positioned(&store, &rows[50..]);
    // Rows fit in memory; each run's share of the budget holds three
    // positions.
    let tuning = SortTuning {
        in_memory_max_bytes: 10 * 1024 * 1024,
        fan_in: 100_000,
        ..SortTuning::default()
    };
    let stats = CompactionStats::new();
    let promoted = PromotedAttributes::default();
    let keys = ClusterKeys::for_promoted(&promoted);
    let mut hooks = RowHooks {
        stats: Some(&stats),
        ..RowHooks::default()
    };

    let outcome =
        compact_sorted_hooked(&store, &part, &promoted, keys, tuning, &mut hooks).expect("compact");

    assert_eq!(outcome.rows, 200);
    assert_eq!(outcome.rows_deduplicated, 300);
    let got = read_partition_rows(&store);
    let distinct: BTreeSet<_> = got.iter().filter_map(|r| r.ingest_position).collect();
    assert_eq!(distinct.len(), 200);
    assert!(stats.spill_runs() > 0, "the positions spilled");
}

/// With no budget every new position spills, and the runs merge each time
/// they pass the fan-in of 2; a repeat is still found wherever its first
/// copy went, in memory, in a run or in a merged run.
#[test]
fn spilled_and_merged_position_runs_still_find_every_repeat() {
    let bucket = tempfile::tempdir().expect("temp");
    let (store, part, promoted) = (
        store_at(bucket.path()),
        partition(),
        PromotedAttributes::default(),
    );
    let stats = CompactionStats::new();
    let plan = SortPlan {
        store: &store,
        partition: &part,
        promoted: &promoted,
        keys: ClusterKeys::TimeOnly,
        tuning: SortTuning {
            in_memory_max_bytes: 0,
            fan_in: 2,
            ..SortTuning::default()
        },
        gauges: Gauges(Some(&stats)),
    };
    let charge = WriteCharge::new(plan.gauges);
    let mut repeats = Repeats::new(plan, &charge);
    let firsts: Vec<_> = (0..40u32)
        .rev()
        .map(|i| positioned(1, TS0, at(9, i)))
        .collect();
    let mut rows = firsts.clone();
    rows.extend(firsts.iter().step_by(3).cloned());
    rows.push(rec(1, TS0));
    rows.extend(firsts.iter().cloned());

    repeats.drop_from(&mut rows).expect("dropped");

    assert_eq!(rows.len(), 41, "each position once, and the local row");
    assert_eq!(repeats.dropped, 54);
    // A run spills each time two positions are held: 20 runs, and each
    // eighth of them merges the tier into one run of the next, which
    // counts as a spill too.
    assert_eq!(stats.spill_runs(), 22);
    assert!(repeats.held() <= 1);
}

/// The positions held are charged to the write phase as the set grows,
/// by the slots it has allocated, and released with a wide run of keys.
#[test]
fn held_positions_are_charged_as_they_change() {
    let bucket = tempfile::tempdir().expect("temp");
    let (store, part, promoted) = (
        store_at(bucket.path()),
        partition(),
        PromotedAttributes::default(),
    );
    let stats = CompactionStats::new();
    let plan = SortPlan {
        store: &store,
        partition: &part,
        promoted: &promoted,
        keys: ClusterKeys::TimeOnly,
        tuning: SortTuning::default(),
        gauges: Gauges(Some(&stats)),
    };
    let charge = WriteCharge::new(plan.gauges);
    let mut repeats = Repeats::new(plan, &charge);
    let mut rows: Vec<_> = (0..2_000u32)
        .map(|i| positioned(1, TS0, at(4, i)))
        .collect();
    repeats.drop_from(&mut rows).expect("dropped");
    let held = stats.usage(MemoryPhase::Write);
    assert!(
        held >= 2_000 * 32,
        "{held} bytes charged for 2000 positions"
    );
    assert_eq!(stats.peak(MemoryPhase::Write), held);

    let mut next = vec![positioned(1, TS0 + 1, at(4, 0))];
    repeats.drop_from(&mut next).expect("dropped");
    assert!(
        stats.usage(MemoryPhase::Write) < held / 10,
        "released with the key"
    );
}
