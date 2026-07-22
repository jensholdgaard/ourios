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

/// The same one-hour corpus [`seed_and_compact`] builds, but as a
/// single flat Vec in ingest (round-robin) order — the shape a
/// partition has *before* compaction: one unsorted ingest-side file, no
/// `sorting_columns`, no per-service/-time clustering. Every `(service,
/// time)` key matches the compacted store's, so the same window query
/// returns the identical answer on both.
fn all_records(plan: &[(&str, u64)], body_len: usize, grid_ns: u64) -> Vec<MinedRecord> {
    let max_rows = plan.iter().map(|&(_, rows)| rows).max().unwrap_or(0);
    let mut records: Vec<MinedRecord> = Vec::new();
    let mut id: u64 = 0;
    for i in 0..max_rows {
        for &(service, rows) in plan {
            if i < rows {
                id += 1;
                records.push(rec(
                    service,
                    HOUR10_START + i * grid_ns,
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
///   ([`seed_and_compact`], 32 MiB [`COMPACTED_ROW_GROUP_FLUSH_BYTES`]
///   row groups). The §3.1 clustering gives each row group tight
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
/// 128 MiB and compaction at 32 MiB (RFC 0036 §3.3), so the compacted store
/// deliberately holds more, smaller groups (and, at these sub-threshold
/// sizes, a physically larger file — the smaller groups compress a little
/// worse). The window still materialises fewer bytes on the compacted store
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

    let src =
        format!("service == \"{TARGET}\" | range(2026-04-02T10:00:00Z, 2026-04-02T10:00:01Z)");
    let query = ourios_querier::dsl::parse(&src).expect("parse");
    let part = PartitionKey::derive(&rec(TARGET, HOUR10_START, 1, String::new()))
        .expect("derive partition");

    let window = HOUR10_START..HOUR10_START + WINDOW_NS;

    // --- Before: one unsorted ingest-side file, no manifest (glob path). The
    // materialization term (RFC 0036 §9) is the compressed column chunks of
    // the row groups a `service == TARGET AND time ∈ window` scan can NOT
    // prune; on the unsorted ingest file every row group spans the whole hour
    // and every service, so none prune — the survivors are the *whole file*. ---
    let before_bucket = tempfile::TempDir::new().expect("temp");
    let before_store = Store::local(before_bucket.path()).expect("local store");
    write_input(&before_store, &part, &all_records(&plan, BODY_LEN, GRID_NS));
    let before_dir = part.data_path(before_bucket.path());
    let before_path = std::fs::read_dir(&before_dir)
        .expect("read partition dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "parquet"))
        .expect("one committed ingest file");
    let (before_total, before_survivors, before_bytes, before) =
        measure_window(before_bucket.path(), &before_path, &query, TARGET, &window).await;

    // --- After: the compacted, sorted store (built exactly as the bound test).
    // Same survivor computation: the §3.1 clustering gives each row group tight
    // `service.name`/`time` statistics, so the survivors are a contiguous
    // minority of the file's groups. ---
    let after_bucket = tempfile::TempDir::new().expect("temp");
    let after_store = Store::local(after_bucket.path()).expect("local store");
    let committed = seed_and_compact(&after_store, &part, &plan, BODY_LEN, GRID_NS);
    let after_path = part.data_path(after_bucket.path()).join(&committed);
    let (after_total, after_survivors, after_bytes, after) =
        measure_window(after_bucket.path(), &after_path, &query, TARGET, &window).await;

    // Integer ×100 ratio (avoids a float cast under clippy::pedantic); read
    // as `N.NNx`, e.g. 198 ⇒ before materialises 1.98× the after bytes.
    let win_x100 = before_bytes * 100 / after_bytes.max(1);
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
