//! RFC 0013 — object-storage backend acceptance scenarios (§5).
//!
//! **Status: `red`.** These are the failing stubs that drive the `green`
//! implementation: each encodes one RFC0013.§5 scenario and currently
//! `todo!()`s. They are `#[ignore]`d so the default `cargo test` (and CI)
//! stays green while the backend is built — `green` replaces each body with
//! a real assertion and removes the `#[ignore]`. The S3-backed scenarios run
//! against a MinIO/localstack container (`testcontainers`); the local-backend
//! scenarios re-run the RFC 0005 / 0009 contract through `Store`.
//!
//! See `docs/rfcs/0013-object-storage.md` §5/§6.

// Anchor the stubs to the public surface so they fail to compile if the
// `Store` seam is removed out from under them.
use ourios_parquet::{S3Config, Store};

/// Scenario RFC0013.1 — a `MinedRecord` batch written and read through the `AmazonS3`
/// backend recovers byte-for-byte against the local backend.
#[test]
#[ignore = "RFC0013.1 — red until the S3 backend + writer/reader migration land"]
fn rfc0013_1_round_trip_through_s3_backend() {
    let _ = Store::s3;
    todo!("RFC0013.1: S3 round-trip == local round-trip (MinIO testcontainer)")
}

/// Scenario RFC0013.2 — the existing RFC 0005 / 0009 suites pass unchanged against the
/// `LocalFileSystem` backend after the seam refactor.
#[test]
#[ignore = "RFC0013.2 — red until the consumers run through Store"]
fn rfc0013_2_local_backend_regresses_nothing() {
    todo!("RFC0013.2: RFC0005/0009 suites green via the LocalFileSystem Store")
}

/// Scenario RFC0013.3 — two `compact_partition` runs racing on one partition: exactly
/// one manifest generation wins; no torn / doubled / missing rows.
#[test]
#[ignore = "RFC0013.3 — red until conditional-PUT atomic publish lands"]
fn rfc0013_3_atomic_publish_under_contention() {
    todo!("RFC0013.3: exactly-one-wins under concurrent publishers")
}

/// Scenario RFC0013.4 — generation publish uses conditional PUT (`PutMode::Create` /
/// `Update{ETag}`) with no `rename` dependency.
#[test]
#[ignore = "RFC0013.4 — red until the manifest swap uses conditional PUT"]
fn rfc0013_4_manifest_swap_via_conditional_put() {
    todo!("RFC0013.4: publish path uses PutMode::Create/Update, never rename")
}

/// Scenario RFC0013.5 — operations in tenant X's context address only X's key
/// sub-prefix; no read/write touches tenant Y's keys (`CLAUDE.md` §3.7).
#[test]
#[ignore = "RFC0013.5 — red until tenant key-prefix scoping is wired"]
fn rfc0013_5_tenant_isolation_across_prefix() {
    todo!("RFC0013.5: no cross-tenant key access")
}

/// Scenario RFC0013.6 — with an object-storage backend, only data/audit/manifest
/// objects reach the store; the WAL stays on local disk (`CLAUDE.md` §3.4).
#[test]
#[ignore = "RFC0013.6 — red until the server wires the object-store backend"]
fn rfc0013_6_wal_stays_local() {
    let _ = S3Config::default();
    todo!("RFC0013.6: WAL frames local; only Parquet/manifest in the store")
}

/// Scenario RFC0013.7 — an S3-compatible store (`MinIO`) configured via an endpoint
/// override (RFC 0004) reads and writes exactly as AWS S3.
#[test]
#[ignore = "RFC0013.7 — red until the S3 backend honours endpoint overrides"]
fn rfc0013_7_s3_compatible_endpoint_via_override() {
    todo!("RFC0013.7: MinIO endpoint override works like AWS S3")
}

/// Scenario RFC0013.8 — the RFC 0005 §3.9 reader forward-compat contract (absent
/// columns default, unknown columns ignored) holds over the object store.
#[test]
#[ignore = "RFC0013.8 — red until the reader resolves objects through Store"]
fn rfc0013_8_reader_forward_compat_over_store() {
    todo!("RFC0013.8: RFC0005 §3.9 holds reading through the object store")
}
