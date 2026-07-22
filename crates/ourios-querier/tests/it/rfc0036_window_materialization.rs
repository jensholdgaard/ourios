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
    PartitionKey, Store, Writer, adaptive_flush_bytes, columns, compact_partition,
    compact_partition_with_flush_threshold,
};
use ourios_querier::Querier;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::metadata::RowGroupMetaData;
use parquet::file::statistics::Statistics;

/// 2026-04-02T10:00:00 UTC — every fixture time is an in-hour offset
/// from this, so all records share one partition.
const HOUR10_START: u64 = 1_775_124_000_000_000_000;

/// Each service in a plan gets its own disjoint time band — its plan
/// index × this width. Under the RFC 0036 §3.3 **adaptive** threshold the
/// compacted file rotates into several fine row groups (not the old
/// single coarse 32 MiB group), so a one-service window is a *contiguous*
/// run of groups only when neighbouring services do not share its time
/// span: otherwise the boundary row group joining two services (whose
/// rows would otherwise both start at time 0) has a `time_unix_nano`
/// min/max that spuriously overlaps the window and reads as a gap in the
/// survivor run. Disjoint bands make the sorted-by-(service, time) layout
/// genuinely time-clustered per service. 10 min ≫ any single service's
/// span (≤ ~200 s at the 10 ms grid), leaving a clean inter-band gap; the
/// three bands (≤ 30 min) stay inside the fixture hour.
const SERVICE_BAND_NS: u64 = 600_000_000_000; // 10 min

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
        .unwrap_or_else(|| panic!("column `{name}` not found in the row group schema"))
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
/// own, then compact. Returns `(committed file basename, summed live input
/// file bytes)` — the input total is the RFC 0036 §3.3
/// `estimated_output_bytes` the writer scaled the adaptive threshold from,
/// so callers recompute the exact `T` via [`adaptive_flush_bytes`].
fn seed_and_compact(
    store: &Store,
    part: &PartitionKey,
    plan: &[(&str, u64)],
    body_len: usize,
    grid_ns: u64,
) -> (String, u64) {
    // Round-robin across services, alternately assigning to file A/B, so
    // neither input is service- or time-clustered on its own — and we
    // never materialise the whole corpus plus two clones (peak is the two
    // files, not three copies). Compaction sorts globally and every
    // (service, time) key is unique, so the sorted output order is fixed
    // regardless of input distribution. Payloads are a fixed length of
    // near-incompressible bytes, so per-row encoded size — and thus the
    // row-group rotation boundaries and count the RFC0036.2 bound reads —
    // is stable no matter which `id`/content a row carries (the exact
    // bytes are not identical: content varies with `id`; B_sw and the
    // bound are measured live from the footer).
    let max_rows = plan.iter().map(|&(_, rows)| rows).max().unwrap_or(0);
    let mut file_a: Vec<MinedRecord> = Vec::new();
    let mut file_b: Vec<MinedRecord> = Vec::new();
    let mut id: u64 = 0;
    let mut to_a = true;
    for i in 0..max_rows {
        for (svc_idx, &(service, rows)) in plan.iter().enumerate() {
            if i < rows {
                id += 1;
                let band = u64::try_from(svc_idx).expect("plan index fits u64") * SERVICE_BAND_NS;
                let r = rec(
                    service,
                    HOUR10_START + band + i * grid_ns,
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
    // The two live ingest files' total — the adaptive threshold's input,
    // captured before compaction consumes them.
    let prefix = part
        .data_path(std::path::Path::new(""))
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let input_total: u64 = store
        .list_with_sizes_blocking(Some(&prefix))
        .expect("list input sizes")
        .iter()
        .filter(|(k, _)| k.ends_with(".parquet"))
        .map(|(_, size)| *size)
        .sum();
    let committed = compact_partition(store, part)
        .expect("compact")
        .committed
        .expect("≥2 inputs ⇒ a commit")
        .file;
    (committed, input_total)
}

/// The same one-hour corpus [`seed_and_compact`] builds, but as a
/// single flat Vec in ingest (round-robin) order — the shape a
/// partition has *before* compaction: one unsorted ingest-side file, no
/// `sorting_columns`, no per-service/-time clustering. Every `(service,
/// time)` key matches the compacted store's, so the same window query
/// returns the identical answer on both.
fn all_records(plan: &[(&str, u64)], body_len: usize, grid_ns: u64) -> Vec<MinedRecord> {
    let max_rows = plan.iter().map(|&(_, rows)| rows).max().unwrap_or(0);
    let total = usize::try_from(plan.iter().map(|&(_, rows)| rows).sum::<u64>()).expect("fits");
    let mut records: Vec<MinedRecord> = Vec::with_capacity(total);
    let mut id: u64 = 0;
    for i in 0..max_rows {
        for (svc_idx, &(service, rows)) in plan.iter().enumerate() {
            if i < rows {
                id += 1;
                let band = u64::try_from(svc_idx).expect("plan index fits u64") * SERVICE_BAND_NS;
                records.push(rec(
                    service,
                    HOUR10_START + band + i * grid_ns,
                    id,
                    payload(id, body_len),
                ));
            }
        }
    }
    records
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
/// whose row groups rotate at the RFC 0036 §3.3 **adaptive** threshold
/// (`input_total` / K). The L6-shape query — one service, a k-row time
/// window — then scans only the row groups that hold that service's
/// window (plus at most two boundary groups), not the whole hour: the
/// RFC 0016 `row_groups_scanned` count is bounded by `ceil(B_sw / T) + 2`,
/// with **B_sw** the queried service's bytes within the window (summed
/// from the compacted footer: the §3.1 sort places one service's window in
/// *contiguous* row groups) and **T** the adaptive threshold the writer
/// used ([`adaptive_flush_bytes`] of the input total `seed_and_compact`
/// returns). Both `B_sw` and T are recomputed from the file/inputs, never
/// hardcoded — so the bound tracks the layout the writer actually
/// produced, whatever the threshold resolves to.
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
    // compacted file rotates into several row groups at the adaptive
    // threshold (`input_total` / K, ~12 MiB here; BODY_LEN high-entropy, so
    // encoded size tracks raw bytes — the RFC0036.1 size lever). `svc-m` is
    // the queried service: it is fully contained in the a→z boundary row
    // groups, so a `service == "svc-m"` window query scans those and
    // prunes the pure-`svc-a`/`svc-z` groups by the §3.1 clustering's
    // tight per-row-group `service.name` statistics.
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
    let (committed, input_total) = seed_and_compact(&store, &part, &plan, BODY_LEN, GRID_NS);

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

    // `svc-m` is plan index 1, so its rows live in the second time band
    // (`1 × SERVICE_BAND_NS` = 10 min in) — the window and the DSL
    // `range(...)` below both target that band.
    let window_start = HOUR10_START + SERVICE_BAND_NS;
    let window = window_start..window_start + WINDOW_NS;
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
    // T is the adaptive threshold the writer used — recomputed from the
    // same input total, so the bound moves with the layout (§3.3).
    let t = u64::try_from(adaptive_flush_bytes(input_total)).expect("threshold fits u64");
    // ceil(B_sw / T) + 2: the groups that hold the answer, plus at most
    // two boundary groups.
    let bound = b_sw.div_ceil(t) + 2;

    let src =
        format!("service == \"{TARGET}\" | range(2026-04-02T10:10:00Z, 2026-04-02T10:10:30Z)");
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

    // The window is the target service's rows on the 10 ms grid from
    // HOUR10_START whose time falls in the half-open [start, start +
    // WINDOW_NS) range: k·GRID_NS < WINDOW_NS for k = 0, 1, …, i.e.
    // ceil(WINDOW_NS / GRID_NS) rows (div_ceil, not floor, so the count
    // stays correct if the window ever stops being an exact grid multiple).
    let expected_rows = WINDOW_NS.div_ceil(GRID_NS);
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

/// Read `file_path`'s footer and run `query` against the store at
/// `bucket_root`, returning `(row_group_total, window_survivors,
/// survivor_bytes, result)`. The survivor set and bytes are the RFC 0036 §9
/// materialization term ([`window_service_bytes`]); `result` carries the live
/// `row_groups_scanned` that cross-checks the footer prediction.
async fn measure_window(
    bucket_root: &std::path::Path,
    file_path: &std::path::Path,
    query: &ourios_querier::dsl::Query,
    target: &str,
    window: &std::ops::Range<u64>,
) -> (u64, Vec<usize>, u64, ourios_querier::QueryResult) {
    let meta = ParquetRecordBatchReaderBuilder::try_new(File::open(file_path).expect("open file"))
        .expect("read footer")
        .metadata()
        .clone();
    let total = u64::try_from(meta.num_row_groups()).expect("count fits u64");
    let (survivors, bytes) = window_service_bytes(&meta, target, window);
    let result = Querier::new(bucket_root)
        .run_query(
            query,
            &TenantId::new("a"),
            NOW,
            DEFAULT_WINDOW_NS,
            Some(&ourios_core::alias::AliasMap::new()),
        )
        .await
        .expect("run_query");
    (total, survivors, bytes, result)
}

/// RFC0036.2 — the before/after materialization measurement (the "cheap
/// A" for RFC 0036, per the maintainer decision recorded in the RFC's
/// §9). The v8 comparative harness builds one ingest file per partition,
/// so `compact_partition` no-ops there and RFC 0036's sort never runs —
/// the baseline comparative run was byte-identical pre/post-0036, so the
/// before/after must be measured in-repo on a genuinely-compacted store.
///
/// The **same** synthetic multi-service hour is materialised two ways
/// and hit with the same L6-shape window query (one service, a narrow
/// time range):
///
/// - **Before** — the uncompacted ingest-side store: every row in one
///   flat, unsorted file at the 128 MiB [`ROW_GROUP_FLUSH_BYTES`]
///   rotation, no `sorting_columns`. Every row group spans the whole
///   hour and every service, so nothing prunes: the window query
///   materialises the *entire* file (`scanned == total`).
/// - **After** — the compacted, `sorting_columns`-declaring store
///   ([`seed_and_compact`], adaptive-threshold row groups). The §3.1
///   clustering gives each row group tight
///   `service.name`/`time` statistics, so the same query prunes away most
///   of its groups (`scanned < total`).
///
/// The win is measured in **materialization bytes** — the RFC 0036 §9
/// diagnostic: "the column chunks of the row groups that survive pruning."
/// For each store that is the summed compressed size of the row groups a
/// `service == TARGET AND time ∈ window` scan can NOT prune (`window_service_bytes`,
/// the same footer computation the bound test trusts); the live query's
/// `row_groups_scanned` cross-checks that the engine prunes exactly as the
/// footer predicts (before scans the whole file, after prunes to a subset).
/// Row-group *counts* are not the honest axis: the ingest side rotates at
/// 128 MiB and compaction at the adaptive threshold (`input_total` / K,
/// RFC 0036 §3.3), so the compacted store deliberately holds more, smaller
/// groups (and, at these sub-threshold sizes, a physically larger file —
/// the smaller groups compress a little worse). The window still
/// materialises fewer bytes on the compacted store
/// because it prunes to a small survivor subset, so the assertion is
/// `after_bytes < before_bytes` with **identical `rows`** (pruning changed
/// the IO, not the answer). The full comparative bytes-vs-Loki arm (total
/// minus the RFC 0033 ~188 KB registry floor) stays the deferred
/// `baseline-8vcpu-32gib` bench arm; this is the in-repo evidence the win is
/// real.
#[tokio::test]
async fn rfc0036_2_materialization_before_after() {
    const BODY_LEN: usize = 4096;
    const TARGET: &str = "svc-m";
    // A k=100-row window (1 s at the 10 ms grid), matching the RFC 0036 §9
    // materialization illustration — a genuinely selective L6 "window browse",
    // narrow enough that the compacted survivor set collapses to the single
    // boundary row group holding svc-m's earliest rows.
    const WINDOW_NS: u64 = 1_000_000_000; // 1 s
    const GRID_NS: u64 = 10_000_000; // 10 ms between a service's rows
    let plan: [(&str, u64); 3] = [("svc-a", 20_000), (TARGET, 18_500), ("svc-z", 20_000)];

    // `svc-m` is plan index 1 → its rows live in the second time band
    // (`1 × SERVICE_BAND_NS` = 10 min in); the window and DSL `range(...)`
    // target that band.
    let src =
        format!("service == \"{TARGET}\" | range(2026-04-02T10:10:00Z, 2026-04-02T10:10:01Z)");
    let query = ourios_querier::dsl::parse(&src).expect("parse");
    let part = PartitionKey::derive(&rec(TARGET, HOUR10_START, 1, String::new()))
        .expect("derive partition");

    let window_start = HOUR10_START + SERVICE_BAND_NS;
    let window = window_start..window_start + WINDOW_NS;

    // --- Before: one unsorted ingest-side file, no manifest (glob path). The
    // materialization term (RFC 0036 §9) is the compressed column chunks of
    // the row groups a `service == TARGET AND time ∈ window` scan can NOT
    // prune; on the unsorted ingest file every row group spans the whole hour
    // and every service, so none prune — the survivors are the *whole file*. ---
    let before_bucket = tempfile::TempDir::new().expect("temp");
    let before_store = Store::local(before_bucket.path()).expect("local store");
    write_input(&before_store, &part, &all_records(&plan, BODY_LEN, GRID_NS));
    let before_dir = part.data_path(before_bucket.path());
    let mut before_files: Vec<std::path::PathBuf> = std::fs::read_dir(&before_dir)
        .expect("read partition dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "parquet"))
        .collect();
    // One `write_input` ⇒ exactly one ingest file; assert it so a stray
    // file can't silently make the measurement pick the wrong parquet.
    assert_eq!(
        before_files.len(),
        1,
        "the uncompacted store must hold exactly one ingest file, found {before_files:?}",
    );
    let before_path = before_files.pop().expect("one ingest file");
    let (before_total, before_survivors, before_bytes, before) =
        measure_window(before_bucket.path(), &before_path, &query, TARGET, &window).await;
    // The "before materialises the whole file" claim below rests on the
    // uncompacted ingest file being a single row group (this corpus is
    // ~100 MB encoded, under the 128 MiB ingest rotation). Make that
    // explicit: if the writer ever flushed multiple time-ordered groups
    // here, time-pruning could kick in and this test would fail in a
    // confusing way — assert the assumption so it fails loudly instead.
    assert_eq!(
        before_total, 1,
        "the uncompacted before-store is expected to be a single row group \
         (the whole-file-materialisation baseline); got {before_total}",
    );

    // --- After: the compacted, sorted store (built exactly as the bound test).
    // Same survivor computation: the §3.1 clustering gives each row group tight
    // `service.name`/`time` statistics, so the survivors are a contiguous
    // minority of the file's groups. ---
    let after_bucket = tempfile::TempDir::new().expect("temp");
    let after_store = Store::local(after_bucket.path()).expect("local store");
    let (committed, _input_total) = seed_and_compact(&after_store, &part, &plan, BODY_LEN, GRID_NS);
    let after_path = part.data_path(after_bucket.path()).join(&committed);
    let (after_total, after_survivors, after_bytes, after) =
        measure_window(after_bucket.path(), &after_path, &query, TARGET, &window).await;

    // Integer ×100 ratio (avoids a float cast under clippy::pedantic); read
    // as `N.NNx`, e.g. 198 ⇒ before materialises 1.98× the after bytes.
    // `u128` so the ×100 can't overflow if the fixture ever grows.
    let win_x100 = u128::from(before_bytes) * 100 / u128::from(after_bytes.max(1));
    eprintln!(
        "RFC0036.2 before/after: uncompacted survivors {}/{} groups, {} bytes \
         (query scanned {}); compacted survivors {}/{} groups, {} bytes (query \
         scanned {}); rows {} (identical); materialization-bytes win {}.{:02}x",
        before_survivors.len(),
        before_total,
        before_bytes,
        before.stats.row_groups_scanned,
        after_survivors.len(),
        after_total,
        after_bytes,
        after.stats.row_groups_scanned,
        after.rows,
        win_x100 / 100,
        win_x100 % 100,
    );

    // Identical answer — the layout change is transparent to the result.
    assert_eq!(
        before.rows, after.rows,
        "before/after must return the same rows (correctness preserved)",
    );
    let expected_rows = WINDOW_NS.div_ceil(GRID_NS);
    assert_eq!(
        after.rows, expected_rows,
        "the window selects k target rows"
    );

    // Before materialises the whole file: unsorted, every row group survives
    // pruning (footer), and the live query confirms it scans them all.
    assert_eq!(
        u64::try_from(before_survivors.len()).expect("fits"),
        before_total,
        "uncompacted store has no prunable row group — the whole file is the \
         window's materialization set (survivors {before_survivors:?} of {before_total})",
    );
    assert_eq!(
        before.stats.row_groups_scanned, before_total,
        "uncompacted query scans the whole file (no pruning); stats={:?}",
        before.stats,
    );
    // After prunes: the survivor set is a strict minority of the compacted
    // file's groups, and the live query confirms it skips the rest.
    assert!(
        !after_survivors.is_empty()
            && u64::try_from(after_survivors.len()).expect("fits") < after_total,
        "compacted store must prune to a subset (survivors {after_survivors:?} of {after_total})",
    );
    assert!(
        after.stats.row_groups_scanned < after_total,
        "compacted query must prune (scanned {} of {after_total} groups); stats={:?}",
        after.stats.row_groups_scanned,
        after.stats,
    );
    // The materialization win, measured: the same window's survivor set is
    // strictly fewer compressed bytes on the compacted store than on the
    // uncompacted whole-file scan. Bytes — not row-group *count* — is the
    // honest axis: compaction rotates at 32 MiB vs the ingest side's 128 MiB
    // (§3.3), so the compacted store deliberately holds more, smaller groups.
    assert!(
        after_bytes < before_bytes,
        "compaction materialises fewer bytes for the same window \
         (after {after_bytes} < before {before_bytes})",
    );
}

// ---------------------------------------------------------------------------
// RFC 0036 §7 — the compacted row-group threshold sweep (16 / 32 / 64 MiB).
//
// The `rfc0036_2_*` measurements above use a near-incompressible random
// payload (chosen so few rows cross the row-group threshold), which made
// the compacted file read as ~2× *larger* on disk (§9.27). Real logs are
// compressible and have per-service locality — sorting by `service.name`
// clusters similar lines, which should *improve* compression, not worsen
// it. This sweep uses a **compressible, service-clustered** synthetic
// corpus to trace the actual trade curve across the three candidate
// thresholds: file size, row-group count, and the L6-shape window query's
// materialization bytes + scanned groups. It is the in-repo, indicative
// half of RFC 0036 §7's first open box (the authoritative
// `baseline-8vcpu-32gib` v8 sweep stays deferred); its numbers are
// recorded in `docs/benchmarks.md` §9.28.
//
// `#[ignore]`d and heavy (it builds a multi-hundred-MiB compressible
// corpus and re-compacts it three times) — run explicitly to reproduce
// §9.28, exactly as the RFC0005.6 sizing test is `#[ignore]`d. Not a CI
// gate: the *gate* is `rfc0036_2_window_materialization_bound`, which
// already tracks whatever threshold the compacted writer resolves
// (the adaptive default, or an explicit/env override).
// ---------------------------------------------------------------------------

/// One hour in nanoseconds — the sweep spreads each service's rows across
/// the whole partition hour so a fixed 30 s window is a small slice.
const SWEEP_HOUR_NANOS: u64 = 3_600_000_000_000;

/// The candidate thresholds, in MiB.
const SWEEP_THRESHOLDS_MIB: [usize; 3] = [16, 32, 64];

/// Distinct, lexicographically-ordered promoted services. Each carries its
/// own template vocabulary ([`service_phrase`]) so the §3.1 sort clusters
/// similar lines — the locality the on-disk compression trade turns on.
const SWEEP_SERVICES: [&str; 6] = [
    "svc-auth",
    "svc-cart",
    "svc-catalog",
    "svc-payment",
    "svc-search",
    "svc-shipping",
];

/// The queried service for the window measurement — a mid-lexicographic
/// service so its clustered run sits between neighbours (the general case,
/// not a file-edge special case).
const SWEEP_TARGET: &str = "svc-payment";

/// A fixed, per-service log phrase: the compressible, clustered part of a
/// line. Distinct wording per service so sorting by `service.name` groups
/// like text together (better compression) rather than interleaving six
/// vocabularies.
fn service_phrase(service: &str) -> &'static str {
    match service {
        "svc-auth" => {
            "user authentication session established via oauth provider realm tenant scope refresh grant issued"
        }
        "svc-cart" => {
            "shopping cart line item quantity updated inventory reservation hold applied for checkout basket session"
        }
        "svc-catalog" => {
            "product catalog entry rendered category taxonomy facet ranking price tier availability warehouse listing"
        }
        "svc-payment" => {
            "payment intent captured gateway authorization settlement ledger posting currency conversion fee schedule"
        }
        "svc-search" => {
            "search query executed index shard fanout relevance scoring recall precision suggestion completion tokens"
        }
        "svc-shipping" => {
            "shipment dispatch label generated carrier route zone estimate tracking manifest customs declaration parcel"
        }
        other => panic!("no phrase for service `{other}`"),
    }
}

/// A realistic, moderately-compressible app log line: the fixed
/// per-service [`service_phrase`] followed by a handful of small varying
/// fields — including 8 hex chars of per-line entropy that hold ZSTD to a
/// realistic ratio rather than the pathological one a constant line would
/// give. ~230–260 bytes.
fn compressible_body(service: &str, id: u64) -> String {
    const STATUS: [u16; 6] = [200, 201, 204, 400, 404, 500];
    const REGION: [&str; 4] = ["us-east", "us-west", "eu-central", "ap-south"];
    let phrase = service_phrase(service);
    let status = STATUS[usize::try_from(id % 6).expect("0..6")];
    let region = REGION[usize::try_from(id % 4).expect("0..4")];
    let trace = id.wrapping_mul(2_654_435_761) & 0xffff_ffff;
    format!(
        "{phrase} request_id={id} user=u{user:06} amount={major}.{minor:02} \
         status={status} latency_ms={lat} region={region} attempt={attempt} trace={trace:08x}",
        user = id % 100_000,
        major = id % 5_000,
        minor = id % 100,
        lat = id % 950,
        attempt = id % 4,
    )
}

/// Rows per service (tunable via `OURIOS_SWEEP_ROWS_PER_SERVICE` so the
/// corpus can be resized to cross the thresholds on a given box without a
/// recompile). The default is calibrated so the compacted file clears
/// 64 MiB — several row groups even at the largest threshold.
fn sweep_rows_per_service() -> u64 {
    std::env::var("OURIOS_SWEEP_ROWS_PER_SERVICE")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(360_000)
}

/// One threshold's measured row: the compacted file's on-disk size and
/// row-group count, plus the window query's materialization term.
struct SweepRow {
    threshold_mib: usize,
    file_bytes: u64,
    row_groups: usize,
    window_survivors: usize,
    window_bytes: u64,
    scanned: u64,
    pruned: u64,
    query_rows: u64,
}

/// Build the same compressible, service-clustered hour as two interleaved
/// ingest files (so neither is service- or time-clustered on its own),
/// then compact it at `threshold`. Returns the committed file name and the
/// summed raw body bytes (the "log line" volume the compression ratio is
/// measured against).
fn seed_sweep(
    store: &Store,
    part: &PartitionKey,
    rows_per_service: u64,
    threshold: usize,
) -> (String, u64) {
    // Spread each service's rows across the whole hour so a fixed 30 s
    // window selects a small, contiguous slice of the service's clustered run.
    let grid_ns = (SWEEP_HOUR_NANOS / rows_per_service).max(1);
    let mut file_a: Vec<MinedRecord> = Vec::new();
    let mut file_b: Vec<MinedRecord> = Vec::new();
    let mut raw_body_bytes: u64 = 0;
    let mut id: u64 = 0;
    let mut to_a = true;
    for i in 0..rows_per_service {
        for &service in &SWEEP_SERVICES {
            id += 1;
            let body = compressible_body(service, id);
            raw_body_bytes += u64::try_from(body.len()).expect("body len fits u64");
            let r = rec(service, HOUR10_START + i * grid_ns, id, body);
            if to_a {
                file_a.push(r);
            } else {
                file_b.push(r);
            }
            to_a = !to_a;
        }
    }
    write_input(store, part, &file_a);
    write_input(store, part, &file_b);
    drop(file_a);
    drop(file_b);
    let committed = compact_partition_with_flush_threshold(store, part, threshold)
        .expect("compact at threshold")
        .committed
        .expect("≥2 inputs ⇒ a commit")
        .file;
    (committed, raw_body_bytes)
}

/// RFC 0036 §7 — the compacted row-group threshold sweep on a compressible,
/// service-clustered corpus. See the section banner above and
/// `docs/rfcs/0036-write-side-layout.md` §7. Prints the trade table
/// (threshold → file size, #groups, window materialization bytes, scanned
/// groups) and the corpus's measured compression ratio for `docs/benchmarks.md`
/// §9.28.
/// A fixed 30 s window from the hour start — a narrow L6-shape browse.
const SWEEP_WINDOW_NS: u64 = 30_000_000_000;

#[ignore = "heavy RFC 0036 §7 threshold sweep — run manually to (re)produce docs/benchmarks.md §9.28"]
#[tokio::test]
async fn rfc0036_7_compacted_threshold_sweep() {
    let rows_per_service = sweep_rows_per_service();
    let total_rows = rows_per_service * u64::try_from(SWEEP_SERVICES.len()).expect("fits");
    let window = HOUR10_START..HOUR10_START + SWEEP_WINDOW_NS;
    let src = format!(
        "service == \"{SWEEP_TARGET}\" | range(2026-04-02T10:00:00Z, 2026-04-02T10:00:30Z)"
    );
    let query = ourios_querier::dsl::parse(&src).expect("parse");
    let part = PartitionKey::derive(&rec(SWEEP_TARGET, HOUR10_START, 1, String::new()))
        .expect("derive partition");

    let mut rows: Vec<SweepRow> = Vec::with_capacity(SWEEP_THRESHOLDS_MIB.len());
    let mut raw_body_bytes = 0u64;
    for threshold_mib in SWEEP_THRESHOLDS_MIB {
        let threshold = threshold_mib * 1024 * 1024;
        // A fresh store per threshold — compaction is destructive (one output
        // file) and the corpus is identical, so each threshold sees the same rows.
        let bucket = tempfile::TempDir::new().expect("temp");
        let store = Store::local(bucket.path()).expect("local store");
        let (committed, raw) = seed_sweep(&store, &part, rows_per_service, threshold);
        raw_body_bytes = raw;
        let path = part.data_path(bucket.path()).join(&committed);
        let file_bytes = std::fs::metadata(&path).expect("stat compacted file").len();
        let (total, survivors, window_bytes, result) =
            measure_window(bucket.path(), &path, &query, SWEEP_TARGET, &window).await;
        rows.push(SweepRow {
            threshold_mib,
            file_bytes,
            row_groups: usize::try_from(total).expect("group count fits usize"),
            window_survivors: survivors.len(),
            window_bytes,
            scanned: result.stats.row_groups_scanned,
            pruned: result.stats.row_groups_pruned,
            query_rows: result.rows,
        });
    }

    // The trade table + the corpus compression ratio (raw log bytes ÷ the
    // 32 MiB compacted file). ×100 integer ratios avoid a float cast under
    // clippy::pedantic; read `NNN` as `N.NNx`.
    let mid = rows
        .iter()
        .find(|r| r.threshold_mib == 32)
        .expect("32 MiB row present");
    let ratio_x100 = u128::from(raw_body_bytes) * 100 / u128::from(mid.file_bytes.max(1));
    eprintln!(
        "RFC0036.7 sweep — {} services × {rows_per_service} rows = {total_rows} rows; \
         raw body {raw_body_bytes} B; compression (raw ÷ 32 MiB file) {}.{:02}x",
        SWEEP_SERVICES.len(),
        ratio_x100 / 100,
        ratio_x100 % 100,
    );
    eprintln!(
        "  threshold | file_bytes | #groups | win_survivors/#groups | win_bytes | scanned | pruned | rows"
    );
    for r in &rows {
        eprintln!(
            "  {:>3} MiB   | {:>10} | {:>7} | {:>10}/{:<7} | {:>9} | {:>7} | {:>6} | {}",
            r.threshold_mib,
            r.file_bytes,
            r.row_groups,
            r.window_survivors,
            r.row_groups,
            r.window_bytes,
            r.scanned,
            r.pruned,
            r.query_rows,
        );
    }

    // --- Sanity assertions (trade *shape*, not brittle absolute bytes) ---
    // Every threshold rotates into multiple row groups, and the same
    // narrow window returns the identical answer regardless of threshold —
    // the layout knob changes IO, never the result.
    let expected_rows = rows[0].query_rows;
    for r in &rows {
        assert!(
            r.row_groups >= 2,
            "{} MiB threshold produced only {} row group(s) — corpus too small to \
             cross it (raise OURIOS_SWEEP_ROWS_PER_SERVICE)",
            r.threshold_mib,
            r.row_groups,
        );
        assert_eq!(
            r.query_rows, expected_rows,
            "the window answer must be identical across thresholds",
        );
        assert!(
            r.scanned < total_u64(r.row_groups),
            "{} MiB: the one-service window must prune (scanned {} of {} groups)",
            r.threshold_mib,
            r.scanned,
            r.row_groups,
        );
    }
    // The finer thresholds give strictly more row groups (16 > 64), the
    // mechanism the pruning-granularity trade rests on.
    let groups_16 = rows[0].row_groups;
    let groups_64 = rows[2].row_groups;
    assert!(
        groups_16 > groups_64,
        "smaller threshold must yield more row groups (16 MiB: {groups_16}, 64 MiB: {groups_64})",
    );
}

/// `usize` row-group count as `u64` for the `scanned <` comparison.
fn total_u64(n: usize) -> u64 {
    u64::try_from(n).expect("row-group count fits u64")
}
