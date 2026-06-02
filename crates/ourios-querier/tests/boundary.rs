//! RFC0007.3 — the §4.6 no-leakage boundary, exercised against a
//! *real* `DataFusion` error rather than a synthetic one. A
//! corrupt committed `*.parquet` makes schema inference fail deep
//! inside the engine; the surfaced [`QueryError`] must carry none
//! of that engine/arrow/SQL text in its operator-facing `Display`.
//! (The colocated unit test pins the same contract at the string
//! level for a synthetic error.)

use ourios_core::tenant::TenantId;
use ourios_querier::{Querier, QueryError, QueryRequest};

/// Engine/SQL substrings that must never reach an operator-facing
/// message (kept in step with the colocated `ENGINE_LEAK_TOKENS`).
const ENGINE_LEAK_TOKENS: &[&str] = &[
    "datafusion",
    "arrow",
    "parquet",
    "sql",
    "select",
    "schema",
    "logical plan",
    "logicalplan",
    "physical",
    "recordbatch",
    "listingtable",
    "during planning",
];

/// A committed (non-`.tmp`) `*.parquet` whose bytes are not valid
/// Parquet trips schema inference inside `DataFusion`. The error
/// must surface as [`QueryError::Storage`] with a generic
/// `Display` — no engine/arrow/SQL token leaks (RFC0007.3 / §4.6)
/// — while `Debug` keeps the detail for logs.
#[tokio::test]
async fn rfc0007_3_real_engine_error_does_not_leak() {
    let bucket = tempfile::TempDir::new().expect("temp");
    // A valid tenant partition path with a *committed* but corrupt
    // parquet file, so `has_published_parquet` passes and the
    // engine is actually invoked.
    let part = bucket
        .path()
        .join("data/tenant_id=a/year=2026/month=04/day=02/hour=10");
    std::fs::create_dir_all(&part).expect("mkdir partition");
    std::fs::write(part.join("corrupt.parquet"), b"not a parquet file at all")
        .expect("write corrupt parquet");

    let q = Querier::new(bucket.path());
    let err = q
        .run(QueryRequest {
            tenant: TenantId::new("a"),
            time_range: None,
            template_id: Some(1),
        })
        .await
        .expect_err("a corrupt parquet must surface as an error");

    assert!(
        matches!(err, QueryError::Storage { .. }),
        "a real engine read failure is a Storage error, got {err:?}",
    );

    let shown = err.to_string().to_ascii_lowercase();
    for token in ENGINE_LEAK_TOKENS {
        assert!(
            !shown.contains(token),
            "operator-facing Display leaked engine token {token:?}: {shown:?}",
        );
    }
    assert_eq!(err.to_string(), "failed to read the log store");

    // The underlying engine detail is preserved for logs (Debug),
    // proving the scrub is a deliberate Display boundary.
    let dbg = format!("{err:?}");
    assert!(
        dbg.len() > "Storage".len(),
        "Debug should preserve the engine detail for logs: {dbg}",
    );
}
