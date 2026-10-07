//! RFC 0036 §3.2 sorted-compaction internals: the §3.1 total order on
//! both sort paths, the time-only key, and the RFC0036.3 decoded-row
//! residency bounds.

use super::*;

/// A record for the sort tests: `service` becomes the promoted
/// `service.name` resource attribute (`None` = absent, the §3.1
/// nulls-first case) and `id` a unique param payload so equal-key
/// rows stay distinguishable for tie-break assertions.
fn sort_rec(service: Option<&str>, ts_ns: u64, id: u64) -> MinedRecord {
    let resource_attributes = match service {
        Some(name) => vec![ourios_core::otlp::KeyValue {
            key: SERVICE_NAME_KEY.to_string(),
            value: Some(ourios_core::otlp::AnyValue {
                value: Some(ourios_core::otlp::any_value::Value::StringValue(
                    name.to_string(),
                )),
            }),
            ..Default::default()
        }],
        None => Vec::new(),
    };
    MinedRecord {
        resource_attributes,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: id.to_string(),
        }],
        ..rec(id, ts_ns)
    }
}

/// Mirror partition `part`'s data files from `from` into `to`
/// byte-for-byte under the same names, so two stores hold the
/// RFC0036.4 "same bytes, same names" input set.
fn mirror_partition(from: &Store, to: &Store, part: &PartitionKey) {
    for key in from
        .list_blocking(Some(&partition_data_prefix(part)))
        .expect("list source")
    {
        let bytes = from.get_blocking(&key).expect("get source");
        to.put_blocking(&key, bytes).expect("put mirror");
    }
}

/// Read the consolidated file's raw bytes after a committed
/// compaction.
fn consolidated_bytes(store: &Store, part: &PartitionKey, committed: &Committed) -> Vec<u8> {
    let key = format!("{}/{}", partition_data_prefix(part), committed.file);
    store.get_blocking(&key).expect("get consolidated")
}

proptest::proptest! {
    // Each case compacts the same inputs through both §3.2 paths
    // (in-memory and forced spill + fan-in-2 hierarchical merge),
    // so keep the case count moderate.
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

    /// RFC0036.1 (§6 merge property, internal half) — for arbitrary
    /// service/time/duplicate-key mixes, both §3.2 paths produce the
    /// §3.1 total order: the multiset equals the inputs' union,
    /// rows are (service, time)-sorted with absent-service first,
    /// equal-key rows land in (sorted-basename input ordinal,
    /// pre-sort row ordinal) tie-break order — and the spill path's
    /// bytes are identical to the in-memory path's, which is the
    /// §3.5 determinism argument across the §7 skip-spill fork.
    /// See `docs/rfcs/0036-write-side-layout.md` §5 / §6.
    #[test]
    fn sorted_merge_realises_the_total_order_on_both_paths(
        files in proptest::collection::vec(
            proptest::collection::vec(
                // (service index; 0 = absent, times from a small
                // pool to force duplicate keys)
                (0usize..4, 0u64..6),
                1..=12usize,
            ),
            2..=5usize,
        )
    ) {
        let services = [None, Some("svc-a"), Some("svc-b"), Some("svc-c")];
        let bucket_a = tempfile::tempdir().expect("temp a");
        let bucket_b = tempfile::tempdir().expect("temp b");
        let store_a = store_at(bucket_a.path());
        let store_b = store_at(bucket_b.path());
        let part = partition();

        let mut id: u64 = 0;
        let mut inputs: Vec<(String, Vec<MinedRecord>)> = Vec::new();
        for file in &files {
            let recs: Vec<MinedRecord> = file
                .iter()
                .map(|(svc, toff)| {
                    id += 1;
                    sort_rec(services[*svc], HOUR10_START + toff * 1_000, id)
                })
                .collect();
            let mut w = Writer::open_in(&store_a, part.clone()).expect("open writer");
            w.append_records(&recs).expect("append");
            let written = w.close().expect("close");
            inputs.push((basename(&written.key).to_owned(), recs));
        }
        mirror_partition(&store_a, &store_b, &part);

        // The §3.1 model order: concatenate in sorted-basename input
        // order, then stable-sort by (service, time) — leaving
        // equal-key rows in (input ordinal, row ordinal) order.
        inputs.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut expected: Vec<MinedRecord> =
            inputs.into_iter().flat_map(|(_, recs)| recs).collect();
        sort_records(ClusterKeys::ServiceThenTime, &mut expected);

        let in_memory = compact_partition(&store_a, &part).expect("compact in-memory");
        let spilled = compact_sorted(
            &store_b,
            &part,
            &PromotedAttributes::default(),
            ClusterKeys::ServiceThenTime,
            SortTuning {
                in_memory_max_bytes: 0,
                fan_in: 2,
                ..SortTuning::default()
            },
        )
        .expect("compact spilled");
        let in_memory = in_memory.committed.expect("in-memory commit");
        let spilled = spilled.committed.expect("spill commit");

        let bytes_a = consolidated_bytes(&store_a, &part, &in_memory);
        let bytes_b = consolidated_bytes(&store_b, &part, &spilled);
        proptest::prop_assert!(
            bytes_a == bytes_b,
            "in-memory and spill paths must emit byte-identical output \
             ({} vs {} bytes)",
            bytes_a.len(),
            bytes_b.len(),
        );

        let got = Reader::open_partition_bytes(
            Bytes::from(bytes_a),
            part.clone(),
            &in_memory.file,
        )
        .expect("open consolidated")
        .read_all()
        .expect("read consolidated");
        proptest::prop_assert_eq!(got, expected, "§3.1 total order realised");
    }
}

/// RFC 0036 §3.1 / §7 — the time-only fallback. A promoted set
/// without `service.name` is unrepresentable today (RFC 0022 makes
/// the key implicit and non-removable), so the degradation is
/// driven through the internal seam: under `ClusterKeys::TimeOnly`
/// the consolidated rows sort by `time_unix_nano` alone (service
/// values deliberately anti-lexicographic to prove they are
/// ignored) and every row group declares the single time sorting
/// column.
#[test]
fn time_only_keys_sort_and_declare_time_alone() {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let part = partition();
    write_file(&store, &[sort_rec(Some("zzz"), TS0, 1)]);
    write_file(&store, &[sort_rec(Some("aaa"), TS0 + 1_000, 2)]);
    write_file(&store, &[sort_rec(None, TS0 + 500, 3)]);

    let outcome = compact_sorted(
        &store,
        &part,
        &PromotedAttributes::default(),
        ClusterKeys::TimeOnly,
        SortTuning::default(),
    )
    .expect("compact");
    let committed = outcome.committed.expect("committed");
    let bytes = consolidated_bytes(&store, &part, &committed);

    let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes.clone()))
        .expect("open consolidated");
    let meta = builder.metadata();
    for rg in meta.row_groups() {
        let declared = rg.sorting_columns().expect("sorting_columns declared");
        assert_eq!(declared.len(), 1, "time-only declares a single key");
        let leaf = usize::try_from(declared[0].column_idx).expect("leaf index");
        assert_eq!(
            rg.column(leaf).column_path().string(),
            crate::columns::TIME_UNIX_NANO,
            "the single key is time_unix_nano",
        );
        assert!(!declared[0].descending, "ascending");
    }

    let rows = Reader::open_partition_bytes(Bytes::from(bytes), part.clone(), &committed.file)
        .expect("open")
        .read_all()
        .expect("read");
    let times: Vec<u64> = rows.iter().map(|r| r.time_unix_nano).collect();
    assert_eq!(
        times,
        vec![TS0, TS0 + 500, TS0 + 1_000],
        "time-only order ignores service values",
    );
}

/// Build a `k`-file partition of `s` rows each, in [`partition`], with
/// rotating promoted `service.name` values and per-row-unique times so
/// the §3.1 sort has real work.
fn build_k_file_partition(store: &Store, k: u64, s: u64) {
    let part = partition();
    let mut id: u64 = 0;
    for _ in 0..k {
        let recs: Vec<MinedRecord> = (0..s)
            .map(|_| {
                id += 1;
                let svc = ["svc-a", "svc-b", "svc-c"][usize::try_from(id % 3).expect("mod 3")];
                sort_rec(Some(svc), HOUR10_START + id, id)
            })
            .collect();
        let mut w = Writer::open_in(store, part.clone()).expect("open writer");
        w.append_records(&recs).expect("append");
        w.close().expect("close");
    }
}

/// RFC0036.3 (memory bound) — the load-bearing §3.2 claim. On a
/// partition of `K` inputs of `S` rows each, the forced-spill sort's
/// peak decoded-row residency is bounded by one input (phase 1,
/// inputs decoded strictly one at a time), then `(F + 1) × batch`
/// (phase 2, one streamed batch per open run plus the merge's output
/// chunk) — it must NOT regress to holding
/// the whole `K × S` partition decoded, which is the whole reason the
/// external merge sort exists. The in-memory (skip-spill) path, by
/// contrast, deliberately holds the whole partition (§7 tradeoff,
/// bounded by `in_memory_max_bytes`); measuring both peaks on the
/// *same* fixture pins both halves of §3.2's accurate bound. The gauge
/// is thread-local (a `compact_*` call runs entirely on this thread),
/// so the assertion is immune to `cargo test`'s in-process parallelism.
/// See `docs/rfcs/0036-write-side-layout.md` §5 / §6.
#[test]
fn rfc0036_3_forced_spill_peak_far_below_whole_partition() {
    // K inputs of S rows. The spill path's peak is dominated by
    // phase-1's one fully decoded input (S rows, decoded strictly one
    // at a time); phase-2 opens only K < F cursors — no hierarchical
    // pass — each holding one small reader batch, well under S. So the
    // peak sits at ~one input, an order of magnitude below the whole
    // partition (K × S), making a whole-partition regression
    // unambiguous. (The (F + 1) × batch term in the RFC 0036 §3.2
    // bound is the worst case for F saturated runs plus the output
    // chunk; it does not bite here.)
    const K: u64 = 6;
    const S: u64 = 12_000;
    let total = usize::try_from(K * S).expect("fits usize");
    let fan_in = SortTuning::default().fan_in;

    // --- Spill path: force it with in_memory_max_bytes = 0 so every
    // input spills one at a time. ---
    let bucket_spill = tempfile::tempdir().expect("temp spill");
    let store_spill = store_at(bucket_spill.path());
    build_k_file_partition(&store_spill, K, S);
    residency::reset();
    let spilled = compact_sorted(
        &store_spill,
        &partition(),
        &PromotedAttributes::default(),
        ClusterKeys::ServiceThenTime,
        SortTuning {
            in_memory_max_bytes: 0,
            fan_in,
            ..SortTuning::default()
        },
    )
    .expect("compact spilled");
    let spill_peak = residency::peak();
    assert!(spilled.committed.is_some(), "≥2 files ⇒ a commit");
    assert_eq!(spilled.rows, K * S, "every row carried");

    // RFC0036.3's property is an *upper* bound — "not whole-partition".
    // The teeth: peak must stay far below the whole partition; this fails
    // if the merge ever buffers everything decoded. We deliberately do
    // NOT assert a lower bound near one input: a future formation that
    // streams within an input could peak below S and still satisfy the
    // RFC. `> 0` is only a gauge-liveness sanity (spilling decodes rows).
    assert!(spill_peak > 0, "the residency gauge recorded nothing");
    assert!(
        spill_peak < total / 2,
        "forced-spill peak {spill_peak} regressed toward whole-partition \
         residency (total {total}) — the merge must not hold the partition decoded",
    );

    // --- In-memory path: same fixture, unbounded skip-spill window. ---
    let bucket_mem = tempfile::tempdir().expect("temp mem");
    let store_mem = store_at(bucket_mem.path());
    build_k_file_partition(&store_mem, K, S);
    residency::reset();
    let in_memory = compact_sorted(
        &store_mem,
        &partition(),
        &PromotedAttributes::default(),
        ClusterKeys::ServiceThenTime,
        SortTuning {
            in_memory_max_bytes: u64::MAX,
            fan_in,
            ..SortTuning::default()
        },
    )
    .expect("compact in-memory");
    let mem_peak = residency::peak();
    assert_eq!(in_memory.rows, K * S, "every row carried");
    assert_eq!(
        mem_peak, total,
        "the in-memory path holds the whole partition decoded (bounded by \
         in_memory_max_bytes — the §7 skip-spill tradeoff)",
    );

    // The contrast is the point: the spill path holds a fraction of what
    // the in-memory path holds on the identical partition.
    assert!(
        spill_peak * 4 < mem_peak,
        "the spill path ({spill_peak}) must hold far less than the \
         in-memory path ({mem_peak})",
    );
}

/// Total encoded bytes of the [`partition`]'s data files in `store`.
fn encoded_partition_bytes(store: &Store) -> u64 {
    store
        .list_with_sizes_blocking(Some(&partition_data_prefix(&partition())))
        .expect("list sizes")
        .iter()
        .map(|(_, size)| *size)
        .sum()
}

/// Issue #895 — the in-memory budget bounds *decoded* residency. A
/// partition of many small inputs whose encoded total fits the budget
/// still decodes many times larger (small files compress well), so
/// gating the skip-spill path on encoded bytes held the whole partition
/// decoded and ran the node out of memory. With the budget at twice the
/// encoded total, so the partition sits well inside the old gate, the
/// sort must spill instead of buffering every row.
#[test]
fn many_small_inputs_within_the_encoded_budget_do_not_decode_at_once() {
    const K: u64 = 120;
    const S: u64 = 200;
    const FAN_IN: usize = 4;
    let total = usize::try_from(K * S).expect("fits usize");
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    build_k_file_partition(&store, K, S);
    let budget = 2 * encoded_partition_bytes(&store);

    residency::reset();
    let outcome = compact_sorted(
        &store,
        &partition(),
        &PromotedAttributes::default(),
        ClusterKeys::ServiceThenTime,
        SortTuning {
            in_memory_max_bytes: budget,
            fan_in: FAN_IN,
            ..SortTuning::default()
        },
    )
    .expect("compact");
    let peak = residency::peak();
    assert_eq!(
        (outcome.rows, outcome.committed.is_some()),
        (K * S, true),
        "one pass commits every row",
    );

    let row_bytes = decoded_footprint(&sort_rec(Some("svc-a"), HOUR10_START, 1));
    let budget_rows = usize::try_from(budget / row_bytes).expect("fits usize");
    let one_input = usize::try_from(S).expect("fits usize");
    // The phases do not overlap: run formation releases its buffer
    // before the merge opens any cursor.
    let phase1 = budget_rows + one_input;
    let phase2 = (FAN_IN + 1) * SUB_BATCH_ROWS;
    let bound = phase1.max(phase2);
    assert!(
        peak <= bound,
        "peak residency {peak} exceeds the larger phase bound {bound} \
         (phase 1: budget {budget_rows} rows + one input = {phase1}; \
         phase 2: (F + 1) x batch = {phase2})",
    );
    assert!(
        peak * 3 < total,
        "peak residency {peak} approached the whole partition ({total} rows)",
    );
}

/// Issue #895 — a partition over the decoded budget still compacts in
/// one pass to the same bytes the unbounded in-memory sort writes, with
/// every row observed once and an erasure applied across spilled runs.
#[test]
fn over_budget_partition_matches_the_in_memory_output_with_hooks() {
    const K: u64 = 40;
    const S: u64 = 300;
    let bucket_a = tempfile::tempdir().expect("temp a");
    let bucket_b = tempfile::tempdir().expect("temp b");
    let store_a = store_at(bucket_a.path());
    let store_b = store_at(bucket_b.path());
    build_k_file_partition(&store_a, K, S);
    mirror_partition(&store_a, &store_b, &partition());

    let erased = |r: &MinedRecord| r.template_id.is_multiple_of(7);
    let run = |store: &Store, budget: u64| {
        let mut observed = 0usize;
        let mut observe = |rows: &[MinedRecord]| observed += rows.len();
        let mut hooks = RowHooks {
            observe: Some(&mut observe),
            drop: Some(&erased),
        };
        let outcome = compact_sorted_hooked(
            store,
            &partition(),
            &PromotedAttributes::default(),
            ClusterKeys::ServiceThenTime,
            SortTuning {
                in_memory_max_bytes: budget,
                fan_in: 3,
                ..SortTuning::default()
            },
            &mut hooks,
        )
        .expect("compact");
        (outcome, observed)
    };
    let (unbounded, observed_a) = run(&store_a, u64::MAX);
    let (bounded, observed_b) = run(&store_b, 64 * 1024);

    let total = usize::try_from(K * S).expect("fits usize");
    assert_eq!(
        (observed_a, observed_b),
        (total, total),
        "every row observed once on both paths",
    );
    assert_eq!(
        (
            bounded.rows,
            bounded.rows_dropped,
            bounded.rows + bounded.rows_dropped
        ),
        (unbounded.rows, unbounded.rows_dropped, K * S),
        "both paths keep and drop the same rows, accounting for all of them",
    );
    let a = unbounded.committed.expect("unbounded commit");
    let b = bounded.committed.expect("bounded commit");
    assert_eq!(
        consolidated_bytes(&store_a, &partition(), &a),
        consolidated_bytes(&store_b, &partition(), &b),
        "the bounded sort writes the unbounded sort's bytes",
    );
}

/// RFC 0036 §3.2 step 2 — after an earlier spill, inputs that leave the
/// buffer at or below the budget form a final run, and that run must be
/// sorted before the merge or the §3.1 order breaks. The residual rows
/// here arrive in descending time order.
#[test]
fn residual_buffer_after_a_spill_is_sorted_before_the_merge() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let at = |offset: u64, id: u64| sort_rec(Some("svc-a"), HOUR10_START + offset, id);
    let inputs: [Vec<MinedRecord>; 3] = [
        (0..10).map(|i| at(100 - i, i)).collect(),
        vec![at(50, 10), at(40, 11)],
        vec![at(30, 12), at(20, 13)],
    ];
    for recs in &inputs {
        write_file(&store, recs);
        // UUIDv7 names order by millisecond, so this keeps the
        // sorted-basename input order equal to the write order.
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // The first input alone overflows the budget; the last two together
    // stay under it and are left in the buffer when the inputs run out.
    let budget = 5 * decoded_footprint(&at(0, 0));

    let outcome = compact_sorted(
        &store,
        &partition(),
        &PromotedAttributes::default(),
        ClusterKeys::ServiceThenTime,
        SortTuning {
            in_memory_max_bytes: budget,
            ..SortTuning::default()
        },
    )
    .expect("compact");
    let committed = outcome.committed.expect("committed");
    let bytes = consolidated_bytes(&store, &partition(), &committed);
    let rows = Reader::open_partition_bytes(Bytes::from(bytes), partition(), &committed.file)
        .expect("open consolidated")
        .read_all()
        .expect("read consolidated");

    let mut expected: Vec<MinedRecord> = inputs.into_iter().flatten().collect();
    sort_records(ClusterKeys::ServiceThenTime, &mut expected);
    assert_eq!(rows, expected, "§3.1 order holds across the residual run");
}

/// A wide row: [`sort_rec`] plus 40 attributes of 250-byte values, the
/// shape of the #853 node's structured events.
fn wide_rec(id: u64) -> MinedRecord {
    let svc = ["svc-a", "svc-b", "svc-c"][usize::try_from(id % 3).expect("mod 3")];
    let pad = "x".repeat(250);
    MinedRecord {
        attributes: (0..40)
            .map(|a| ourios_core::otlp::KeyValue {
                key: format!("attr.{a}"),
                value: Some(ourios_core::otlp::AnyValue {
                    value: Some(ourios_core::otlp::any_value::Value::StringValue(format!(
                        "{pad}{}",
                        id % 97 + a
                    ))),
                }),
                ..Default::default()
            })
            .collect(),
        ..sort_rec(Some(svc), HOUR10_START + (id * 7_919) % 1_000_000, id)
    }
}

/// `k` inputs of `s` [`wide_rec`] rows.
fn build_wide_partition(store: &Store, k: u64, s: u64) {
    let mut id = 0;
    for _ in 0..k {
        let recs: Vec<MinedRecord> = (0..s)
            .map(|_| {
                id += 1;
                wide_rec(id)
            })
            .collect();
        write_file(store, &recs);
    }
}

/// Issue #853 — the merge is bounded in bytes, not rows: wide rows over a
/// small budget, hierarchically merged, never hold more decoded bytes than
/// the larger phase bound — phase 1's budget plus one input, or phase 2's
/// budget across the open runs plus the [`SUB_BATCH_ROWS`] output chunk.
/// Batches of [`SUB_BATCH_ROWS`] rows per open run held F × 1024 wide rows.
#[test]
fn wide_rows_merge_within_the_byte_budget() {
    const K: u64 = 16;
    const S: u64 = 300;
    const FAN_IN: usize = 4;
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    build_wide_partition(&store, K, S);
    let row = decoded_footprint(&wide_rec(1));
    let budget = 8 * row;

    residency::reset();
    let outcome = compact_sorted(
        &store,
        &partition(),
        &PromotedAttributes::default(),
        ClusterKeys::ServiceThenTime,
        SortTuning {
            in_memory_max_bytes: budget,
            fan_in: FAN_IN,
            ..SortTuning::default()
        },
    )
    .expect("compact");
    let peak = residency::peak_bytes();

    assert_eq!(outcome.rows, K * S, "every row carried");
    // Rows vary by a few bytes of suffix; a 2 % allowance covers it.
    let widest = row + row / 50;
    let chunk = u64::try_from(SUB_BATCH_ROWS).expect("fits u64");
    let phase1 = budget + S * widest;
    let phase2 = budget + chunk * widest;
    let bound = phase1.max(phase2);
    assert!(
        peak <= bound,
        "peak decoded bytes {peak} exceed the larger phase bound {bound} \
         (phase 1: budget + one input = {phase1}; phase 2: budget + output chunk = {phase2})",
    );
}

/// Issue #853 — byte-sized merge batches change no output byte: wide rows
/// forced through the spill path, with merge batches down to one row, write
/// exactly the in-memory sort's file (whose bytes the merge has always
/// matched, RFC 0036 §3.5).
#[test]
fn byte_sized_merge_batches_write_the_in_memory_bytes() {
    const K: u64 = 9;
    const S: u64 = 200;
    let bucket_a = tempfile::tempdir().expect("temp a");
    let bucket_b = tempfile::tempdir().expect("temp b");
    let store_a = store_at(bucket_a.path());
    let store_b = store_at(bucket_b.path());
    build_wide_partition(&store_a, K, S);
    mirror_partition(&store_a, &store_b, &partition());
    let run = |store: &Store, budget: u64| {
        compact_sorted(
            store,
            &partition(),
            &PromotedAttributes::default(),
            ClusterKeys::ServiceThenTime,
            SortTuning {
                in_memory_max_bytes: budget,
                fan_in: 3,
                ..SortTuning::default()
            },
        )
        .expect("compact")
        .committed
        .expect("committed")
    };

    let in_memory = run(&store_a, u64::MAX);
    let spilled = run(&store_b, 4 * decoded_footprint(&wide_rec(1)));

    assert_eq!(
        consolidated_bytes(&store_a, &partition(), &in_memory),
        consolidated_bytes(&store_b, &partition(), &spilled),
        "the byte-bounded merge writes the in-memory sort's bytes",
    );
}

/// The cursor batch is the budget's (open runs + 1)-th share over the
/// widest row, at least one row and at most [`SUB_BATCH_ROWS`].
#[test]
fn cursor_batches_are_sized_from_the_byte_budget() {
    assert_eq!(cursor_batch_rows(64 << 20, 64, 10_000), 103);
    assert_eq!(cursor_batch_rows(64 << 20, 64, 100), SUB_BATCH_ROWS);
    assert_eq!(cursor_batch_rows(0, 4, 10_000), 1, "floor of one row");
    assert_eq!(cursor_batch_rows(1 << 20, 3, u64::MAX), 1);
    assert_eq!(cursor_batch_rows(1 << 20, 3, 0), SUB_BATCH_ROWS);
}
