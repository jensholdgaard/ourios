//! RFC 0036 §5 — write-side layout, the four compaction-side scenarios.
//!
//! RFC0036.1 and RFC0036.4 are green (the sorted-compaction slice);
//! the remaining stubs are `#[ignore]`d so the default run stays green
//! while the RFC is red, each naming the green slice that discharges
//! it. The §6 merge property's forced-spill / hierarchical half lives
//! with the machinery in `compaction.rs` (the spill path needs the
//! internal tuning seam); the public-API half is here.
//!
//! Placement note: RFC0036.1/.3/.4/.5 live here because the machinery
//! they gate — the §3.2 sort-run merge, the §3.3 compacted row-group
//! threshold, and the §3.4 `sorting_columns` declaration — is
//! `compaction.rs`/`writer.rs` code (RFC 0036 §6). RFC0036.2's
//! in-repo slice (the synthetic-hour scanned-count bound via the
//! RFC 0016 counters) lives with the querier counter assertions in
//! `ourios-querier/tests/it/rfc0036_window_materialization.rs`.

use std::path::Path;

use bytes::Bytes;
use ourios_core::audit::ParamType;
use ourios_core::otlp::{AnyValue, KeyValue, any_value};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::promoted::{RESOURCE_PREFIX, SERVICE_NAME_KEY, project_string_value};
use ourios_parquet::{
    COMPACTED_ROW_GROUP_FLUSH_BYTES, MANIFEST_FILENAME, Manifest, PartitionKey, Reader,
    SUB_BATCH_ROWS, Store, Writer, columns, compact_partition,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::{ParquetMetaData, RowGroupMetaData, SortingColumn};
use parquet::file::statistics::Statistics;

/// 2026-04-02T10:00:00 UTC — every fixture time is an in-hour offset
/// from this, so all records share one partition.
const HOUR10_START: u64 = 1_775_124_000_000_000_000;

/// A record whose promoted `service.name` is `service` and whose
/// `body` carries `payload` (the size lever for row-group rotation).
/// `id` rides in the single param so every row stays distinguishable.
fn rec(service: &str, ts_ns: u64, id: u64, payload: String) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id: id,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: ts_ns,
        observed_time_unix_nano: Some(ts_ns + 1_000),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: vec![KeyValue {
            key: SERVICE_NAME_KEY.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(service.to_string())),
            }),
            ..Default::default()
        }],
        trace_id: None,
        span_id: None,
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: id.to_string(),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: Some(payload),
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// Deterministic high-entropy printable-ASCII payload (xorshift64) —
/// near-incompressible so encoded row-group sizes track the raw bytes.
fn payload(seed: u64, len: usize) -> String {
    let mut x = seed | 1;
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.push(char::from(
            b'!' + u8::try_from(x % 94).expect("0..94 fits u8"),
        ));
    }
    s
}

/// The §3.1 sort key of a decoded row, for order/multiset assertions.
fn sort_key(r: &MinedRecord) -> (Option<String>, u64, String) {
    (
        project_string_value(&r.resource_attributes, SERVICE_NAME_KEY).map(str::to_owned),
        r.time_unix_nano,
        r.params[0].value.clone(),
    )
}

/// Write `records` as one committed ingest-side file, returning
/// `(basename, object key)` — the key is the store object key the
/// writer published under, not a filesystem path.
fn write_input(store: &Store, part: &PartitionKey, records: &[MinedRecord]) -> (String, String) {
    let mut w = Writer::open_in(store, part.clone()).expect("open writer");
    w.append_records(records).expect("append");
    let written = w.close().expect("close");
    let name = written
        .key
        .rsplit('/')
        .next()
        .expect("key has a basename")
        .to_owned();
    (name, written.key)
}

/// Fetch the consolidated file's bytes after a committed compaction.
fn consolidated_bytes(store: &Store, part: &PartitionKey, file: &str) -> Vec<u8> {
    let prefix = partition_prefix(part);
    store
        .get_blocking(&format!("{prefix}/{file}"))
        .expect("get consolidated")
}

/// The partition's `/`-delimited object-key prefix (what the writer
/// publishes under).
fn partition_prefix(part: &PartitionKey) -> String {
    part.data_path(Path::new(""))
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// Leaf index of the named top-level column in the file's Parquet
/// schema, from a row group's column-chunk paths.
fn leaf_index(rg: &RowGroupMetaData, name: &str) -> usize {
    (0..rg.num_columns())
        .find(|&i| rg.column(i).column_path().string() == name)
        .unwrap_or_else(|| panic!("column `{name}` has a leaf"))
}

/// A row group's `service.name` min/max statistics as UTF-8.
fn service_min_max(rg: &RowGroupMetaData, svc_leaf: usize) -> (String, String) {
    let stats = rg
        .column(svc_leaf)
        .statistics()
        .expect("service.name statistics present");
    let Statistics::ByteArray(v) = stats else {
        panic!("service.name statistics are byte-array, got {stats:?}");
    };
    let min = v.min_opt().expect("min").as_utf8().expect("utf8 min");
    let max = v.max_opt().expect("max").as_utf8().expect("utf8 max");
    (min.to_owned(), max.to_owned())
}

/// Build a multi-service partition and write it as two ingest-side
/// input files, each holding a round-robin-interleaved half so neither
/// is service- or time-clustered on its own. Returns the full row set
/// (the compaction's expected multiset), the partition key, and the
/// object key of the first input file. Each service's rows carry
/// unique, per-service-ascending times on a 10 ms grid (the largest
/// service stays inside hour 10) with a per-service 77 ns offset (≪ the
/// grid) keeping times distinct across services.
fn seed_two_input_partition(
    store: &Store,
    plan: &[(&str, u64)],
    body_len: usize,
) -> (Vec<MinedRecord>, PartitionKey, String) {
    let mut per_service: Vec<Vec<MinedRecord>> = Vec::new();
    let mut id: u64 = 0;
    for (svc_idx, (service, rows)) in plan.iter().enumerate() {
        let mut recs = Vec::new();
        for i in 0..*rows {
            id += 1;
            let ts = HOUR10_START + i * 10_000_000 + u64::try_from(svc_idx).expect("small") * 77;
            recs.push(rec(service, ts, id, payload(id, body_len)));
        }
        per_service.push(recs);
    }
    let mut interleaved: Vec<MinedRecord> = Vec::new();
    let mut cursors: Vec<std::vec::IntoIter<MinedRecord>> =
        per_service.into_iter().map(Vec::into_iter).collect();
    loop {
        let mut any = false;
        for cursor in &mut cursors {
            if let Some(r) = cursor.next() {
                interleaved.push(r);
                any = true;
            }
        }
        if !any {
            break;
        }
    }
    let part = PartitionKey::derive(&interleaved[0]).expect("derive partition");
    let file_a: Vec<MinedRecord> = interleaved.iter().step_by(2).cloned().collect();
    let file_b: Vec<MinedRecord> = interleaved.iter().skip(1).step_by(2).cloned().collect();
    let (_, key_a) = write_input(store, &part, &file_a);
    write_input(store, &part, &file_b);
    (interleaved, part, key_a)
}

/// Scenario RFC0036.1 — compacted layout (clustering + sizing + declaration).
/// See `docs/rfcs/0036-write-side-layout.md` §5.
#[test]
fn rfc0036_1_compacted_layout() {
    // Three services sized so the *encoded* corpus (~53 MiB, the estimate
    // the rotation predicate actually watches — high-entropy bodies still
    // ZSTD ~2.4×, so uncompressed is ~128 MiB) crosses the 32 MiB
    // compacted threshold once: svc-a alone fills the first group, so the
    // seal lands at the a→b boundary and every group's service min/max
    // spans at most a boundary pair.
    const BODY_LEN: usize = 4096;
    let plan: [(&str, u64); 3] = [("svc-a", 18_500), ("svc-b", 13_500), ("svc-c", 900)];

    let bucket = tempfile::TempDir::new().expect("temp");
    let store = Store::local(bucket.path()).expect("local store");
    let (interleaved, part, key_a) = seed_two_input_partition(&store, &plan, BODY_LEN);
    let expected_rows = interleaved.len();

    // §3.4: ingest-side files declare NOTHING.
    let input_bytes = store.get_blocking(&key_a).expect("get input");
    let input_meta = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(input_bytes))
        .expect("open input")
        .metadata()
        .clone();
    for rg in input_meta.row_groups() {
        assert!(
            rg.sorting_columns().is_none(),
            "ingest-side files declare no sorting_columns",
        );
    }

    let outcome = compact_partition(&store, &part).expect("compact");
    let committed = outcome.committed.expect("committed");
    let bytes = consolidated_bytes(&store, &part, &committed.file);

    // --- Footer assertions (ParquetMetaData) ---
    let meta: ParquetMetaData =
        ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes.clone()))
            .expect("open consolidated")
            .metadata()
            .as_ref()
            .clone();
    assert!(
        meta.num_row_groups() >= 2,
        "the corpus crosses the compacted threshold, so rotation must \
         have produced multiple row groups (got {})",
        meta.num_row_groups(),
    );

    let service_column = format!("{RESOURCE_PREFIX}{SERVICE_NAME_KEY}");
    let first = &meta.row_groups()[0];
    let svc_leaf = leaf_index(first, &service_column);
    let time_leaf = leaf_index(first, columns::TIME_UNIX_NANO);
    let declared_keys = vec![
        SortingColumn {
            column_idx: i32::try_from(svc_leaf).expect("leaf fits i32"),
            descending: false,
            nulls_first: true,
        },
        SortingColumn {
            column_idx: i32::try_from(time_leaf).expect("leaf fits i32"),
            descending: false,
            nulls_first: false,
        },
    ];

    // Size bounds: the rotation predicate watches the writer's encoded
    // in-progress estimate, so the tight bound is on encoded bytes; the
    // §5 uncompressed bound follows via the file's own encode ratio.
    // "One sub-batch's bounded overshoot" = 2 × SUB_BATCH_ROWS ×
    // per-row bytes (2× slack for the in-progress estimate's buffered
    // component).
    let uncompressed_total: u64 = meta
        .row_groups()
        .iter()
        .map(|rg| u64::try_from(rg.total_byte_size()).expect("non-negative"))
        .sum();
    let encoded_total: u64 = meta
        .row_groups()
        .iter()
        .map(|rg| u64::try_from(rg.compressed_size()).expect("non-negative"))
        .sum();
    let rows = u64::try_from(expected_rows).expect("row count");
    let sub_batch = u64::try_from(SUB_BATCH_ROWS).expect("sub-batch rows");
    let threshold = u64::try_from(COMPACTED_ROW_GROUP_FLUSH_BYTES).expect("threshold");
    let encoded_bound = threshold + 2 * sub_batch * (encoded_total / rows);
    let uncompressed_bound = threshold * uncompressed_total / encoded_total
        + 2 * sub_batch * (uncompressed_total / rows);

    let services: Vec<&str> = plan.iter().map(|(s, _)| *s).collect();
    for rg in meta.row_groups() {
        assert_eq!(
            rg.sorting_columns(),
            Some(&declared_keys),
            "every row group declares the §3.1 keys 1–2",
        );
        let enc = u64::try_from(rg.compressed_size()).expect("non-negative");
        let unc = u64::try_from(rg.total_byte_size()).expect("non-negative");
        assert!(
            enc <= encoded_bound,
            "row group encoded size {enc} within threshold + sub-batch \
             overshoot {encoded_bound}",
        );
        assert!(
            unc <= uncompressed_bound,
            "row group uncompressed size {unc} within the ratio-adjusted \
             threshold + sub-batch overshoot {uncompressed_bound}",
        );
        // Clustering: min/max span at most a boundary pair of services.
        let (min, max) = service_min_max(rg, svc_leaf);
        let min_pos = services.iter().position(|s| *s == min).expect("known min");
        let max_pos = services.iter().position(|s| *s == max).expect("known max");
        assert!(
            max_pos.saturating_sub(min_pos) <= 1,
            "row group service min/max ({min}, {max}) spans more than a \
             boundary pair",
        );
    }

    // --- Decode assertions: §3.1 key order + multiset == inputs' union ---
    let got = Reader::open_partition_bytes(Bytes::from(bytes), part.clone(), &committed.file)
        .expect("open consolidated")
        .read_all()
        .expect("read consolidated");
    assert_eq!(got.len(), expected_rows, "row count conserved");
    assert_key_order_and_multiset(got, interleaved);
}

/// Assert the decoded rows are in §3.1 key order and their multiset
/// equals `expected`'s (order-independent).
fn assert_key_order_and_multiset(got: Vec<MinedRecord>, expected: Vec<MinedRecord>) {
    for pair in got.windows(2) {
        let (a, b) = (sort_key(&pair[0]), sort_key(&pair[1]));
        assert!(
            (a.0.as_deref(), a.1) <= (b.0.as_deref(), b.1),
            "§3.1 key order violated: {:?} then {:?}",
            (a.0, a.1),
            (b.0, b.1),
        );
    }
    let mut got_sorted = got;
    got_sorted.sort_by_key(sort_key);
    let mut expected = expected;
    expected.sort_by_key(sort_key);
    assert_eq!(
        got_sorted, expected,
        "row multiset equals the inputs' union"
    );
}

/// The public-API half of the RFC0036.1 §6 merge property: for
/// arbitrary service/time/duplicate-key mixes across ≥ 2 input files,
/// `compact_partition` preserves the row multiset and yields §3.1
/// (service, time) order with absent-service rows first. The
/// tie-break clause and the spill/hierarchical paths are pinned by the
/// forced-spill proptest beside the machinery in `compaction.rs`.
/// See `docs/rfcs/0036-write-side-layout.md` §6.
mod rfc0036_1_merge_property {
    use super::*;

    fn small_rec(service: Option<&str>, ts_ns: u64, id: u64) -> MinedRecord {
        let mut r = rec(service.unwrap_or(""), ts_ns, id, format!("body-{id}"));
        if service.is_none() {
            r.resource_attributes = Vec::new();
        }
        r
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        #[test]
        fn multiset_preserved_and_sorted(
            files in proptest::collection::vec(
                proptest::collection::vec((0usize..4, 0u64..6), 1..=12usize),
                2..=5usize,
            )
        ) {
            let services = [None, Some("svc-a"), Some("svc-b"), Some("svc-c")];
            let bucket = tempfile::TempDir::new().expect("temp");
            let store = Store::local(bucket.path()).expect("local store");
            let mut id: u64 = 0;
            let mut expected: Vec<MinedRecord> = Vec::new();
            let mut part: Option<PartitionKey> = None;
            for file in &files {
                let recs: Vec<MinedRecord> = file
                    .iter()
                    .map(|(svc, toff)| {
                        id += 1;
                        small_rec(services[*svc], HOUR10_START + toff * 1_000, id)
                    })
                    .collect();
                let p = part
                    .get_or_insert_with(|| {
                        PartitionKey::derive(&recs[0]).expect("derive partition")
                    })
                    .clone();
                expected.extend(recs.iter().cloned());
                write_input(&store, &p, &recs);
            }
            let part = part.expect("at least one file");

            let outcome = compact_partition(&store, &part).expect("compact");
            let committed = outcome.committed.expect("≥2 files ⇒ a commit");
            let bytes = consolidated_bytes(&store, &part, &committed.file);
            let got = Reader::open_partition_bytes(
                Bytes::from(bytes),
                part.clone(),
                &committed.file,
            )
            .expect("open consolidated")
            .read_all()
            .expect("read consolidated");

            for pair in got.windows(2) {
                let (a, b) = (sort_key(&pair[0]), sort_key(&pair[1]));
                proptest::prop_assert!(
                    (a.0.as_deref(), a.1) <= (b.0.as_deref(), b.1),
                    "§3.1 key order violated (absent-service first): {:?} then {:?}",
                    (a.0, a.1),
                    (b.0, b.1),
                );
            }
            let mut got = got;
            got.sort_by_key(sort_key);
            expected.sort_by_key(sort_key);
            proptest::prop_assert_eq!(got, expected, "multiset equals the inputs' union");
        }
    }
}

/// Names of the partition's live data files, per its `manifest.json`.
fn live_manifest_files(store: &Store, part: &PartitionKey) -> Vec<String> {
    let key = format!("{}/{MANIFEST_FILENAME}", partition_prefix(part));
    let (manifest, _etag) = Manifest::read_with_etag(store, &key)
        .expect("read manifest")
        .expect("manifest present after a committed compaction");
    manifest.files
}

/// Count committed `*.parquet` objects physically present under the
/// partition prefix (what the H4 small-file detector counts).
fn on_disk_parquet_count(store: &Store, part: &PartitionKey) -> usize {
    store
        .list_blocking(Some(&partition_prefix(part)))
        .expect("list")
        .into_iter()
        .filter(|k| k.ends_with(".parquet"))
        .count()
}

/// Scenario RFC0036.3 — compaction properties preserved (D2 / D3 / memory).
/// See `docs/rfcs/0036-write-side-layout.md` §5.
///
/// This test pins the **D3-unchanged** half structurally: the sorted
/// compaction still emits exactly one output file per partition and drives
/// the H4 small-file *count* down to one — the file-band property D3
/// measures, preserved across the RFC 0036 sort (the row-group threshold
/// dropped, but the *file* band is untouched — §3.3). The absolute
/// 256 MiB–2 GiB file-size band needs real corpus volume on
/// `baseline-8vcpu-32gib` (`docs/benchmarks.md` §9.7 / §9.25), exactly as
/// the `rfc0009_1_*` structural test defers it; a synthetic unit-scale run
/// produces a sub-band file, so the size band is asserted in the bench, not
/// here.
///
/// The other two halves live where their machinery is. The **memory
/// bound** (forced-spill peak decoded residency = one input + F×batch,
/// never whole-partition) is
/// `compaction::tests::rfc0036_3_forced_spill_peak_is_one_input_not_whole_partition`,
/// which needs the internal `SortTuning` spill seam. The **D2 throughput
/// band** is a wall-clock measurement, recorded in the `ourios-bench`
/// `compaction` bench and `docs/benchmarks.md` §9.25 (indicative) —
/// deliberately *not* an in-repo gate (wall-clock gates flake; RFC 0036 §6).
#[test]
fn rfc0036_3_compaction_properties_preserved() {
    // A tens-of-files backlog (the §5 "tens of input files" shape) across
    // three services with interleaved times, so the sorted compaction has
    // real cross-file merge + clustering work.
    const FILES: usize = 24;
    const ROWS_PER_FILE: u64 = 40;
    const BODY_LEN: usize = 256;
    let services = ["svc-a", "svc-b", "svc-c"];

    let bucket = tempfile::TempDir::new().expect("temp");
    let store = Store::local(bucket.path()).expect("local store");

    let mut part: Option<PartitionKey> = None;
    let mut id: u64 = 0;
    let mut expected_rows = 0usize;
    for _ in 0..FILES {
        let recs: Vec<MinedRecord> = (0..ROWS_PER_FILE)
            .map(|i| {
                id += 1;
                let svc = services[usize::try_from(id).expect("id fits usize") % services.len()];
                // Interleave times across files so no input is self-sorted.
                let ts = HOUR10_START + (i * u64::try_from(FILES).expect("small")) + id % 7;
                rec(svc, ts, id, payload(id, BODY_LEN))
            })
            .collect();
        let p = part
            .get_or_insert_with(|| PartitionKey::derive(&recs[0]).expect("derive partition"))
            .clone();
        write_input(&store, &p, &recs);
        expected_rows += recs.len();
    }
    let part = part.expect("at least one file");

    let outcome = compact_partition(&store, &part).expect("compact");
    let committed = outcome.committed.expect("tens of files ⇒ a commit");

    // D3 (file band): one output file per partition, the small-file count
    // collapsed to one — physically, not merely manifest-hidden.
    assert_eq!(outcome.files_before, FILES, "every input was live");
    assert_eq!(
        outcome.rows,
        u64::try_from(expected_rows).expect("row count"),
        "compaction conserves every row across the collapse",
    );
    assert_eq!(
        live_manifest_files(&store, &part),
        vec![committed.file.clone()],
        "exactly one live file per partition (D3 unchanged)",
    );
    assert_eq!(outcome.gc_failures, 0, "every superseded input removed");
    assert_eq!(
        on_disk_parquet_count(&store, &part),
        1,
        "exactly one physical .parquet remains (H4 small-file count → 1)",
    );

    // The one output file is the sorted layout: every row group declares
    // the §3.4 sorting_columns and decodes in §3.1 order (D3 preserving
    // the RFC 0036 clustering, not reverting to append order).
    let bytes = consolidated_bytes(&store, &part, &committed.file);
    let meta = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes.clone()))
        .expect("open consolidated")
        .metadata()
        .clone();
    for rg in meta.row_groups() {
        assert!(
            rg.sorting_columns().is_some(),
            "compacted row groups still declare sorting_columns after the collapse",
        );
    }
    let got = Reader::open_partition_bytes(Bytes::from(bytes), part.clone(), &committed.file)
        .expect("open consolidated")
        .read_all()
        .expect("read consolidated");
    assert_eq!(got.len(), expected_rows, "row count conserved on read-back");
    for pair in got.windows(2) {
        let (a, b) = (sort_key(&pair[0]), sort_key(&pair[1]));
        assert!(
            (a.0.as_deref(), a.1) <= (b.0.as_deref(), b.1),
            "§3.1 key order preserved through compaction: {:?} then {:?}",
            (a.0, a.1),
            (b.0, b.1),
        );
    }
}

/// Scenario RFC0036.4 — determinism (the harness's contract).
/// See `docs/rfcs/0036-write-side-layout.md` §5.
#[test]
fn rfc0036_4_rebuild_byte_identity() {
    let bucket_a = tempfile::TempDir::new().expect("temp a");
    let bucket_b = tempfile::TempDir::new().expect("temp b");
    let store_a = Store::local(bucket_a.path()).expect("store a");
    let store_b = Store::local(bucket_b.path()).expect("store b");

    // Three inputs with interleaved times and deliberate duplicate
    // (service, time) keys across files, so the §3.1 tie-break is on
    // the determinism path.
    let mut id: u64 = 0;
    let mut files: Vec<Vec<MinedRecord>> = Vec::new();
    for _ in 0..3 {
        let mut recs = Vec::new();
        for (svc, toff) in [
            ("svc-b", 5_u64),
            ("svc-a", 5),
            ("svc-a", 1),
            ("svc-b", 1),
            ("svc-a", 5),
        ] {
            id += 1;
            recs.push(rec(svc, HOUR10_START + toff * 1_000, id, payload(id, 64)));
        }
        files.push(recs);
    }
    let part = PartitionKey::derive(&files[0][0]).expect("derive partition");

    // Same bytes, same names: write into store A, mirror byte-for-byte
    // into store B.
    let mut names: Vec<String> = Vec::new();
    for recs in &files {
        let (name, key) = write_input(&store_a, &part, recs);
        let bytes = store_a.get_blocking(&key).expect("get input");
        store_b.put_blocking(&key, bytes).expect("mirror input");
        names.push(name);
    }

    // The compactor resolves its listing through the manifest when one
    // exists, so a shuffled manifest IS a store returning listings in a
    // different order: seed A sorted, B reversed.
    let mut sorted = names.clone();
    sorted.sort();
    let mut reversed = sorted.clone();
    reversed.reverse();
    let prefix = partition_prefix(&part);
    for (store, listing) in [(&store_a, sorted), (&store_b, reversed)] {
        let manifest = Manifest {
            generation: 1,
            files: listing,
        };
        store
            .put_blocking(
                &format!("{prefix}/{MANIFEST_FILENAME}"),
                manifest.to_json().expect("manifest json"),
            )
            .expect("seed manifest");
    }

    let committed_a = compact_partition(&store_a, &part)
        .expect("compact a")
        .committed
        .expect("commit a");
    let committed_b = compact_partition(&store_b, &part)
        .expect("compact b")
        .committed
        .expect("commit b");

    let bytes_a = consolidated_bytes(&store_a, &part, &committed_a.file);
    let bytes_b = consolidated_bytes(&store_b, &part, &committed_b.file);
    assert_eq!(bytes_a.len(), bytes_b.len(), "same output length");
    assert!(
        bytes_a == bytes_b,
        "the two consolidated outputs are byte-identical across listing orders",
    );
}

/// Scenario RFC0036.5 — no read-path or schema regression.
/// See `docs/rfcs/0036-write-side-layout.md` §5.
#[test]
#[ignore = "RFC0036.5 stub — implemented in the compat green slice (pre-RFC fixture reads + B1/B2 + frozen-gate rerun)"]
fn rfc0036_5_no_read_path_or_schema_regression() {
    todo!(
        "RFC0036.5 — stores built before and after the change: B1/B2 \
         and the frozen RFC 0031 comparative gates run against the \
         post-change store and every frozen gate still passes, with \
         the L1/L3/L4 pairs not degraded beyond the documented \
         Loki-wobble band and query results identical row-sets; a \
         pre-RFC-0036 fixture file (no sorting_columns, 128 MiB row \
         groups) reads without error or special-casing — no migration \
         exists because none is needed (CLAUDE.md §3.5)"
    );
}
