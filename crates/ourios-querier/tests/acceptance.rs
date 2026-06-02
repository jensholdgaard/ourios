//! RFC 0007 §5 acceptance criteria — red gate.
//!
//! Each test enumerates one §5 scenario the querier must satisfy
//! before RFC 0007 moves `specified → red → green`. They are
//! `#[ignore]`'d `unimplemented!()` stubs until the execution
//! slice lands (which itself waits on RFC 0002's DSL branch, per
//! the crate docs). The scenario id is in each panic message so
//! the test ↔ criterion mapping is greppable (RFC 0007 §5 /
//! `docs/verification.md` §2).

// RFC0007.1 (B1 pushdown prunes row groups) is now a LIVE test —
// see `tests/execution.rs::rfc0007_1_pushdown_prunes_row_groups`.

// RFC0007.2 (B2 — template-exact work tracks result size, not
// corpus size) is now a LIVE test — see
// `tests/execution.rs::rfc0007_2_template_exact_work_scales_with_result_not_corpus`.
// It asserts the claim structurally (row groups scanned + bytes
// read are flat across an ~8× larger corpus, the growth absorbed
// by pruning), which is deterministic where a wall-clock latency
// bench would be flaky. A criterion latency bench across the
// otel-demo corpora is supportive evidence, tracked separately.

// RFC0007.3 (no DataFusion/arrow/SQL leakage — §4.6) is now LIVE:
//   - string level: `lib.rs` unit test
//     `rfc0007_3_storage_display_leaks_no_engine_tokens` (a
//     `Storage` error scrubs synthetic engine/SQL text from
//     `Display` while `Debug` preserves it);
//   - real engine error: `tests/boundary.rs::
//     rfc0007_3_real_engine_error_does_not_leak` (a corrupt
//     `*.parquet` trips `DataFusion` schema inference; the
//     surfaced message leaks none of the engine token denylist).
// The "no engine type in a public *signature*" half is enforced
// structurally: `QueryRequest`/`QueryResult`/`QueryStats`/
// `QueryError` are all Ourios-owned (see crate docs / Cargo.toml
// note), so no `datafusion`/`arrow` type crosses the API.

#[ignore = "RFC 0007 red gate — execution pending (RFC0007.4)"]
#[test]
fn rfc0007_4_forward_compatible_reads() {
    // §3.5: Parquet files with unknown columns (future schema)
    // or missing optional columns (old schema) query without
    // error, honouring RFC 0005 §3.9 reader-contract defaults.
    unimplemented!("RFC0007.4 — assert unknown/missing columns honour RFC 0005 §3.9 defaults");
}

// RFC0007.5 (tenant isolation) is now a LIVE test — see
// `tests/execution.rs::rfc0007_5_tenant_isolation`.
