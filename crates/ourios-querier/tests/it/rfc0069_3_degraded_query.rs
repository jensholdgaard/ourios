//! RFC0069.3 through the query path: a query over a file holding
//! undecodable cells returns every row, each bad cell degraded alone, and
//! names each bad cell once with its file and row. The reader leg lives in
//! `ourios-parquet/tests/it/rfc0069_3_undecodable_cells.rs`.

use ourios_core::otlp::KeyValue;
use ourios_core::tenant::TenantId;
use ourios_miner::reconstruct::Reconstruction;
use ourios_parquet::columns;
use ourios_querier::{LogBody, LogRow, Querier, QueryRequest};

use crate::rfc0069_fixture as fixture;

use fixture::{
    BAD_ATTRIBUTES, BAD_RESOURCE, BAD_SCOPE, BAD_STRUCTURED, Events, NOT_UTF8, TS0, kv, record,
    write_fixture,
};

fn row_at(rows: &[LogRow], i: u64) -> &LogRow {
    rows.iter()
        .find(|row| row.time_unix_nano == TS0 + i)
        .expect("row returned")
}

fn fallback(stored: &str) -> Vec<KeyValue> {
    vec![kv(ourios_semconv::OURIOS_ATTRIBUTE_UNDECODABLE, stored)]
}

/// Scenario RFC0069.3 (query): every row returns, each bad cell degraded
/// alone, and one `WARN` event names each bad cell's file, row and column.
/// See `docs/rfcs/0069-nested-value-read-depth.md` §5.
#[tokio::test]
async fn rfc0069_3_a_query_returns_every_row_with_each_bad_cell_degraded() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let bucket = tempfile::TempDir::new().expect("temp");
    write_fixture(bucket.path());
    let events = Events::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(events.clone()));

    let result = Querier::new(bucket.path())
        .run(QueryRequest {
            tenant: TenantId::new("a"),
            time_range: None,
            template_id: None,
            severity_text: None,
            limit: Some(10),
        })
        .await
        .expect("the query does not fail");
    let rows = &result.records;
    assert_eq!(rows.len(), 4, "every row returns");

    let healthy = row_at(rows, 0);
    assert_eq!(healthy.attributes, record(0).attributes);

    let bad = row_at(rows, 1);
    assert_eq!(bad.attributes, fallback(BAD_ATTRIBUTES));
    assert_eq!(bad.resource_attributes, fallback(BAD_RESOURCE));
    assert_eq!(bad.scope_attributes, fallback(BAD_SCOPE));

    let mixed = row_at(rows, 2);
    assert_eq!(mixed.scope_attributes, fallback(BAD_SCOPE));
    assert_eq!(mixed.attributes, record(2).attributes, "valid cells decode");
    assert_eq!(
        mixed.body,
        LogBody::Rendered {
            line: String::from_utf8_lossy(NOT_UTF8).into_owned().into_bytes(),
            reconstruction: Reconstruction::RetainedLossy,
        }
    );

    assert_eq!(
        row_at(rows, 3).body,
        LogBody::Rendered {
            line: BAD_STRUCTURED.as_bytes().to_vec(),
            reconstruction: Reconstruction::RetainedVerbatim,
        }
    );

    let cells = result.stats.undecodable_cells;
    assert_eq!(
        (
            cells.body,
            cells.attributes,
            cells.resource_attributes,
            cells.scope_attributes
        ),
        (2, 1, 1, 2)
    );

    let mut seen = events.0.lock().expect("events").clone();
    seen.sort();
    let expected = [
        ("1", columns::ATTRIBUTES),
        ("1", columns::RESOURCE_ATTRIBUTES),
        ("1", columns::SCOPE_ATTRIBUTES),
        ("2", columns::BODY),
        ("2", columns::SCOPE_ATTRIBUTES),
        ("3", columns::BODY),
    ];
    assert_eq!(seen.len(), expected.len(), "{seen:?}");
    for ((file, row, column), (want_row, want_column)) in seen.iter().zip(expected) {
        assert!(file.ends_with("degraded.parquet"), "{file}");
        assert_eq!((row.as_str(), column.as_str()), (want_row, want_column));
    }
}
