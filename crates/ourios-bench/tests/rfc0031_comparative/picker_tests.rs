//! Unit tests for the pickers and their fixtures.

use crate::*;

#[test]
fn pick_selective_pair_finds_the_fixture_error_row() {
    // The picker is locally provable on the shared fixture: one ERROR row,
    // text-consistent, in the comparative-fixture service.
    let records = comparative_fixture(1_000_000);
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let pair = pick_selective_pair(corpus.path());
    assert_eq!(
        pair.service, FIXTURE_SERVICE,
        "the (1, 17) tie against FIXTURE_SERVICE_B's ERROR row breaks to \
         the lexicographically smaller service"
    );
    assert_eq!(
        pair.threshold, 17,
        "the ERROR band is the rarest single-text band"
    );
    assert_eq!(pair.text, "ERROR");
    assert_eq!(pair.rows, 1, "exactly the one FIXTURE_SERVICE ERROR record");
    assert_eq!(pair.total_records, 4);
    assert_eq!(pair.min_ts, 1_000_000);
    assert_eq!(pair.max_ts, 1_003_000, "the service-B record is the latest");
}

#[test]
fn pick_selective_pair_generalizes_without_error_rows() {
    // The otel-demo-v8 shape in miniature: an INFO-dominated service with
    // a rare WARN band and NO ERROR rows anywhere — the generalization
    // path run #1 surfaced. The picker must select the WARN band.
    let records = vec![
        FixtureRecord {
            time_unix_nano: 1_000,
            severity_number: 9,
            severity_text: "INFO",
            body: "user 1 logged in",
            trace_id: None,
            service: FIXTURE_SERVICE,
        },
        FixtureRecord {
            time_unix_nano: 2_000,
            severity_number: 9,
            severity_text: "INFO",
            body: "user 2 logged in",
            trace_id: None,
            service: FIXTURE_SERVICE,
        },
        FixtureRecord {
            time_unix_nano: 3_000,
            severity_number: 13,
            severity_text: "WARN",
            body: "cache nearly full",
            trace_id: None,
            service: FIXTURE_SERVICE,
        },
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let pair = pick_selective_pair(corpus.path());
    assert_eq!(pair.service, FIXTURE_SERVICE);
    assert_eq!(
        pair.threshold, 13,
        "the WARN band is the rarest single-text band"
    );
    assert_eq!(pair.text, "WARN");
    assert_eq!(pair.rows, 1);
}

#[test]
fn pick_window_pair_prefers_a_clean_mid_corpus_window() {
    let ts: Vec<u64> = (0..100).map(|i| 1_000 + i * 10).collect();
    let (a, b) = pick_window_pair(&ts, &[], 10).expect("every window is clean");
    assert_eq!(a, 1_000 + 45 * 10, "centred start index (100−10)/2 = 45");
    assert_eq!(b, 1_000 + 54 * 10 + 1, "b = last in-window timestamp + 1");
    assert_eq!(ts.iter().filter(|&&t| t >= a && t < b).count(), 10);
}

#[test]
fn pick_window_pair_edges_are_clean() {
    // Duplicates and 1 ns gaps around the centre force the picker off the
    // centred window; whatever it returns must satisfy both edge
    // invariants and contain exactly k rows.
    let ts: Vec<u64> = vec![0, 10, 20, 30, 40, 40, 41, 50, 60, 70, 80, 90];
    let k = 3;
    let (a, b) = pick_window_pair(&ts, &[], k).expect("clean windows exist");
    assert_eq!(ts.iter().filter(|&&t| t >= a && t < b).count(), k);
    assert!(
        !ts.contains(&b),
        "end-inclusive semantics at b must not admit an extra row"
    );
    let first_inside = ts.iter().position(|&t| t >= a).expect("window non-empty");
    assert!(
        first_inside == 0 || ts[first_inside - 1] < a,
        "no earlier record shares the window's start timestamp"
    );
}

#[test]
fn pick_window_pair_avoids_poison_timestamps() {
    let ts: Vec<u64> = (0..100).map(|i| i * 10).collect();
    let poison = vec![455];
    let (a, b) = pick_window_pair(&ts, &poison, 10).expect("clean windows exist off-centre");
    assert!(
        !(a..b).contains(&455),
        "a zero-time row's fallback timestamp inside the window guarantees \
         an equivalence mismatch"
    );
    assert_eq!(ts.iter().filter(|&&t| t >= a && t < b).count(), 10);
}

#[test]
fn pick_window_pair_none_when_insufficient() {
    assert_eq!(pick_window_pair(&[1, 2, 3], &[], 4), None);
    assert_eq!(pick_window_pair(&[1, 2, 3], &[], 0), None);
}

#[test]
fn collect_service_timestamps_reads_the_fixture() {
    let records = comparative_fixture(1_000_000);
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let (clean, poison) = collect_service_timestamps(corpus.path(), FIXTURE_SERVICE);
    assert_eq!(clean, vec![1_000_000, 1_001_000, 1_002_000]);
    assert!(poison.is_empty(), "the fixture has no zero-time records");
    let (other_clean, other_poison) = collect_service_timestamps(corpus.path(), "no-such-service");
    assert!(other_clean.is_empty() && other_poison.is_empty());
}

/// Locally proves the window-pair query shape end to end on the fixture:
/// the bare-service DSL parses, and the `now = end` / `window = end −
/// start` mapping slices exactly `[start, end)` (the querier's
/// `time_window_filter` is `ts ≥ now − window ∧ ts < now`).
#[test]
fn window_pair_dsl_slices_the_fixture() {
    let records = comparative_fixture(1_000_000);
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);

    // [1_001_000, 1_002_001) → the second and third fixture records.
    let (start, end) = (1_001_000u64, 1_002_001u64);
    let dsl = format!("service == \"{FIXTURE_SERVICE}\" | limit 5000");
    let answer = ourios_bench::ourios_query_answer(bucket.path(), &tenant, &dsl, end, end - start)
        .expect("ourios answer");
    let mut got: Vec<u64> = answer
        .lines
        .iter()
        .map(|k| k.timestamp_unix_nanos)
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![1_001_000, 1_002_000]);
}

// ---------------------------------------------------------------------------
// Promoted `service.name` parity (RFC 0022 §3.1 in the comparative store).
//
// Loki's stock OTLP ingest promotes `service.name` to the `service_name`
// stream label by default, so every service-scoped LogQL query in the
// comparison is label-indexed. Ourios holds the same ground by
// construction: RFC 0022 §3.1 promotes `service.name` implicitly on every
// writer constructor, so the comparative store's files carry a
// stats-bearing `resource.service.name` column and the DSL `service ==`
// predicate compiles to the promoted arm (RFC 0022 §3.3), not the
// JSON-substring fallback. These tests pin that parity: a writer or
// store-builder change that demoted the column would silently hold Ourios
// to a harder version of the service-scoped question than Loki answers,
// exactly the strawman RFC 0031 §2 forbids in either direction.
// ---------------------------------------------------------------------------

/// Every data file the comparative store publishes carries the RFC 0022
/// promoted `resource.service.name` column, byte-for-byte the field the
/// writer's default (implicit-`service.name`) schema declares.
#[test]
fn comparative_store_promotes_the_service_name_column() {
    let (bucket, built) = build_two_service_store();
    assert_eq!(built.rows, 6);
    assert_eq!(built.files, 2, "one file per hour partition");

    let expected_schema =
        ourios_parquet::data_schema_with_promoted(&ourios_parquet::PromotedAttributes::default());
    let expected_field = expected_schema
        .field_with_name("resource.service.name")
        .expect("default promoted schema declares the service column");

    let mut checked = 0;
    let mut stack = vec![bucket.path().join("data")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "parquet") {
                let file = std::fs::File::open(&path).expect("open parquet file");
                let builder =
                    parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
                        .expect("parquet footer");
                let field = builder
                    .schema()
                    .field_with_name("resource.service.name")
                    .unwrap_or_else(|_| {
                        panic!("{} lacks the promoted service column", path.display())
                    });
                assert_eq!(field, expected_field, "{}", path.display());
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 2, "both data files checked");
}

/// A `service ==` query over the two-service store answers off the
/// promoted column: the other service's row group is pruned by its
/// statistics, and the query reads strictly fewer bytes than the
/// full-window scan. The JSON-substring fallback arm can do neither —
/// this is the pruning evidence RFC 0031's service-scoped pairs rest on.
#[test]
fn service_predicate_prunes_on_the_promoted_column() {
    let (bucket, built) = build_two_service_store();
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

    let query = ourios_querier::dsl::parse("service == \"svc-a\" | limit 100").expect("parse DSL");
    let querier = ourios_querier::Querier::new(bucket.path());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let result = runtime
        .block_on(querier.run_query(&query, &tenant, now, window, None))
        .expect("service query");
    assert_eq!(result.rows, 3, "exactly the svc-a rows match");
    assert!(
        result.stats.row_groups_pruned >= 1,
        "svc-b's row group must be pruned via promoted-column statistics \
         (scanned {}, pruned {})",
        result.stats.row_groups_scanned,
        result.stats.row_groups_pruned,
    );

    // The same two queries through the comparative measurement channel:
    // the service-scoped answer is the right rows AND fewer bytes than
    // the full-window scan.
    let service_answer = ourios_bench::ourios_query_answer(
        bucket.path(),
        &tenant,
        "service == \"svc-a\" | limit 100",
        now,
        window,
    )
    .expect("service answer");
    let expected: Vec<LineKey> = two_service_records("svc-a")
        .into_iter()
        .map(|(ts, body)| LineKey {
            timestamp_unix_nanos: ts,
            body: body.as_bytes().to_vec(),
        })
        .collect();
    assert!(
        compare_lines(&service_answer.lines, &expected, 8).is_equal(),
        "the promoted arm must return exactly the svc-a lines",
    );

    let full_answer = ourios_bench::ourios_query_answer(
        bucket.path(),
        &tenant,
        "severity >= 0 | limit 100",
        now,
        window,
    )
    .expect("full-window answer");
    assert_eq!(
        full_answer.lines.len(),
        6,
        "the full scan reads both services"
    );
    assert!(
        service_answer.bytes_read < full_answer.bytes_read,
        "service-scoped bytes_read ({}) must undercut the full scan ({})",
        service_answer.bytes_read,
        full_answer.bytes_read,
    );
}

#[test]
fn select_pair_candidates_rejects_poisoned_bands() {
    use std::collections::HashMap;

    let mut bands: SeverityBands = HashMap::new();
    bands.entry(9).or_default().insert("INFO".into(), 5);
    bands.entry(13).or_default().insert("WARN".into(), 1);

    let clean: HashMap<String, (SeverityBands, SeverityBands)> =
        [("svc".to_string(), (bands.clone(), HashMap::new()))].into();
    assert!(
        select_pair_candidates(&clean)
            .iter()
            .any(|&(rows, threshold, _, text)| rows == 1 && threshold == 13 && text == "WARN"),
        "without poison the WARN band is a candidate"
    );

    // A zero-time row the DSL side would select (number ≥ threshold)…
    let mut poison_high: SeverityBands = HashMap::new();
    poison_high.entry(17).or_default().insert("ERROR".into(), 1);
    let poisoned: HashMap<String, (SeverityBands, SeverityBands)> =
        [("svc".to_string(), (bands.clone(), poison_high))].into();
    assert!(
        select_pair_candidates(&poisoned).is_empty(),
        "a poison row at number ≥ threshold disqualifies the candidate"
    );

    // …and one the LogQL side would select (same text, below threshold).
    let mut poison_text: SeverityBands = HashMap::new();
    poison_text.entry(5).or_default().insert("WARN".into(), 1);
    let poisoned_text: HashMap<String, (SeverityBands, SeverityBands)> =
        [("svc".to_string(), (bands, poison_text))].into();
    assert!(
        select_pair_candidates(&poisoned_text).is_empty(),
        "a below-threshold poison row carrying the text disqualifies the candidate"
    );
}

#[test]
fn pick_trace_pair_finds_the_shared_fixture_trace() {
    // Three fixture records share FIXTURE_TRACE across two services
    // (eligible: 3 rows, clean timestamps, named services); the remaining
    // record's trace has only one row — below the 2-row floor — so
    // exactly one candidate remains.
    let records = comparative_fixture(1_000_000);
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let (hex, rows) = pick_trace_pair(corpus.path()).expect("the shared trace is eligible");
    assert_eq!(hex, FIXTURE_TRACE);
    assert_eq!(rows, 3, "two FIXTURE_SERVICE rows + the service-B row");
}

#[test]
fn pick_trace_pair_rejects_zero_ts_traces() {
    // A trace containing any zero-time_unix_nano row is poison (the two
    // systems answer with different keys); with every trace poisoned or
    // single-row, there is no candidate.
    let records = vec![
        FixtureRecord {
            time_unix_nano: 0,
            severity_number: 9,
            severity_text: "INFO",
            body: "zero ts row",
            trace_id: Some("00112233445566778899aabbccddeeff"),
            service: FIXTURE_SERVICE,
        },
        FixtureRecord {
            time_unix_nano: 1_000,
            severity_number: 9,
            severity_text: "INFO",
            body: "clean row same trace",
            trace_id: Some("00112233445566778899aabbccddeeff"),
            service: FIXTURE_SERVICE,
        },
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    assert_eq!(pick_trace_pair(corpus.path()), None);
}

#[test]
fn pick_trace_pair_prefers_multi_service_then_smallest_id() {
    let rec = |ts, body, trace, service| FixtureRecord {
        time_unix_nano: ts,
        severity_number: 9,
        severity_text: "INFO",
        body,
        trace_id: Some(trace),
        service,
    };

    // A 2-row multi-service trace beats a 3-row single-service one.
    let records = vec![
        rec(
            1_000,
            "big a",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
        rec(
            2_000,
            "big b",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
        rec(
            3_000,
            "big c",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
        rec(
            4_000,
            "spans a",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            FIXTURE_SERVICE,
        ),
        rec(
            5_000,
            "spans b",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            FIXTURE_SERVICE_B,
        ),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");
    assert_eq!(
        pick_trace_pair(corpus.path()),
        Some(("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(), 2)),
        "multi-service wins over more rows"
    );

    // Otherwise-equal candidates tie-break to the lexicographically
    // smallest id.
    let records = vec![
        rec(
            1_000,
            "tie c1",
            "cccccccccccccccccccccccccccccccc",
            FIXTURE_SERVICE,
        ),
        rec(
            2_000,
            "tie c2",
            "cccccccccccccccccccccccccccccccc",
            FIXTURE_SERVICE,
        ),
        rec(
            3_000,
            "tie a1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
        rec(
            4_000,
            "tie a2",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            FIXTURE_SERVICE,
        ),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");
    assert_eq!(
        pick_trace_pair(corpus.path()),
        Some(("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), 2)),
        "equal candidates pick the smallest id"
    );
}

#[test]
fn template_needle_takes_the_longest_safe_constant_run() {
    let fixed = |s: &str| OwnedToken::Fixed(s.to_owned());

    // Wildcards and unsafe tokens split runs; the longest surviving run
    // wins, joined with single spaces.
    let tokens = vec![
        fixed("shutting"),
        fixed("down"),
        OwnedToken::Wildcard,
        fixed("connection"),
        fixed("established"),
        fixed("to"),
        fixed("peer"),
        fixed("via:"), // unsafe ':' splits the run
        fixed("gateway"),
    ];
    assert_eq!(
        template_needle(&tokens).as_deref(),
        Some("connection established to peer"),
    );

    // Under 10 chars → no candidate; all wildcards → no candidate.
    assert_eq!(template_needle(&[fixed("logged"), fixed("in")]), None);
    assert_eq!(
        template_needle(&[OwnedToken::Wildcard, OwnedToken::Wildcard]),
        None,
    );

    // A length tie breaks to the lexicographically smallest run.
    let tie = vec![
        fixed("bbbbb"),
        fixed("bbbb"),
        OwnedToken::Wildcard,
        fixed("aaaaa"),
        fixed("aaaa"),
    ];
    assert_eq!(template_needle(&tie).as_deref(), Some("aaaaa aaaa"));
}

#[test]
fn eligible_template_candidates_enforce_poison_rules_and_bounds() {
    let candidates: Vec<(u64, String)> = [
        (1, "needle alpha"),         // clean, 3 matches
        (2, "needle beta"),          // zero-ts poison
        (3, "needle gamma"),         // empty-service poison
        (4, "needle delta"),         // 1 match — under the 2-row floor
        (5, "needle epsilon"),       // 4001 — over the one-page cap
        (6, "short need"),           // clean, 2 matches — most selective
        (7, "a longer needle here"), // clean, 3 matches, longer needle
    ]
    .into_iter()
    .map(|(id, needle)| (id, needle.to_string()))
    .collect();
    let tally = |matches, has_zero_ts, has_empty_service| NeedleTally {
        matches,
        has_zero_ts,
        has_empty_service,
    };
    let tallies = vec![
        tally(3, false, false),
        tally(3, true, false),
        tally(3, false, true),
        tally(1, false, false),
        tally(4001, false, false),
        tally(2, false, false),
        tally(3, false, false),
    ];

    assert_eq!(
        eligible_template_candidates(&candidates, &tallies),
        vec![
            (6, "short need", 2),           // fewest matches first
            (7, "a longer needle here", 3), // then the longer needle
            (1, "needle alpha", 3),
        ],
        "poisoned and out-of-bounds candidates drop; the rest sort \
         most-selective first with deterministic tiebreaks",
    );
}

#[test]
fn pick_template_pair_finds_a_validated_needle() {
    // Three rows of one template whose constant run is ≥ 10 safe chars
    // ("connection established to peer" — the numbers mask to one
    // wildcard slot) plus a second template whose longest run ("logged
    // in", 9 chars) is under the floor: exactly one candidate, and it
    // validates — 3 needle-matching corpus lines == 3 template rows,
    // every rendered row contains the needle.
    let records: Vec<(u64, &str)> = vec![
        (1_000, "connection established to peer 10"),
        (2_000, "connection established to peer 11"),
        (3_000, "connection established to peer 12"),
        (4_000, "user 4 logged in"),
        (5_000, "user 5 logged in"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

    let pair = pick_template_pair(corpus.path(), bucket.path(), &tenant, now, window)
        .expect("the peer template validates");
    assert_eq!(pair.needle, "connection established to peer");
    assert_eq!(pair.rows, 3, "exactly the three peer rows");
    assert_ne!(pair.template_id, 0, "never the NO_TEMPLATE sentinel");
}

#[test]
fn pick_template_pair_rejects_substring_collisions() {
    // Two rows of the peer template, plus ONE line of a DIFFERENT
    // template (different token count) that contains the same constant
    // text as a substring: the needle matches 3 corpus lines against 2
    // template rows, so count-equality fails — a Loki line filter for it
    // would return a row the DSL side never selects. The colliding
    // template's own needle is the same string (3 matches vs 1 row), so
    // no candidate validates and the picker returns None.
    let records: Vec<(u64, &str)> = vec![
        (1_000, "connection established to peer 10"),
        (2_000, "connection established to peer 11"),
        (3_000, "retry: connection established to peer 12 via proxy"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

    assert!(
        pick_template_pair(corpus.path(), bucket.path(), &tenant, now, window).is_none(),
        "a needle whose corpus count exceeds the template's rows must be rejected",
    );
}

#[test]
fn pick_bucket_width_targets_four_buckets_floored_at_one_second() {
    assert_eq!(pick_bucket_width(4_000_000_000), "1s", "4s span / 4 = 1s");
    assert_eq!(
        pick_bucket_width(400_000_000),
        "1s",
        "a sub-second span floors to the DSL's finest unit",
    );
    assert_eq!(
        pick_bucket_width(4 * 60_000_000_000),
        "1m",
        "4m span / 4 = 1m"
    );
    assert_eq!(
        pick_bucket_width(4 * 3_600_000_000_000),
        "1h",
        "4h span / 4 = 1h"
    );
    assert_eq!(
        pick_bucket_width(4 * 86_400_000_000_000),
        "1d",
        "4d span / 4 = 1d"
    );
    assert_eq!(
        pick_bucket_width(4 * 7 * 86_400_000_000_000),
        "1w",
        "4w span / 4 = 1w",
    );
    assert_eq!(
        pick_bucket_width(9 * 60_000_000_000),
        "2m",
        "9m / 4 = 2.25m, floored to 2m",
    );
}

#[test]
fn bucket_width_seconds_converts_every_dsl_unit() {
    assert_eq!(bucket_width_seconds("30s"), 30);
    assert_eq!(bucket_width_seconds("2m"), 120);
    assert_eq!(bucket_width_seconds("1h"), 3_600);
    assert_eq!(bucket_width_seconds("1d"), 86_400);
    assert_eq!(bucket_width_seconds("1w"), 7 * 86_400);
}

#[test]
fn regex_escape_escapes_go_re2_metacharacters() {
    assert_eq!(regex_escape("peer"), "peer");
    assert_eq!(regex_escape("a.b*c"), r"a\.b\*c");
    assert_eq!(regex_escape(r"[x](y)"), r"\[x\]\(y\)");
}

#[test]
fn param_capture_regex_anchors_on_the_nearest_fixed_neighbours() {
    let fixed = |s: &str| OwnedToken::Fixed(s.to_owned());

    // A single trailing wildcard: only a prefix neighbour exists.
    let tokens = vec![
        fixed("connection"),
        fixed("established"),
        fixed("to"),
        fixed("peer"),
        OwnedToken::Wildcard,
    ];
    assert_eq!(
        param_capture_regex(&tokens, 0).as_deref(),
        Some(r"peer\s+(?P<value>\S+)"),
    );

    // A wildcard with fixed neighbours on both sides.
    let tokens = vec![
        fixed("user"),
        OwnedToken::Wildcard,
        fixed("logged"),
        fixed("in"),
    ];
    assert_eq!(
        param_capture_regex(&tokens, 0).as_deref(),
        Some(r"user\s+(?P<value>\S+)\s+logged"),
    );

    // The SECOND wildcard, disambiguated from the first by index.
    let tokens = vec![
        fixed("from"),
        OwnedToken::Wildcard,
        fixed("to"),
        OwnedToken::Wildcard,
        fixed("bytes"),
    ];
    assert_eq!(
        param_capture_regex(&tokens, 1).as_deref(),
        Some(r"to\s+(?P<value>\S+)\s+bytes"),
    );

    // A metacharacter neighbour is escaped.
    let tokens = vec![fixed("count."), OwnedToken::Wildcard];
    assert_eq!(
        param_capture_regex(&tokens, 0).as_deref(),
        Some(r"count\.\s+(?P<value>\S+)"),
    );

    // Out of range: no such wildcard position.
    assert_eq!(
        param_capture_regex(&[fixed("no"), fixed("wildcards")], 0),
        None,
    );
    assert_eq!(param_capture_regex(&[OwnedToken::Wildcard], 1), None);

    // Unanchored: neither neighbour is fixed — a single wildcard with no
    // surrounding tokens, and two adjacent wildcards where the target's
    // neighbour on both sides is itself a wildcard. An unanchored
    // `(?P<value>\S+)` would match any token, so both must reject rather
    // than return a pattern a caller could unknowingly measure with.
    assert_eq!(param_capture_regex(&[OwnedToken::Wildcard], 0), None);
    assert_eq!(
        param_capture_regex(&[OwnedToken::Wildcard, OwnedToken::Wildcard], 0),
        None,
    );
    assert_eq!(
        param_capture_regex(&[OwnedToken::Wildcard, OwnedToken::Wildcard], 1),
        None,
    );
}

#[test]
fn pick_frequency_pair_finds_a_moderate_cardinality_group() {
    // Two ids ("10"/"11") of the peer template, spread over five
    // 12-minute buckets — cardinality 2 (within 2..=50), 6 total rows
    // (above the L4_MIN_ROWS floor), and 1000x the original sub-3s
    // timestamps (~580s average spacing over the 100s-3000s span) so
    // this candidate also clears L4_MIN_AVG_INTERVAL_SECONDS: a tight
    // synthetic timeline is convenient for a unit test but is exactly
    // the bursty shape a short average inter-arrival interval rejects
    // — a lower-frequency candidate is what the RFC 0031 L4 workstream
    // (runs #6-#12) found measures more completely against Loki, though
    // the exact mechanism is uncharacterized (NOT ingest-side dedup,
    // ruled out directly — see `L4_COMPLETENESS_MARGIN`'s documentation).
    let records: Vec<(u64, &str)> = vec![
        (100_000_000_000, "connection established to peer 10"),
        (500_000_000_000, "connection established to peer 10"),
        (900_000_000_000, "connection established to peer 11"),
        (2_100_000_000_000, "connection established to peer 10"),
        (2_500_000_000_000, "connection established to peer 11"),
        (3_000_000_000_000, "connection established to peer 11"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

    let pair = pick_frequency_pair(bucket.path(), &tenant, now, window)
        .expect("the peer template's trailing id validates");
    assert_eq!(pair.param, 0);
    assert_eq!(pair.bucket_width, "12m");
    assert_eq!(pair.needle, "connection established to peer");
    assert_eq!(pair.capture_regex, r"peer\s+(?P<value>\S+)");
    assert_eq!(pair.groups.values().sum::<u64>(), 6);
    assert!(pair.bytes_read > 0);
}

#[test]
fn pick_frequency_pair_rejects_below_the_row_floor() {
    // Three rows, each a DISTINCT trailing id (cardinality 3, inside the
    // 2..=50 band) but under L4_MIN_ROWS (4): the same generalised bound
    // that rejects kafka's per-line-unique slots at scale — small enough
    // here to pin the row floor explicitly. No other template exists to
    // fall through to, so the picker returns None loudly rather than
    // silently degrading to a weaker candidate.
    let records: Vec<(u64, &str)> = vec![
        (1_000, "connection established to peer 10"),
        (2_000, "connection established to peer 11"),
        (3_000, "connection established to peer 12"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

    assert!(
        pick_frequency_pair(bucket.path(), &tenant, now, window).is_none(),
        "3 rows is below L4_MIN_ROWS — the picker must reject loudly, not fall through",
    );
}

// A wide bucket width so `frequency_shape_rejection_enforces_the_row_ceiling`'s
// ~50K-rows-per-bucket density clears [`L4_MIN_AVG_INTERVAL_SECONDS`]
// too — that fixture's two-bucket, 1-second-apart `AggKey`s are
// synthetic (isolating the row ceiling from the other floors), not a
// real bucket width, so this value is chosen only to keep the
// frequency floor from interfering.
pub(crate) const WIDE_BUCKET_SECONDS: u64 = 10_000_000;

#[test]
fn frequency_shape_rejection_enforces_the_row_ceiling() {
    // A ~971K-row candidate (a service's dominant, near-catch-all
    // template) is exactly the run #3 dispatch finding: cardinality 2
    // and 2 bucket windows both clear their floors, but the total row
    // count made the Loki poll unable to finish within its shared
    // 300s deadline. Two groups summing past L4_MAX_ROWS reproduces
    // the shape without building a 100K+-row real corpus fixture.
    let groups = HashMap::from([
        (
            AggKey {
                bucket_start_unix_nanos: 0,
                group_key: "a".to_string(),
            },
            L4_MAX_ROWS / 2 + 1,
        ),
        (
            AggKey {
                bucket_start_unix_nanos: 1_000_000_000,
                group_key: "b".to_string(),
            },
            L4_MAX_ROWS / 2 + 1,
        ),
    ]);
    let rejection = frequency_shape_rejection(&groups, WIDE_BUCKET_SECONDS)
        .expect("a candidate past L4_MAX_ROWS must be rejected");
    assert!(
        rejection.contains(&L4_MAX_ROWS.to_string()),
        "the rejection reason must name the ceiling: {rejection}",
    );

    // Exactly at the ceiling passes (isolating the ceiling from the
    // other floors: still cardinality 2, still 2 bucket windows).
    let mut under_ceiling = groups;
    let cell = under_ceiling
        .get_mut(&AggKey {
            bucket_start_unix_nanos: 0,
            group_key: "a".to_string(),
        })
        .expect("cell exists");
    *cell -= 2;
    assert_eq!(
        frequency_shape_rejection(&under_ceiling, WIDE_BUCKET_SECONDS),
        None,
        "exactly at the ceiling must pass",
    );
}

#[test]
fn frequency_shape_rejection_enforces_the_average_interval_floor() {
    // RFC 0031 L4 workstream, runs #6-#12: kafka's `template_id=16`
    // needle averaged a ~15s inter-arrival cadence and measured a
    // stable ~93-97% completeness ceiling against Loki, independent of
    // every query-side fix tried — a plain unaggregated line count came
    // back just as short, ruling out anything specific to the metric-
    // aggregation path (NOT proof of ingest-side loss specifically: the
    // plain count is still a query_range call, so it can't distinguish
    // "never stored" from "this wide-time-range query came back
    // incomplete" — see L4_COMPLETENESS_MARGIN's documentation, which
    // settles on the latter). A later corpus-side check ruled out exact
    // (timestamp, body) collision entirely; the mechanism correlates
    // with cadence but is otherwise uncharacterized. 100 rows over a 1h
    // bucket, 2 distinct values, 2 buckets: cardinality and bucket-
    // diversity both clear, but ~72s average spacing (below
    // L4_MIN_AVG_INTERVAL_SECONDS) must still reject.
    let dense = HashMap::from([
        (
            AggKey {
                bucket_start_unix_nanos: 0,
                group_key: "a".to_string(),
            },
            50,
        ),
        (
            AggKey {
                bucket_start_unix_nanos: 3_600_000_000_000,
                group_key: "b".to_string(),
            },
            50,
        ),
    ]);
    let rejection =
        frequency_shape_rejection(&dense, 3_600).expect("a ~72s average cadence must be rejected");
    assert!(
        rejection.contains("average inter-arrival interval"),
        "the rejection reason must name the interval floor: {rejection}",
    );

    // Same shape, a fifth of the volume: ~360s average spacing clears
    // the floor, isolating it from cardinality/ceiling/bucket-diversity.
    let sparse = HashMap::from([
        (
            AggKey {
                bucket_start_unix_nanos: 0,
                group_key: "a".to_string(),
            },
            10,
        ),
        (
            AggKey {
                bucket_start_unix_nanos: 3_600_000_000_000,
                group_key: "b".to_string(),
            },
            10,
        ),
    ]);
    assert_eq!(
        frequency_shape_rejection(&sparse, 3_600),
        None,
        "a ~360s average cadence must pass",
    );
}

#[test]
fn pick_frequency_pair_rejects_a_single_bucket_window() {
    // Cardinality 2 and 4 rows both clear their respective floors, but
    // every timestamp falls inside the same 1s bucket window — the
    // candidate never exercises the bucket dimension of the (bucket,
    // group_key) → count equivalence shape, so the picker must reject
    // it rather than measure an L4 pair that leaves bucket alignment
    // untested.
    let records: Vec<(u64, &str)> = vec![
        (100_000_000, "connection established to peer 10"),
        (300_000_000, "connection established to peer 10"),
        (500_000_000, "connection established to peer 11"),
        (700_000_000, "connection established to peer 11"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

    assert!(
        pick_frequency_pair(bucket.path(), &tenant, now, window).is_none(),
        "all rows land in one bucket window — the picker must reject loudly, not fall through",
    );
}

#[test]
fn pick_frequency_pair_rejects_a_single_value_slot() {
    // Every row carries the SAME trailing id — cardinality 1, below the
    // L4 grouping floor of 2 (a single value is not a grouping question):
    // enough rows to clear L4_MIN_ROWS, isolating the cardinality bound
    // from the row-floor rejection the previous test pins.
    let records: Vec<(u64, &str)> = vec![
        (100_000_000, "connection established to peer 10"),
        (500_000_000, "connection established to peer 10"),
        (900_000_000, "connection established to peer 10"),
        (2_100_000_000, "connection established to peer 10"),
    ];
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        serde_json::to_string(&service_logs_data(FIXTURE_SERVICE, &records))
            .expect("serialize LogsData"),
    )
    .expect("write corpus");
    let bucket = tempfile::TempDir::new().expect("bucket dir");
    let built = ourios_bench::build_comparative_store(
        corpus.path(),
        bucket.path(),
        ourios_bench::TxtSeverity::Fixed,
    )
    .expect("build comparative store");
    let tenant = TenantId::new(built.tenant);
    let now = built.max_effective_time_unix_nano + 1;
    let window = built.max_effective_time_unix_nano - built.min_effective_time_unix_nano + 2;

    assert!(
        pick_frequency_pair(bucket.path(), &tenant, now, window).is_none(),
        "cardinality 1 is below the L4 grouping floor — reject, don't fall through",
    );
}

#[test]
fn pick_rare_window_pair_selects_the_low_volume_service() {
    // Service B is rarer than FIXTURE_SERVICE (1 row vs 3) but a 1-row
    // window fails the 2-row floor, so the picker must fall through to
    // the next-rarest service with a clean window.
    let records = comparative_fixture(1_000_000);
    let corpus = tempfile::TempDir::new().expect("corpus dir");
    std::fs::write(
        corpus.path().join("fixture.jsonl"),
        fixture_jsonl(&records).expect("fixture jsonl"),
    )
    .expect("write corpus");

    let (service, start, end, rows) =
        pick_rare_window_pair(corpus.path(), "not-in-corpus").expect("a clean window exists");
    assert_eq!(
        service, FIXTURE_SERVICE,
        "the 1-row service B fails the 2-row floor; fall through"
    );

    // Excluding the only viable service yields a loud None, never a
    // duplicate of an existing pair (the run #16 lesson).
    assert_eq!(pick_rare_window_pair(corpus.path(), FIXTURE_SERVICE), None);
    assert_eq!(rows, 3, "all three FIXTURE_SERVICE rows fit one window");
    assert!(start < end);
    assert!(start >= 1_000_000 && end <= 1_002_001);
}
