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

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use parquet::arrow::ArrowWriter;
use tempfile::TempDir;

use crate::common::{
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
///
/// The `rfc0007_1` shape (counters, not wall-clock): three files in distinct
/// hours (⇒ distinct row groups), the needle value concentrated in one of
/// them. Every row carries the promoted key, so in the non-matching row
/// groups the typed arm is excluded by min/max statistics *and* the
/// fallback arm by a zero null-count (`P IS NULL` can never hold) — §3.3's
/// steady-state fast path. B1/B2 are the bench gates, unchanged by
/// construction (this suite asserts the counters only).
#[tokio::test]
async fn rfc0022_5_promoted_predicates_prune() {
    let bucket = TempDir::new().expect("temp");
    for (i, ns) in ["alpha", "beta", "needle"].iter().enumerate() {
        let hour = u64::try_from(i).expect("small index") * crate::common::HOUR_NS;
        let recs: Vec<MinedRecord> = (0..3u64)
            .map(|j| MinedRecord {
                template_id: 500 + j,
                ..rec_with_attrs(
                    "a",
                    TS0 + hour + j * 1_000,
                    vec![kv("service.name", "api"), kv("k8s.namespace.name", ns)],
                    Vec::new(),
                )
            })
            .collect();
        write_all_with_promoted(bucket.path(), &recs, &promoted_set());
    }
    let q = Querier::new(bucket.path());

    let query =
        ourios_querier::dsl::parse(r#"resource.k8s.namespace.name == "needle""#).expect("parse");
    let r = q
        .run_query(
            &query,
            &TenantId::new("a"),
            NOW,
            DEFAULT_WINDOW_NS,
            Some(&no_aliases()),
        )
        .await
        .expect("run_query");

    assert_eq!(r.rows, 3, "exactly the needle file's rows match");
    assert!(
        r.stats.row_groups_pruned >= 2,
        "the alpha/beta row groups are pruned by the promoted column's \
         statistics; stats={:?}",
        r.stats,
    );
    let total = r.stats.row_groups_scanned + r.stats.row_groups_pruned;
    assert!(
        total > r.stats.row_groups_pruned,
        "at least one row group was also scanned (matched); stats={:?}",
        r.stats,
    );
    assert!(
        r.stats.bytes_read > 0,
        "bytes_read extraction (from the engine's bytes_scanned metric) \
         stays wired; stats={:?}",
        r.stats,
    );
}

/// Scenario RFC0022.7 — promoted-set drift across deploys (§3.4).
/// See `docs/rfcs/0022-queryable-attribute-columns.md` §5.
///
/// Three files written under configured sets `{}`, `{a}`, `{a,b}` (each on
/// top of the implicit `service.name`), for `a = k8s.namespace.name`
/// (resource) and `b = http.route` (log). One scan spans all three; the
/// schema union is the ordinary §3.9 case and each predicate answers from
/// the typed arm where the column exists and is non-`NULL`, the JSON arm
/// otherwise.
#[tokio::test]
async fn rfc0022_7_promoted_set_drift_unions_cleanly() {
    let bucket = TempDir::new().expect("temp");
    let row = |tid: u64, ts: u64, ns: &str, route: &str| MinedRecord {
        template_id: tid,
        ..rec_with_attrs(
            "a",
            ts,
            vec![kv("service.name", "api"), kv("k8s.namespace.name", ns)],
            vec![kv("http.route", route)],
        )
    };
    // {}: the default set (implicit service.name only) — the plain writer.
    crate::common::write_all(bucket.path(), &[row(601, TS0, "prod", "/cart")]);
    // {a}: k8s.namespace.name promoted, http.route not.
    write_all_with_promoted(
        bucket.path(),
        &[row(602, TS0 + crate::common::HOUR_NS, "dev", "/cart")],
        &PromotedAttributes::new(["k8s.namespace.name".to_string()], []),
    );
    // {a,b}: both promoted.
    write_all_with_promoted(
        bucket.path(),
        &[row(603, TS0 + 2 * crate::common::HOUR_NS, "prod", "/login")],
        &promoted_set(),
    );
    let q = Querier::new(bucket.path());

    // The union scan itself must not error.
    assert_eq!(count(&q, "true").await, 3, "all three files are scanned");

    // `a` answers from every file: typed arm in {a}/{a,b} files, JSON arm in
    // the {} file (its `resource.k8s.namespace.name` cell reads NULL).
    assert_eq!(
        count(&q, r#"resource.k8s.namespace.name == "prod""#).await,
        2
    );
    assert_eq!(
        count(&q, r#"resource.k8s.namespace.name != "prod""#).await,
        1
    );
    // `b` answers from every file: typed arm only in the {a,b} file.
    assert_eq!(count(&q, r#"attr.http.route == "/cart""#).await, 2);
    assert_eq!(count(&q, r#"attr.http.route != "/cart""#).await, 1);
    // Row identity across the drift: each == match is the expected file.
    for (query, tid) in [
        (r#"resource.k8s.namespace.name == "dev""#, 602),
        (r#"attr.http.route == "/login""#, 603),
    ] {
        assert_eq!(
            count(&q, &format!("{query} and template_id == {tid}")).await,
            1,
            "{query} matches exactly template {tid}"
        );
    }
    // Ordering stays typed-arm-only under drift: only the {a,b} file has a
    // non-NULL `attr.http.route` cell, so its /login row is the sole match —
    // the /cart rows in the {}/{a} files read NULL and silently non-match.
    assert_eq!(count(&q, r#"attr.http.route >= "/cart""#).await, 1);
}
