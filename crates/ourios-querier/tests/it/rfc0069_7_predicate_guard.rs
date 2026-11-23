//! RFC0069.7: a filter over an undecodable attribute cell does not fail the
//! query, and neither the comparison nor its negation matches the bad row,
//! whatever the cell's text holds. A comparison on a promoted key answers
//! from its typed column when that column is set.

use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_parquet::{PromotedAttributes, PromotedClass, PromotedKey, columns};
use ourios_querier::{Querier, QueryResult, UndecodableCells};

use crate::rfc0069_fixture as fixture;

use fixture::{Events, TS0, kv, record};

use crate::common::{DEFAULT_WINDOW_NS, NOW, no_aliases};

/// An attributes cell that does not decode (its list is never closed) but
/// whose text holds every pair the queries below look for.
const BAD: &str = concat!(
    r#"[{"key":"k","value":{"stringValue":"x"}},"#,
    r#"{"key":"p","value":{"stringValue":"x"}},"#,
    r#"{"key":"i","value":{"intValue":"1"}}"#,
);

fn int_kv(key: &str, value: i64) -> ourios_core::otlp::KeyValue {
    ourios_core::otlp::KeyValue {
        key: key.to_owned(),
        value: Some(ourios_core::otlp::AnyValue {
            value: Some(ourios_core::otlp::any_value::Value::IntValue(value)),
        }),
        key_strindex: 0,
    }
}

fn with_attributes(i: u64, attributes: Vec<ourios_core::otlp::KeyValue>) -> MinedRecord {
    MinedRecord {
        attributes,
        ..record(i)
    }
}

/// `p` is promoted as a string and `i` as an `i64`; `k` is not promoted.
fn promoted() -> PromotedAttributes {
    PromotedAttributes::new_typed(
        [],
        [
            PromotedKey::string("p"),
            PromotedKey {
                key: "i".to_owned(),
                class: PromotedClass::I64,
            },
        ],
    )
}

/// Row 0 matches `x`/`1`, row 1 holds `y`/`2`. Rows 2 and 3 have the bad
/// cell: row 2 was written from `p = "x"`, `i = 1`, so its promoted columns
/// are set; row 3 from no attributes, so they are `NULL` and a promoted
/// comparison falls back to the bad cell. Row 4's cell does not decode and
/// holds none of the pairs, so the `LIKE` prefilter rules it out before
/// the comparison runs: only the guard keeps `not` from matching it.
fn write(bucket: &std::path::Path) {
    let records = [
        with_attributes(0, vec![kv("k", "x"), kv("p", "x"), int_kv("i", 1)]),
        with_attributes(1, vec![kv("k", "y"), kv("p", "y"), int_kv("i", 2)]),
        with_attributes(2, vec![kv("k", "x"), kv("p", "x"), int_kv("i", 1)]),
        with_attributes(3, Vec::new()),
        with_attributes(4, Vec::new()),
    ];
    let batch =
        ourios_parquet::mined_records_to_batch_with_promoted(&records, &promoted()).expect("batch");
    let batch = fixture::corrupt(&batch, columns::ATTRIBUTES, 2, BAD.as_bytes());
    let batch = fixture::corrupt(&batch, columns::ATTRIBUTES, 3, BAD.as_bytes());
    let batch = fixture::corrupt(&batch, columns::ATTRIBUTES, 4, b"not json");
    fixture::write_batch(bucket, &records[0], &batch);
}

async fn run(q: &Querier, src: &str) -> QueryResult {
    let query = ourios_querier::dsl::parse(&format!("{src} | limit 10")).expect("parse");
    q.run_query(
        &query,
        &TenantId::new("a"),
        NOW,
        DEFAULT_WINDOW_NS,
        Some(&no_aliases()),
    )
    .await
    .unwrap_or_else(|e| panic!("{src}: the query does not fail: {e:?}"))
}

/// The rows `src` matches, by their index in [`write`].
async fn matching(q: &Querier, src: &str) -> Vec<u64> {
    let result = run(q, src).await;
    let mut rows: Vec<u64> = result
        .records
        .iter()
        .map(|row| row.time_unix_nano - TS0)
        .collect();
    rows.sort_unstable();
    rows
}

/// Scenario RFC0069.7 — a filter over an undecodable cell does not fail
/// the query. See `docs/rfcs/0069-nested-value-read-depth.md` §5.
#[tokio::test]
async fn rfc0069_7_a_filter_over_an_undecodable_cell_does_not_fail_the_query() {
    let bucket = tempfile::TempDir::new().expect("temp");
    write(bucket.path());
    let q = Querier::new(bucket.path());

    let cases: [(&str, &[u64]); 12] = [
        // The JSON arm alone: the bad rows' text holds `k = "x"`, yet
        // neither the comparison nor its negation matches them.
        (r#"attr.k == "x""#, &[0]),
        (r#"not (attr.k == "x")"#, &[1]),
        // A promoted key answers from its typed column where it is set
        // (row 2), and falls back to the bad cell where it is NULL (row 3).
        (r#"attr.p == "x""#, &[0, 2]),
        (r#"not (attr.p == "x")"#, &[1]),
        (r#"attr.p != "x""#, &[1]),
        (r#"not (attr.p != "x")"#, &[0, 2]),
        ("attr.i == 1", &[0, 2]),
        ("not (attr.i == 1)", &[1]),
        // Polarity through nesting and compounds, by SQL three-valued
        // logic: a bad cell's comparison is NULL however it is combined.
        (r#"not (not (attr.k == "x"))"#, &[0]),
        (r#"not (attr.k == "x" or attr.k == "y")"#, &[]),
        // A healthy cell without the key is a plain false, so its negation
        // matches; the bad cells stay NULL.
        (r#"not (attr.q == "x")"#, &[0, 1]),
        // The resource cells all decode, so only the attribute arm is NULL.
        (
            r#"not (attr.k == "x") and not (resource.service.name == "nope")"#,
            &[1],
        ),
    ];
    for (src, want) in cases {
        assert_eq!(matching(&q, src).await, want, "{src}");
    }

    // And a query that only filters on the bad cells, returning none of
    // their rows, logs and counts none of them.
    let events = Events::default();
    let _guard = tracing::subscriber::set_default(tracing_subscriber::layer::SubscriberExt::with(
        tracing_subscriber::registry(),
        events.clone(),
    ));
    let result = run(&q, r#"not (attr.k == "x")"#).await;
    assert_eq!(result.records.len(), 1, "only row 1 returns");
    assert_eq!(result.stats.undecodable_cells, UndecodableCells::default());
    assert!(events.0.lock().expect("events").is_empty());
}

/// Two rows the `LIKE` prefilter misses for `a.b == "x"`: row 0 holds a
/// healthy `a = {b: "y"}`, row 1 a closed list whose scan along `a.b` hits
/// an unterminated string, and row 2 a healthy list wrapped in JSON
/// whitespace, which the reader accepts.
fn write_paths(bucket: &std::path::Path) {
    let records = [
        with_attributes(0, Vec::new()),
        with_attributes(1, Vec::new()),
        with_attributes(2, vec![kv("k", "x")]),
    ];
    let batch = ourios_parquet::mined_records_to_batch(&records).expect("batch");
    let healthy = r#"[{"key":"a","value":{"kvlistValue":{"values":[{"key":"b","value":{"stringValue":"y"}}]}}}]"#;
    let broken =
        r#"[{"key":"a","value":{"kvlistValue":{"values":[{"key":"b","value":{"stringValue":"y]"#;
    let spaced = " \n[{\"key\":\"k\",\"value\":{\"stringValue\":\"x\"}}]\t";
    let batch = fixture::corrupt(&batch, columns::ATTRIBUTES, 0, healthy.as_bytes());
    let batch = fixture::corrupt(&batch, columns::ATTRIBUTES, 1, broken.as_bytes());
    let batch = fixture::corrupt(&batch, columns::ATTRIBUTES, 2, spaced.as_bytes());
    fixture::write_batch(bucket, &records[0], &batch);
}

/// A cell whose scan along the key's path fails is `NULL` where the
/// comparison runs. The branch a negation takes for a row its prefilter
/// rules out checks only that the cell is a closed list, so the same cell
/// matches there (RFC 0069 §3.2, maintainer, 2026-11-23). A healthy cell
/// wrapped in JSON whitespace is readable.
#[tokio::test]
async fn a_path_scan_failure_is_null_where_the_comparison_runs() {
    let bucket = tempfile::TempDir::new().expect("temp");
    write_paths(bucket.path());
    let q = Querier::new(bucket.path());

    let cases: [(&str, &[u64]); 4] = [
        (r#"attr.a.b != "x""#, &[0]),
        // The prefilter rules row 1 out, and its cell is a closed list.
        (r#"not (attr.a.b == "x")"#, &[0, 1, 2]),
        (r#"attr.k == "x""#, &[2]),
        // Row 1's broken map is not on `k`'s path, so its scan for `k` does
        // not fail: a cell corrupted inside but closed is read by its text
        // (RFC 0069 §3.2), and holds no `k`.
        (r#"not (attr.k == "x")"#, &[0, 1]),
    ];
    for (src, want) in cases {
        assert_eq!(matching(&q, src).await, want, "{src}");
    }
}
