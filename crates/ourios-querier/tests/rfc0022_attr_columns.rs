//! RFC 0022 §5 — query-side promoted attribute columns.
//!
//! Scenarios RFC0022.3–.7 (old-file parity, operator gating, pruning,
//! projection-blind read path, promoted-set drift). The writer-side
//! scenarios (`.1`/`.2`) live in
//! `crates/ourios-parquet/tests/rfc0022_promoted_columns.rs`.
//!
//! The pre-amendment file is the committed
//! `testdata/rfc0022/pre-amendment.parquet` fixture (the RFC 0021 §6
//! committed-fixture discipline): it carries the base RFC 0005 schema with
//! **no** promoted columns, which the current writer can no longer produce
//! (`service.name` is implicitly promoted on every path since RFC 0022).

mod common;

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

use common::{
    DEFAULT_WINDOW_NS, NOW, TS0, kv, no_aliases, rec_with_attrs, write_all_with_promoted,
};
use ourios_core::otlp::any_value::Value as AvValue;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::MinedRecord;
use ourios_core::tenant::TenantId;
use ourios_parquet::{
    PartitionKey, PromotedAttributes, mined_records_to_batch, mined_records_to_batch_with_promoted,
};
use ourios_querier::{Querier, QueryError, QueryRequest};

fn kv_int(key: &str, value: i64) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(AvValue::IntValue(value)),
        }),
        ..Default::default()
    }
}

/// The rows inside the committed pre-amendment fixture. The generator wrote
/// exactly these, so tests can hand-compute per-file expectations.
fn pre_records() -> Vec<MinedRecord> {
    vec![
        MinedRecord {
            template_id: 101,
            ..rec_with_attrs(
                "a",
                TS0,
                vec![kv("service.name", "api"), kv("k8s.namespace.name", "prod")],
                vec![kv("http.route", "/cart")],
            )
        },
        MinedRecord {
            template_id: 102,
            ..rec_with_attrs(
                "a",
                TS0 + 1_000,
                vec![kv("service.name", "web")],
                vec![kv("http.route", "/login")],
            )
        },
        MinedRecord {
            template_id: 103,
            // Non-string value: projects NULL under promotion, and has no
            // `stringValue` for the JSON arm — matches nothing either way.
            ..rec_with_attrs(
                "a",
                TS0 + 2_000,
                vec![kv("service.name", "api"), kv_int("k8s.namespace.name", 3)],
                Vec::new(),
            )
        },
    ]
}

/// The post-amendment rows, written under the promoted set
/// `{resource: k8s.namespace.name, log: http.route}` (plus the implicit
/// `service.name`). `http.request.method` is deliberately present in the
/// JSON but NOT promoted, so RFC0022.4 can drive the non-promoted rejection
/// against a key that actually exists in the data.
fn post_records() -> Vec<MinedRecord> {
    vec![
        MinedRecord {
            template_id: 201,
            ..rec_with_attrs(
                "a",
                TS0 + 10_000,
                vec![kv("service.name", "api"), kv("k8s.namespace.name", "prod")],
                vec![kv("http.route", "/cart"), kv("http.request.method", "GET")],
            )
        },
        MinedRecord {
            template_id: 202,
            ..rec_with_attrs(
                "a",
                TS0 + 11_000,
                vec![kv("service.name", "web"), kv("k8s.namespace.name", "dev")],
                vec![
                    kv("http.route", "/checkout"),
                    kv("http.request.method", "GET"),
                ],
            )
        },
        MinedRecord {
            template_id: 203,
            ..rec_with_attrs(
                "a",
                TS0 + 12_000,
                vec![kv("service.name", "api"), kv_int("k8s.namespace.name", 3)],
                Vec::new(),
            )
        },
    ]
}

fn promoted_set() -> PromotedAttributes {
    PromotedAttributes::new(
        ["k8s.namespace.name".to_string()],
        ["http.route".to_string()],
    )
}

/// The committed fixture's location (the established repo pattern for
/// `testdata/` paths — parent-walk from `CARGO_MANIFEST_DIR`, no `..`
/// components in the resulting path).
fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root is two levels above CARGO_MANIFEST_DIR")
        .join("testdata/rfc0022/pre-amendment.parquet")
}

/// Copy the committed pre-amendment fixture into `bucket` at the partition
/// path its rows derive to, so the querier scans it like any other file.
/// The installed name sorts *before* the writer's `UUIDv7` file names, so a
/// scan-schema regression to first-file-only inference (rather than the
/// §3.9 union) surfaces as a deterministic RFC0022.4 failure instead of
/// listing-order luck.
fn install_pre_amendment_fixture(bucket: &Path) {
    let dir = PartitionKey::derive(&pre_records()[0])
        .expect("derive partition")
        .data_path(bucket);
    std::fs::create_dir_all(&dir).expect("mkdir partition");
    std::fs::copy(fixture_path(), dir.join("0-pre-amendment.parquet")).expect("install fixture");
}

async fn count(q: &Querier, src: &str) -> u64 {
    let query = ourios_querier::dsl::parse(src).expect("parse");
    q.run_query(
        &query,
        &TenantId::new("a"),
        NOW,
        DEFAULT_WINDOW_NS,
        Some(&no_aliases()),
    )
    .await
    .expect("run_query")
    .rows
}

async fn query_err(q: &Querier, src: &str) -> QueryError {
    let query = ourios_querier::dsl::parse(src).expect("parse");
    q.run_query(
        &query,
        &TenantId::new("a"),
        NOW,
        DEFAULT_WINDOW_NS,
        Some(&no_aliases()),
    )
    .await
    .expect_err("query must be rejected")
}

/// Regenerates `testdata/rfc0022/pre-amendment.parquet` (the RFC 0021 §6
/// committed-fixture discipline). The base [`mined_records_to_batch`] schema
/// carries no promoted columns — exactly what the pre-RFC 0022 writer
/// declared. Run manually (`cargo test -p ourios-querier --test
/// rfc0022_attr_columns -- --ignored rfc0022_fixture`); never in CI.
#[test]
#[ignore = "fixture generator — run manually to (re)create the committed pre-amendment file"]
fn rfc0022_fixture_writes_the_pre_amendment_file() {
    let batch = mined_records_to_batch(&pre_records()).expect("base-schema batch");
    std::fs::create_dir_all(fixture_path().parent().expect("parent")).expect("mkdir testdata");
    let file = File::create(fixture_path()).expect("create fixture");
    let mut w = ArrowWriter::try_new(file, batch.schema(), None).expect("arrow writer");
    w.write(&batch).expect("write batch");
    w.close().expect("close writer");
}

/// Scenario RFC0022.3 — old files answer identically (§3.9 / §3.4).
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
///
/// The oracle is live: the pre-only bucket's union schema carries no
/// promoted column, so it compiles to the pure-`LIKE` form (the exact
/// pre-RFC 0022 code path); the combined bucket compiles to the two-arm
/// form. Equality of `combined == pre_only + post_only` for every query is
/// the row-for-row parity claim, and the template-scoped conjunctions pin
/// *which* rows matched.
#[tokio::test]
async fn rfc0022_3_pre_amendment_files_answer_identically() {
    let pre_bucket = TempDir::new().expect("temp");
    install_pre_amendment_fixture(pre_bucket.path());
    let post_bucket = TempDir::new().expect("temp");
    write_all_with_promoted(post_bucket.path(), &post_records(), &promoted_set());
    let combined = TempDir::new().expect("temp");
    install_pre_amendment_fixture(combined.path());
    write_all_with_promoted(combined.path(), &post_records(), &promoted_set());

    let q_pre = Querier::new(pre_bucket.path());
    let q_post = Querier::new(post_bucket.path());
    let q_all = Querier::new(combined.path());

    for (query, want_pre, want_post) in [
        (r#"service == "api""#, 2, 2),
        (r#"service != "api""#, 1, 1),
        (r#"attr.http.route == "/cart""#, 1, 1),
        (r#"attr.http.route != "/cart""#, 1, 1),
        (r#"resource.k8s.namespace.name == "prod""#, 1, 1),
        // Non-string values (`k8s.namespace.name` as an int) match neither
        // the typed arm (NULL cell) nor the JSON arm (no `stringValue`).
        (r#"resource.k8s.namespace.name != "prod""#, 0, 1),
    ] {
        let pre = count(&q_pre, query).await;
        let post = count(&q_post, query).await;
        let all = count(&q_all, query).await;
        assert_eq!(
            pre, want_pre,
            "{query}: pure-LIKE compile, pre-amendment file only"
        );
        assert_eq!(
            post, want_post,
            "{query}: two-arm compile, post-amendment file only"
        );
        assert_eq!(
            all,
            pre + post,
            "{query}: the union scan answers identically to the per-file compiles"
        );
    }

    // Row identity, not just counts: the `/cart` matches are exactly the
    // pre-amendment row 101 (via the JSON fallback arm) and the
    // post-amendment row 201 (via the typed arm).
    for tid in [101, 201] {
        assert_eq!(
            count(
                &q_all,
                &format!(r#"attr.http.route == "/cart" and template_id == {tid}"#)
            )
            .await,
            1,
            "template {tid} is one of the /cart matches"
        );
    }
}

/// Scenario RFC0022.4 — full operator set on promoted keys only.
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[tokio::test]
async fn rfc0022_4_operator_set_gated_on_promotion() {
    let bucket = TempDir::new().expect("temp");
    install_pre_amendment_fixture(bucket.path());
    write_all_with_promoted(bucket.path(), &post_records(), &promoted_set());
    let q = Querier::new(bucket.path());

    // Ordering / regex answer from the typed arm only: pre-amendment rows
    // whose JSON values would match ("/login" >= "/ch", "/cart" =~ ^/c)
    // never do — their promoted cells read as NULL (§3.3 silent non-match).
    assert_eq!(
        count(&q, r#"attr.http.route >= "/ch""#).await,
        1,
        "/checkout only"
    );
    assert_eq!(
        count(&q, r#"attr.http.route =~ "^/c""#).await,
        2,
        "/cart + /checkout"
    );
    // `!~` keeps NULL-never-matches: every post row either matches ^/c or
    // has no route; every pre row is NULL.
    assert_eq!(count(&q, r#"attr.http.route !~ "^/c""#).await, 0);
    // The implicit `service.name` promotion answers the new operators too.
    assert_eq!(
        count(&q, r#"service =~ "^a""#).await,
        2,
        "post-amendment api rows"
    );

    // ==/!= continue to work as today (the §3.3 two-arm form).
    assert_eq!(count(&q, r#"attr.http.route == "/cart""#).await, 2);
    assert_eq!(count(&q, r#"attr.http.route != "/cart""#).await, 2);

    // A non-promoted key rejects ordering/regex with InvalidQuery — even
    // though the key exists in the JSON — while ==/!= stay on the JSON arm.
    for query in [
        r#"attr.http.request.method > "A""#,
        r#"attr.http.request.method =~ "^G""#,
    ] {
        match query_err(&q, query).await {
            QueryError::InvalidQuery { detail } => assert!(
                detail.contains("non-promoted"),
                "{query}: rejection names the non-promoted gate: {detail}"
            ),
            other => panic!("{query}: expected InvalidQuery, got {other:?}"),
        }
    }
    assert_eq!(count(&q, r#"attr.http.request.method == "GET""#).await, 2);
    assert_eq!(count(&q, r#"attr.http.request.method != "GET""#).await, 0);
}

/// Scenario RFC0022.6 — the read path is projection-blind (§3.1).
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[tokio::test]
async fn rfc0022_6_read_path_is_projection_blind() {
    let record = MinedRecord {
        template_id: 301,
        ..rec_with_attrs(
            "a",
            TS0,
            vec![kv("service.name", "real-svc")],
            vec![kv("http.route", "/real")],
        )
    };
    let promoted = PromotedAttributes::new([], ["http.route".to_string()]);
    let batch = mined_records_to_batch_with_promoted(std::slice::from_ref(&record), &promoted)
        .expect("batch");

    // Hand-forge the file: both promoted cells disagree with the JSON truth.
    let schema = batch.schema();
    let mut cols: Vec<ArrayRef> = batch.columns().to_vec();
    cols[schema
        .index_of("resource.service.name")
        .expect("svc column")] = Arc::new(StringArray::from(vec!["forged-svc"]));
    cols[schema.index_of("attr.http.route").expect("route column")] =
        Arc::new(StringArray::from(vec!["/forged"]));
    let forged = RecordBatch::try_new(schema.clone(), cols).expect("forged batch");

    let bucket = TempDir::new().expect("temp");
    let dir = PartitionKey::derive(&record)
        .expect("derive partition")
        .data_path(bucket.path());
    std::fs::create_dir_all(&dir).expect("mkdir partition");
    let file = File::create(dir.join("forged.parquet")).expect("create parquet");
    let mut w = ArrowWriter::try_new(file, schema, None).expect("arrow writer");
    w.write(&forged).expect("write batch");
    w.close().expect("close writer");

    // The RFC 0017 read path: rows round-trip from the JSON columns; the
    // forged promoted cells are invisible.
    let result = Querier::new(bucket.path())
        .run(QueryRequest {
            tenant: TenantId::new("a"),
            time_range: Some((TS0 - 1, TS0 + 1)),
            template_id: None,
            severity_text: None,
            limit: Some(10),
        })
        .await
        .expect("query");
    assert_eq!(result.rows, 1, "the forged file's row is scanned");
    let row = &result.records[0];
    assert_eq!(
        row.resource_attributes,
        vec![kv("service.name", "real-svc")]
    );
    assert_eq!(row.attributes, vec![kv("http.route", "/real")]);
    let shown = format!("{row:?}");
    assert!(
        !shown.contains("forged"),
        "no forged promoted cell leaks into the returned row: {shown}"
    );
}

/// Scenario RFC0022.5 — promoted predicates prune (pillar 2).
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
#[ignore = "RFC0022.5 stub — implemented in the pruning green slice"]
fn rfc0022_5_promoted_predicates_prune() {
    todo!(
        "RFC0022.5 — a selective equality query on a promoted key shows \
         pruned > 0 via the RFC 0016 scanned/pruned counters on a \
         multi-row-group corpus; B1/B2 unchanged"
    );
}

/// Scenario RFC0022.7 — promoted-set drift across deploys (§3.4).
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
#[test]
#[ignore = "RFC0022.7 stub — implemented in the pruning green slice"]
fn rfc0022_7_promoted_set_drift_unions_cleanly() {
    todo!(
        "RFC0022.7 — one scan over files written under configured sets {{}}, \
         {{a}}, {{a,b}} unions schemas without error; predicates on a and b \
         answer correctly from every file"
    );
}
