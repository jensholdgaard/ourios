//! RFC 0019 — storage-backend selection, the §5 acceptance scenarios (red).
//!
//! These stubs establish the red baseline: each scenario is `#[ignore]`d and
//! `todo!()` until its slice lands green. They cover selecting the backend from
//! config (`.1`), the WAL staying local under S3 (`.2`), an end-to-end
//! ingest→query on an S3-compatible backend (`.3`), compaction on S3 (`.4`),
//! tenant isolation on S3 (`.5`), config/secret hygiene (`.6`), and the
//! local-backend regression (`.7`).
//!
//! The S3 scenarios (`.3`/`.4`/`.5`) will use a localstack container (the
//! `crates/ourios-parquet/tests/rfc0013_object_store.rs` harness pattern) and
//! stay `#[ignore]`d in the default `cargo test` run even once green. The CI
//! job that exercises these `ourios-server` ignored tests against localstack is
//! added with their green slice (today CI only runs `ourios-parquet`'s ignored
//! localstack tests).
//!
//! See `docs/rfcs/0019-storage-backend-selection.md` §5 / §6.

/// Scenario RFC0019.1 — backend selection from config.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
#[test]
#[ignore = "RFC0019.1 — red until StoreConfig resolves local/s3 from env (green)"]
fn rfc0019_1_backend_selection_from_config() {
    todo!(
        "RFC0019.1: unset→local from OURIOS_BUCKET_ROOT; s3+bucket→s3; s3 sans bucket / unknown→fail-fast"
    )
}

/// Scenario RFC0019.2 — the WAL stays local under every backend.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
#[test]
#[ignore = "RFC0019.2 — red until the WAL is local while data is on S3 (green)"]
fn rfc0019_2_wal_stays_local_under_s3() {
    todo!(
        "RFC0019.2: OURIOS_STORAGE_BACKEND=s3 → WAL under local OURIOS_WAL_ROOT, never an object key"
    )
}

/// Scenario RFC0019.3 — end-to-end ingest→query on S3.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
#[test]
#[ignore = "RFC0019.3 — red until ingest→query round-trips on an S3 backend (localstack/CI)"]
fn rfc0019_3_ingest_query_end_to_end_on_s3() {
    todo!(
        "RFC0019.3: localstack — ingest a batch lands Parquet under the S3 prefix; query returns the rows"
    )
}

/// Scenario RFC0019.4 — compaction operates on S3.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
#[test]
#[ignore = "RFC0019.4 — red until a compaction sweep consolidates on S3 via publish_cas (localstack/CI)"]
fn rfc0019_4_compaction_operates_on_s3() {
    todo!(
        "RFC0019.4: localstack — small files consolidated via Store; manifest swapped with conditional PUT"
    )
}

/// Scenario RFC0019.5 — tenant isolation on S3.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
#[test]
#[ignore = "RFC0019.5 — red until a query reads only its tenant's prefix on S3 (localstack/CI)"]
fn rfc0019_5_tenant_isolation_on_s3() {
    todo!("RFC0019.5: localstack — one tenant's query never returns another tenant's objects")
}

/// Scenario RFC0019.6 — config is governed by RFC 0004; no secret leakage.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
#[test]
#[ignore = "RFC0019.6 — red until secret hygiene is asserted on the config path (green)"]
fn rfc0019_6_config_governed_no_secret_leakage() {
    todo!(
        "RFC0019.6: no credential value in logs/errors/metrics; missing-S3-config names only the key"
    )
}

/// Scenario RFC0019.7 — local backend regression.
/// See `docs/rfcs/0019-storage-backend-selection.md` §5.
#[test]
#[ignore = "RFC0019.7 — red until the default local path is proven byte-for-byte unchanged (green)"]
fn rfc0019_7_local_backend_regression() {
    todo!(
        "RFC0019.7: default (no backend env, OURIOS_BUCKET_ROOT set) — receiver/querier/compactor identical"
    )
}
