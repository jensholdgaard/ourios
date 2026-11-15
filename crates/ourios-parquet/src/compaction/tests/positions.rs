//! RFC0065.33: compaction keeps the RFC 0065 §6.6 ingest position columns,
//! keeps one row per `(tenant, ingest position)`, and keeps every row of a
//! file without the columns, in memory and through spilled runs alike.

use std::collections::{BTreeMap, BTreeSet};

use ourios_core::record::IngestPosition;

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
        let unpositioned = rows.iter().filter(|r| r.ingest_position.is_none()).count();
        assert_eq!(
            unpositioned,
            local.len(),
            "{sorted:?}: every local row kept"
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
    let mut repeats = Repeats::new(ClusterKeys::TimeOnly);
    let mut rows = vec![
        positioned(1, TS0, at(1, 0)),
        positioned(1, TS0, at(1, 1)),
        rec(1, TS0),
        positioned(1, TS0, at(1, 0)),
        rec(1, TS0),
        positioned(1, TS0 + 1, at(2, 0)),
        positioned(1, TS0 + 1, at(2, 0)),
    ];
    repeats.drop_from(&mut rows);
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
    assert_eq!(
        repeats.seen.len(),
        1,
        "only the last key's positions are held"
    );
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
        let unpositioned = got.iter().filter(|r| r.ingest_position.is_none()).count();
        proptest::prop_assert_eq!(unpositioned, local);
        proptest::prop_assert_eq!(outcome.rows, got.len() as u64);
    }
}
