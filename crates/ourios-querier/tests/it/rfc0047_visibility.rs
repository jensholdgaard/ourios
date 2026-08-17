//! RFC 0047 §3.4 at the engine (RFC0047.5/.6/.8, the predicate-composition
//! half of §6): a scoped [`Visibility`] becomes an `IN (…)` over the
//! promoted conversation column OR'd with the self fast path; a masked one
//! returns every row with the content columns nulled and rejects a filter or
//! aggregation on them; tenant-wide is today's plan. The graph decision is
//! the caller's — here it is handed in directly.
//! See `docs/rfcs/0047-rebac-resolver-and-graph-visibility.md` §5.

use ourios_core::tenant::TenantId;
use ourios_parquet::{PromotedAttributes, PromotedClass, PromotedKey};
use ourios_querier::{
    LogBody, Querier, QueryError, QueryOptions, QueryResult, SelfMatch, Visibility,
};

use crate::common::{
    DEFAULT_WINDOW_NS, NOW, TS0, kv, no_aliases, rec_with_attrs, write_all_with_promoted,
};

const CONVERSATION: &str = "attr.gen_ai.conversation.id";
const USER_HASH: &str = "attr.user.hash";

fn kv_double(key: &str, value: f64) -> ourios_core::otlp::KeyValue {
    ourios_core::otlp::KeyValue {
        key: key.to_string(),
        value: Some(ourios_core::otlp::AnyValue {
            value: Some(ourios_core::otlp::any_value::Value::DoubleValue(value)),
        }),
        ..Default::default()
    }
}

/// Six rows in `acme`: conversations c-1..c-3 (two rows each), each with a
/// `user.hash`, a `gen_ai.input.messages` content attribute and a
/// `cost_usd`; plus one `globex` row that shares an id with `acme/c-1`.
fn seed() -> tempfile::TempDir {
    let bucket = tempfile::TempDir::new().expect("temp");
    let promoted = PromotedAttributes::new_typed(
        [],
        [
            PromotedKey::string("gen_ai.conversation.id".to_string()),
            PromotedKey::string("user.hash".to_string()),
            PromotedKey::string("model".to_string()),
            PromotedKey {
                key: "cost_usd".into(),
                class: PromotedClass::F64,
            },
        ],
    );
    let row = |tenant: &str, i: u64, conversation: &str, user: &str| {
        rec_with_attrs(
            tenant,
            TS0 + i * 1_000,
            vec![kv("service.name", "agent")],
            vec![
                kv("gen_ai.conversation.id", conversation),
                kv("user.hash", user),
                kv("gen_ai.input.messages", "the secret prompt"),
                kv("model", "gpt"),
                kv_double("cost_usd", 1.5),
            ],
        )
    };
    let recs = vec![
        row("acme", 1, "c-1", "alice"),
        row("acme", 2, "c-1", "alice"),
        row("acme", 3, "c-2", "bob"),
        row("acme", 4, "c-2", "carol"),
        row("acme", 5, "c-3", "carol"),
        row("acme", 6, "c-3", "carol"),
        row("globex", 7, "c-1", "mallory"),
    ];
    write_all_with_promoted(bucket.path(), &recs, &promoted);
    bucket
}

async fn run(
    bucket: &std::path::Path,
    dsl: &str,
    visibility: Visibility,
) -> Result<QueryResult, QueryError> {
    let query = ourios_querier::dsl::parse(dsl).expect("parse DSL");
    Querier::new(bucket)
        .run_query_with(
            &query,
            &TenantId::new("acme"),
            NOW,
            DEFAULT_WINDOW_NS,
            Some(&no_aliases()),
            QueryOptions::default().with_visibility(visibility),
        )
        .await
}

fn conversations(result: &QueryResult) -> Vec<String> {
    let mut ids: Vec<String> = result
        .records
        .iter()
        .map(|row| {
            row.attributes
                .iter()
                .find(|kv| kv.key == "gen_ai.conversation.id")
                .and_then(|kv| kv.value.as_ref())
                .and_then(|v| match &v.value {
                    Some(ourios_core::otlp::any_value::Value::StringValue(s)) => Some(s.clone()),
                    _ => None,
                })
                .expect("conversation id")
        })
        .collect();
    ids.sort();
    ids
}

fn scoped(ids: &[&str], self_match: Option<&str>) -> Visibility {
    Visibility::Scoped {
        column: CONVERSATION.to_string(),
        ids: ids.iter().map(|s| (*s).to_string()).collect(),
        self_match: self_match.map(|value| SelfMatch {
            column: USER_HASH.to_string(),
            value: value.to_string(),
        }),
    }
}

/// RFC0047.5/.6 (engine half): exactly the scoped ids' rows return, the
/// self fast path adds the principal's own rows, an empty scope with no
/// fast path is an empty result (not an error), and the tenant-wide branch
/// is untouched. `true | limit 100` is a match-all query.
#[tokio::test]
async fn scoped_visibility_filters_to_the_ids_and_self() {
    let bucket = seed();
    let all = run(bucket.path(), "true | limit 100", Visibility::TenantWide)
        .await
        .expect("query");
    assert_eq!(all.rows, 6, "tenant-wide: every acme row (never globex)");

    let bob = run(bucket.path(), "true | limit 100", scoped(&["c-2"], None))
        .await
        .expect("query");
    assert_eq!(conversations(&bob), ["c-2", "c-2"]);
    assert_eq!(bob.rows, 2, "the count follows the same predicate");

    // Scoped to c-2 plus the self fast path on `carol` picks up c-3 too.
    let carol = run(
        bucket.path(),
        "true | limit 100",
        scoped(&["c-2"], Some("carol")),
    )
    .await
    .expect("query");
    assert_eq!(conversations(&carol), ["c-2", "c-2", "c-3", "c-3"]);

    // The user's own predicate composes (AND) with the visibility filter.
    let narrowed = run(
        bucket.path(),
        "attr.user.hash == \"bob\" | limit 100",
        scoped(&["c-2", "c-3"], None),
    )
    .await
    .expect("query");
    assert_eq!(narrowed.rows, 1);

    let nothing = run(bucket.path(), "true | limit 100", scoped(&[], None))
        .await
        .expect("empty scope is not an error");
    assert_eq!(nothing.rows, 0);
    assert!(nothing.records.is_empty());

    // Aggregations run over the scoped rows only.
    let spend = run(
        bucket.path(),
        "true | sum(attr.cost_usd) by attr.model",
        scoped(&["c-1"], None),
    )
    .await
    .expect("query");
    let groups = spend.aggregate.expect("aggregate");
    assert_eq!(groups.len(), 1);
    let value = groups[0].value.flatten().expect("sum");
    assert!((value - 3.0).abs() < 1e-9, "two c-1 rows × 1.5");
}

/// RFC0047.8 (engine half): a masked reader gets every row with the
/// content columns nulled — body `Masked`, the attribute's value unset —
/// while other attributes and aggregations over metadata are intact; a
/// filter or aggregation on a content column is `Forbidden` naming it.
#[tokio::test]
async fn masked_visibility_nulls_content_and_forbids_reading_it() {
    let bucket = seed();
    let masked = Visibility::Masked {
        content_columns: vec!["body".to_string(), "attr.gen_ai.input.messages".to_string()],
    };
    let rows = run(bucket.path(), "true | limit 100", masked.clone())
        .await
        .expect("query");
    assert_eq!(rows.rows, 6, "every row of the tenant");
    for row in &rows.records {
        assert_eq!(row.body, LogBody::Masked);
        let content = row
            .attributes
            .iter()
            .find(|kv| kv.key == "gen_ai.input.messages")
            .expect("key kept");
        assert!(content.value.is_none(), "value unset");
        assert!(
            row.attributes
                .iter()
                .any(|kv| kv.key == "model" && kv.value.is_some()),
            "metadata intact"
        );
    }
    let spend = run(
        bucket.path(),
        "true | sum(attr.cost_usd) by attr.model",
        masked.clone(),
    )
    .await
    .expect("metadata aggregation");
    let groups = spend.aggregate.expect("aggregate");
    let value = groups[0].value.flatten().expect("sum");
    assert!((value - 9.0).abs() < 1e-9, "six rows × 1.5");

    for dsl in [
        "attr.gen_ai.input.messages == \"x\"",
        "contains(body, \"secret\")",
        "true | count by attr.gen_ai.input.messages",
    ] {
        match run(bucket.path(), dsl, masked.clone()).await {
            Err(QueryError::Forbidden { column }) => assert!(
                column == "body" || column == "attr.gen_ai.input.messages",
                "{dsl}: {column}"
            ),
            other => panic!("{dsl}: expected Forbidden, got {other:?}"),
        }
    }
}
