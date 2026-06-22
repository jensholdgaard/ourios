//! RFC 0016 — query-serving endpoint (HTTP query API over the logs DSL), the
//! §5 acceptance scenarios.
//!
//! **Status: `red`.** Failing stubs driving the `green` implementation: each
//! encodes a RFC 0016 §5 scenario and currently `todo!()`s. They are
//! `#[ignore]`d so the default `cargo test` (and CI) stays green until the
//! querier role lands; `green` replaces each body with the real assertions and
//! removes the `#[ignore]`.
//!
//! See `docs/rfcs/0016-query-serving-endpoint.md` §5 / §6.

/// Scenario RFC0016.1 — the querier role serves a DSL query end-to-end: a
/// `POST /v1/query` (with `X-Ourios-Tenant`) against a populated store parses
/// via the RFC 0002 front-end, runs through `Querier::run_query`, and returns
/// `200` with the matching rows + pruning stats.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[test]
#[ignore = "RFC0016.1 — red until the querier role serves a DSL query (green)"]
fn rfc0016_1_querier_role_serves_a_dsl_query_end_to_end() {
    todo!("RFC0016.1: POST /v1/query → run_query → 200 with rows + pruning stats")
}

/// Scenario RFC0016.2 — tenant scoping is enforced at the API: `X-Ourios-Tenant`
/// scopes reads to that tenant only, and a missing/empty header is `400` from
/// the server's header check, before any data is scanned.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[test]
#[ignore = "RFC0016.2 — red until the tenant header is enforced at the API (green)"]
fn rfc0016_2_tenant_scoping_is_enforced_at_the_api() {
    todo!("RFC0016.2: X-Ourios-Tenant scopes reads; missing header → 400 pre-scan")
}

/// Scenario RFC0016.3 — a drift query routes to the drift path: a
/// `drift from <t1> to <t2>` statement dispatches the `Drift` arm to
/// `run_drift` and returns the RFC 0010 `DriftResult` shape.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[test]
#[ignore = "RFC0016.3 — red until drift statements route to run_drift (green)"]
fn rfc0016_3_a_drift_query_routes_to_the_drift_path() {
    todo!("RFC0016.3: drift statement → run_drift → DriftResult shape")
}

/// Scenario RFC0016.4 — malformed DSL is a clean `400` with an Ourios-owned
/// error body, and **no** `DataFusion` type / SQL string / plan text appears in
/// the response (H6).
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[test]
#[ignore = "RFC0016.4 — red until malformed DSL → 400 with no engine leak (green)"]
fn rfc0016_4_malformed_dsl_is_a_clean_400_no_engine_leak() {
    todo!("RFC0016.4: invalid/uncompilable DSL → 400; body has no DataFusion/SQL/plan text")
}

/// Scenario RFC0016.5 — role gating + graceful shutdown: with
/// `OURIOS_QUERIER_ENABLED` unset no listener binds; enabled then sent
/// SIGINT/SIGTERM, the querier listener drains and the process exits cleanly.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[test]
#[ignore = "RFC0016.5 — red until the querier role is env-gated + drains on shutdown (green)"]
fn rfc0016_5_role_gating_and_graceful_shutdown() {
    todo!("RFC0016.5: unset → no listener; enabled → binds, drains on shutdown")
}

/// Scenario RFC0016.6 — pruning is observable: a selective query returns a
/// non-zero `row_groups_pruned`, and a query-latency + pruning-ratio metric is
/// emitted via the OpenTelemetry meter surface.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[test]
#[ignore = "RFC0016.6 — red until pruning stats + OTel query metrics are emitted (green)"]
fn rfc0016_6_pruning_is_observable() {
    todo!("RFC0016.6: selective query → row_groups_pruned > 0 + latency/pruning-ratio metric")
}

/// Scenario RFC0016.7 — receiver and querier compose in one binary: with both
/// roles enabled on distinct addresses, both listeners bind and serve over the
/// one `OURIOS_BUCKET_ROOT`, and shutdown drains both.
/// See `docs/rfcs/0016-query-serving-endpoint.md` §5.
#[test]
#[ignore = "RFC0016.7 — red until receiver + querier compose in one binary (green)"]
fn rfc0016_7_receiver_and_querier_compose_in_one_binary() {
    todo!("RFC0016.7: both roles enabled → both bind over one bucket root; shutdown drains both")
}
