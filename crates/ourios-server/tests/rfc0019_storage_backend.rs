//! RFC 0019 — storage-backend selection, the server-level §5 scenarios.
//!
//! The config-resolution scenarios (`.1` backend selection, `.6` config/secret
//! hygiene, `.7` local-backend regression) are unit tests of the private
//! `build_store_config` / `build_config` in `src/main.rs`; see those.
//!
//! The remaining scenarios are server-level and stay `#[ignore]`d until their
//! slice lands green: the WAL staying local under an S3 backend (`.2`), an
//! end-to-end ingest→query on S3 (`.3`), compaction on S3 (`.4`), and tenant
//! isolation on S3 (`.5`).
//!
//! The S3 scenarios (`.3`/`.4`/`.5`) will use a localstack container (the
//! `crates/ourios-parquet/tests/rfc0013_object_store.rs` harness pattern) and
//! stay `#[ignore]`d in the default `cargo test` run even once green. The CI
//! job that exercises these `ourios-server` ignored tests against localstack is
//! added with their green slice (today CI only runs `ourios-parquet`'s ignored
//! localstack tests).
//!
//! See `docs/rfcs/0019-storage-backend-selection.md` §5 / §6.

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
