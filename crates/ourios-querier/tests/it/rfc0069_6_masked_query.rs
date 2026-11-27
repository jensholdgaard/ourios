//! RFC0069.6 through the query path: a metadata-only reader querying
//! RFC0069.3's file gets each reserved fallback attribute with its value
//! masked whole, and none of the stored JSON. The masking rule itself is
//! unit-tested in `src/visibility.rs`.

use ourios_core::tenant::TenantId;
use ourios_querier::{LogBody, LogRow, Querier, QueryOptions, Visibility};

use crate::common::{DEFAULT_WINDOW_NS, NOW, no_aliases};
use crate::rfc0069_fixture as fixture;

async fn query(bucket: &std::path::Path, visibility: Visibility) -> Vec<LogRow> {
    let query = ourios_querier::dsl::parse("true | limit 10").expect("parse DSL");
    Querier::new(bucket)
        .run_query_with(
            &query,
            &TenantId::new("a"),
            NOW,
            DEFAULT_WINDOW_NS,
            Some(&no_aliases()),
            QueryOptions::default().with_visibility(visibility),
        )
        .await
        .expect("the query does not fail")
        .records
}

/// Each reserved fallback attribute of `rows`, as (list, value present).
fn fallbacks(rows: &[LogRow]) -> Vec<(&'static str, bool)> {
    let mut found = Vec::new();
    for row in rows {
        for (list, attributes) in [
            ("attributes", &row.attributes),
            ("resource_attributes", &row.resource_attributes),
            ("scope_attributes", &row.scope_attributes),
        ] {
            found.extend(
                attributes
                    .iter()
                    .filter(|kv| kv.key == ourios_semconv::OURIOS_ATTRIBUTE_UNDECODABLE)
                    .map(|kv| (list, kv.value.is_some())),
            );
        }
    }
    found.sort_unstable();
    found
}

/// Scenario RFC0069.6: under a tenant with a content key masked from a
/// metadata-only reader, a query over RFC0069.3's file returns every
/// fallback with its value masked whole, and no stored JSON anywhere in
/// the result; a tenant-wide reader sees it. See
/// `docs/rfcs/0069-nested-value-read-depth.md` §5.
#[tokio::test]
async fn rfc0069_6_a_masked_reader_gets_no_stored_json_from_a_query() {
    let bucket = tempfile::TempDir::new().expect("temp");
    fixture::write_fixture(bucket.path());
    let masked = Visibility::Masked {
        content_columns: vec!["body".to_owned(), "attr.k".to_owned()],
    };

    let rows = query(bucket.path(), masked).await;

    assert_eq!(rows.len(), 4, "every row returns");
    assert!(
        rows.iter().all(|row| row.body == LogBody::Masked),
        "every body masked, the undecodable ones included"
    );
    assert_eq!(
        fallbacks(&rows),
        [
            ("attributes", false),
            ("resource_attributes", false),
            ("scope_attributes", false),
            ("scope_attributes", false),
        ],
        "every fallback masked whole"
    );
    // `{:?}` escapes the stored JSON's quotes, so it is searched for in
    // its escaped form.
    let result = format!("{rows:?}");
    for stored in [fixture::BAD_ATTRIBUTES, fixture::BAD_SCOPE] {
        let escaped = format!("{stored:?}");
        let escaped = escaped.trim_matches('"');
        assert!(!result.contains(escaped), "{stored} leaked: {result}");
    }

    let open = query(bucket.path(), Visibility::TenantWide).await;
    assert!(
        fallbacks(&open).iter().all(|&(_, present)| present),
        "a tenant-wide reader sees the stored JSON"
    );
    let open = format!("{open:?}");
    let escaped = format!("{:?}", fixture::BAD_ATTRIBUTES);
    assert!(
        open.contains(escaped.trim_matches('"')),
        "the search finds the stored JSON when it is there"
    );
}
