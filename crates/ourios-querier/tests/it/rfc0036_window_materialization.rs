//! RFC 0036 §5 — window-query materialization (RFC0036.2), the
//! in-repo slice.
//!
//! Placement note: the RFC 0016 scanned/pruned row-group counts this
//! scenario bounds are asserted from this harness (`execution.rs`), so
//! the synthetic-hour CI slice co-locates with them (RFC 0036 §6). The
//! full comparative arm — the L6-shape pair on the v8 corpus through
//! the RFC 0031 dispatch and the before/after §9 bytes diagnostic — is
//! a paid `baseline-8vcpu-32gib` measurement in `ourios-bench`,
//! deferred to `validated`, matching how RFC 0033 handled its `.6`
//! comparative arm and RFC0036.5's comparative half. The other four
//! scenarios live with the compaction code in
//! `ourios-parquet/tests/it/rfc0036_write_side_layout.rs`.

use std::fs::File;

use ourios_core::audit::ParamType;
use ourios_core::otlp::{AnyValue, KeyValue, any_value};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::promoted::{RESOURCE_PREFIX, SERVICE_NAME_KEY};
use ourios_parquet::{
    COMPACTED_ROW_GROUP_FLUSH_BYTES, PartitionKey, Store, Writer, columns, compact_partition,
};
use ourios_querier::Querier;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::RowGroupMetaData;
use parquet::file::statistics::Statistics;

/// 2026-04-02T10:00:00 UTC — every fixture time is an in-hour offset
/// from this, so all records share one partition.
const HOUR10_START: u64 = 1_775_124_000_000_000_000;

/// A `now` reference and default window comfortably covering the
/// fixture hour, so the DSL `range(...)` is the only time bound that
/// narrows the query (RFC 0002).
const DEFAULT_WINDOW_NS: u64 = 30 * 24 * 3_600_000_000_000;
const NOW: u64 = HOUR10_START + 24 * 3_600_000_000_000;

/// A record whose promoted `service.name` is `service` and whose
/// `body` carries `payload` (the size lever for row-group rotation).
/// `id` rides in the single param so every row stays distinguishable.
fn rec(service: &str, ts_ns: u64, id: u64, payload: String) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("a"),
        template_id: id,
        template_version: 1,
        severity_number: 9,
        // Optional metadata the window query never touches — left None so
        // ~58k fixture rows don't each allocate three identical strings.
        severity_text: None,
        scope_name: None,
        scope_version: None,
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
/// near-incompressible so encoded row-group sizes track the raw bytes
/// (the same size lever RFC0036.1 uses to cross the compacted
/// threshold; `ourios-parquet` RFC0036.1 test).
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

/// Leaf index of the named top-level column in a row group.
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

/// A row group's `time_unix_nano` min/max (physical `Int64` nanos).
fn time_min_max(rg: &RowGroupMetaData, time_leaf: usize) -> (u64, u64) {
    let stats = rg
        .column(time_leaf)
        .statistics()
        .expect("time_unix_nano statistics present");
    let Statistics::Int64(v) = stats else {
        panic!("time_unix_nano statistics are int64, got {stats:?}");
    };
    let min = u64::try_from(*v.min_opt().expect("min")).expect("non-negative time");
    let max = u64::try_from(*v.max_opt().expect("max")).expect("non-negative time");
    (min, max)
}

/// Write `records` as one committed ingest-side file.
fn write_input(store: &Store, part: &PartitionKey, records: &[MinedRecord]) {
    let mut w = Writer::open_in(store, part.clone()).expect("open writer");
    w.append_records(records).expect("append");
    w.close().expect("close");
}

/// Seed a one-hour partition from `plan` (each `(service, rows)` pair's
/// rows ascend on the `grid_ns` grid), split across two interleaved
/// ingest-side files so neither is service- or time-clustered on its
/// own, then compact. Returns the committed file's basename.
fn seed_and_compact(
    store: &Store,
    part: &PartitionKey,
    plan: &[(&str, u64)],
    body_len: usize,
    grid_ns: u64,
) -> String {
    // Round-robin across services, alternately assigning to file A/B, so
    // neither input is service- or time-clustered on its own — and we
    // never materialise the whole corpus plus two clones (peak is the two
    // files, not three copies). Compaction sorts globally, so the input
    // distribution does not affect the compacted layout; every
    // (service, time) key is unique, so payload length is fixed per row
    // regardless of `id`, keeping the output byte-identical.
    let max_rows = plan.iter().map(|&(_, rows)| rows).max().unwrap_or(0);
    let mut file_a: Vec<MinedRecord> = Vec::new();
    let mut file_b: Vec<MinedRecord> = Vec::new();
    let mut id: u64 = 0;
    let mut to_a = true;
    for i in 0..max_rows {
        for &(service, rows) in plan {
            if i < rows {
                id += 1;
                let r = rec(
                    service,
                    HOUR10_START + i * grid_ns,
                    id,
                    payload(id, body_len),
                );
                if to_a {
                    file_a.push(r);
                } else {
                    file_b.push(r);
                }
                to_a = !to_a;
            }
        }
    }
    write_input(store, part, &file_a);
    write_input(store, part, &file_b);
    compact_partition(store, part)
        .expect("compact")
        .committed
        .expect("≥2 inputs ⇒ a commit")
        .file
}

/// The queried service's bytes within the window: the compressed sizes
/// of the row groups a `service == target AND time ∈ window` scan can
/// NOT prune (target within the group's `service.name` min/max AND the
/// group's time range overlaps the window). Returns their indices and
/// the byte sum (`B_sw`). The §3.1 sort places one service's window in
/// a contiguous run of row groups, so the indices are contiguous.
fn window_service_bytes(
    meta: &parquet::file::metadata::ParquetMetaData,
    target: &str,
    window: &std::ops::Range<u64>,
) -> (Vec<usize>, u64) {
    let service_column = format!("{RESOURCE_PREFIX}{SERVICE_NAME_KEY}");
    let svc_leaf = leaf_index(&meta.row_groups()[0], &service_column);
    let time_leaf = leaf_index(&meta.row_groups()[0], columns::TIME_UNIX_NANO);
    let mut overlapping: Vec<usize> = Vec::new();
    let mut b_sw: u64 = 0;
    for (i, rg) in meta.row_groups().iter().enumerate() {
        let (smin, smax) = service_min_max(rg, svc_leaf);
        let (tmin, tmax) = time_min_max(rg, time_leaf);
        let service_overlaps = smin.as_str() <= target && target <= smax.as_str();
        let time_overlaps = tmin < window.end && window.start <= tmax;
        if service_overlaps && time_overlaps {
            overlapping.push(i);
            b_sw += u64::try_from(rg.compressed_size()).expect("non-negative");
        }
    }
    (overlapping, b_sw)
}

/// Scenario RFC0036.2 — window-query materialization (the point).
/// See `docs/rfcs/0036-write-side-layout.md` §5.
///
/// A synthetic v8-shape hour (many services, promoted `service.name`)
/// is compacted into one clustered, `sorting_columns`-declaring file
/// whose row groups rotate at [`COMPACTED_ROW_GROUP_FLUSH_BYTES`]. The
/// L6-shape query — one service, a k-row time window — then scans only
/// the row groups that hold that service's window (plus at most two
/// boundary groups), not the whole hour: the RFC 0016
/// `row_groups_scanned` count is bounded by `ceil(B_sw / T) + 2`, with
/// **B_sw** the queried service's bytes within the window (summed from
/// the compacted footer: the §3.1 sort places one service's window in
/// *contiguous* row groups) and **T** the compacted threshold. Both
/// `B_sw` and T are read from the file/const, never hardcoded.
///
/// This is the CI-testable half. The comparative arm — the same L6
/// pair on the v8 corpus through the RFC 0031 dispatch, and the
/// before/after materialization-**bytes** §9 diagnostic (total scanned
/// minus the RFC 0033 registry acquisition) — is a paid
/// `baseline-8vcpu-32gib` bench measurement deferred to `validated`
/// (§2.2: the ~188 KB registry floor makes bytes a diagnostic, so the
/// *gate* is this scanned-row-group bound, not a bytes ratio).
#[tokio::test]
async fn rfc0036_2_window_materialization_bound() {
    // A one-hour partition, three promoted services sized so the
    // compacted file rotates into several row groups at the 32 MiB
    // compacted threshold (BODY_LEN high-entropy, so encoded size
    // tracks raw bytes — the RFC0036.1 size lever). `svc-m` is the
    // queried service: it is fully contained in the a→z boundary row
    // group, so a `service == "svc-m"` window query scans that one
    // group and prunes the pure-`svc-a`/`svc-z` groups by the §3.1
    // clustering's tight per-row-group `service.name` statistics.
    const BODY_LEN: usize = 4096;
    const TARGET: &str = "svc-m";
    /// The k-row window: `[HOUR10_START, HOUR10_START + WINDOW_NS)`.
    const WINDOW_NS: u64 = 30_000_000_000; // 30 s
    const GRID_NS: u64 = 10_000_000; // 10 ms between a service's rows
    let plan: [(&str, u64); 3] = [("svc-a", 20_000), (TARGET, 18_500), ("svc-z", 20_000)];

    let bucket = tempfile::TempDir::new().expect("temp");
    let store = Store::local(bucket.path()).expect("local store");
    let part = PartitionKey::derive(&rec(TARGET, HOUR10_START, 1, String::new()))
        .expect("derive partition");
    let committed = seed_and_compact(&store, &part, &plan, BODY_LEN, GRID_NS);

    // --- Footer: B_sw (queried service's bytes in the window) and the
    // §3.1 contiguity of that service's clustered run ---
    let path = part.data_path(bucket.path()).join(&committed);
    let meta = ParquetRecordBatchReaderBuilder::try_new(File::open(&path).expect("open"))
        .expect("read footer")
        .metadata()
        .clone();
    let total_row_groups = meta.num_row_groups();
    assert!(
        total_row_groups >= 3,
        "the corpus must rotate into several row groups so a one-service \
         window is a small fraction of the hour (got {total_row_groups})",
    );

    let window = HOUR10_START..HOUR10_START + WINDOW_NS;
    let (overlapping, b_sw) = window_service_bytes(&meta, TARGET, &window);
    assert!(!overlapping.is_empty(), "the answer lives in ≥ 1 row group");
    // §3.1 contiguity: one service's window occupies a *contiguous* run
    // of row groups (the sorted layout's claim RFC0036.2 rests on).
    assert!(
        overlapping.windows(2).all(|w| w[1] == w[0] + 1),
        "the queried service's window is not contiguous in the sorted \
         layout: groups {overlapping:?}",
    );
    assert!(
        overlapping.len() < total_row_groups,
        "the window must not span every row group — otherwise there is \
         no materialization win to bound ({}/{total_row_groups})",
        overlapping.len(),
    );

    // --- The L6-shape query and the scanned-row-group bound ---
    let t = u64::try_from(COMPACTED_ROW_GROUP_FLUSH_BYTES).expect("threshold fits u64");
    // ceil(B_sw / T) + 2: the groups that hold the answer, plus at most
    // two boundary groups.
    let bound = b_sw.div_ceil(t) + 2;

    let src =
        format!("service == \"{TARGET}\" | range(2026-04-02T10:00:00Z, 2026-04-02T10:00:30Z)");
    let query = ourios_querier::dsl::parse(&src).expect("parse");
    let r = Querier::new(bucket.path())
        .run_query(
            &query,
            &TenantId::new("a"),
            NOW,
            DEFAULT_WINDOW_NS,
            Some(&ourios_core::alias::AliasMap::new()),
        )
        .await
        .expect("run_query");

    // The window holds exactly the target service's first
    // WINDOW_NS/GRID_NS rows (its rows ascend on the 10 ms grid from
    // HOUR10_START).
    let expected_rows = WINDOW_NS / GRID_NS;
    assert_eq!(
        r.rows, expected_rows,
        "the window selects the target service's k rows",
    );

    // The gate (RFC0036.2): scanned row groups ≤ ceil(B_sw / T) + 2.
    assert!(
        r.stats.row_groups_scanned <= bound,
        "window query scanned {} row groups, over the ceil(B_sw / T) + 2 \
         bound {bound} (B_sw={b_sw}, T={t}); stats={:?}",
        r.stats.row_groups_scanned,
        r.stats,
    );
    // The materialization point: the query fetches a few row groups,
    // NOT the whole hour. Pruning removed the rest.
    assert!(
        r.stats.row_groups_scanned >= 1,
        "the answer's row group is scanned; stats={:?}",
        r.stats,
    );
    let total_seen = r.stats.row_groups_scanned + r.stats.row_groups_pruned;
    assert!(
        r.stats.row_groups_scanned < total_seen,
        "the one-service window prunes strictly more than it scans — the \
         whole hour is not materialized; stats={:?}",
        r.stats,
    );
}
