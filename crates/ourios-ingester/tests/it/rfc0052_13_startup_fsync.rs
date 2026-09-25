//! RFC0052.13 — the startup leg: a snapshot governs reclamation only once
//! its directory entry is durable in this process.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Placement note: the `RetainFloor` cases and the churn leg are WAL
//! ledger behaviour and live in
//! `ourios-wal/tests/it/rfc0052_13_floor_pinning.rs`; this leg is the
//! ingester's snapshot listing at startup. The fsync is made to fail
//! with the file-in-place-of-directory technique the rotation tests
//! use (RFC 0052 §6), since read-only permissions do not bind under
//! root.

use ourios_ingester::barrier::fsync_snapshots_root;
use ourios_ingester::snapshot_store;

/// Scenario RFC0052.13 — a failed startup fsync of the snapshots root fails startup.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
fn rfc0052_13_failed_snapshots_root_fsync_fails_startup_rather_than_pinning() {
    // Given a snapshots-root fixture whose directory fsync cannot
    // succeed, because the path is a *file*: opening it succeeds and the
    // listing below would too, so nothing but the fsync distinguishes
    // this from a healthy root.
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path().join("snapshots");
    std::fs::write(&root, b"not a directory").expect("a file where the root belongs");

    // When startup fsyncs the snapshots root.
    let failed = fsync_snapshots_root(&root);

    // Then startup fails rather than discarding the snapshots: a horizon
    // whose directory entry may not be durable must never govern
    // reclamation, and throwing the artefacts away would lose state
    // nothing else can rebuild.
    let error = failed.expect_err("a root that cannot be fsynced fails startup");
    assert!(
        matches!(error, snapshot_store::SnapshotStoreError::Io { .. }),
        "the failure names the I/O step rather than reading as an empty store: {error}",
    );
    assert!(
        std::fs::read(&root).is_ok(),
        "and nothing was discarded on the way out",
    );

    // And with the fsync succeeding, the listed snapshots are used as
    // horizons: the same call over a real directory returns, and the
    // artefacts in it are listed.
    let healthy = tmp.path().join("healthy");
    std::fs::create_dir_all(&healthy).expect("healthy root");
    let tenant = ourios_core::tenant::TenantId::new("checkout");
    snapshot_store::write(
        &healthy,
        &tenant,
        &ourios_miner::cluster::MinerCluster::new(ourios_config::MinerConfig::default())
            .snapshot_state(&tenant),
    )
    .expect("write a snapshot");
    fsync_snapshots_root(&healthy).expect("a healthy root fsyncs");
    let listed = snapshot_store::load_all_durable(&healthy).expect("list");
    assert_eq!(
        listed.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
        vec![tenant],
        "the listed snapshot is available as a horizon",
    );

    // An absent root is a cold start, not a failure — the one case that
    // must not be read as "the fsync failed".
    fsync_snapshots_root(&tmp.path().join("never-created")).expect("a cold start is not a failure");
}

/// A `*.snap.tmp` left by a process that died mid-write is removed
/// before the listing, so a failed barrier cannot leak one snapshot per
/// attempt (RFC 0052 §3.1).
#[test]
fn rfc0052_13_a_stranded_snapshot_temp_is_swept_before_the_listing() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path().join("snapshots");
    std::fs::create_dir_all(&root).expect("root");
    let stranded = root.join("checkout.42.snap.tmp");
    std::fs::write(&stranded, b"a previous process's half-written snapshot").expect("temp");

    let listed = snapshot_store::load_all_durable(&root).expect("list");

    assert!(listed.is_empty(), "a temp file is not an artefact");
    assert!(!stranded.exists(), "and it does not survive the listing");
}

/// The listing owns the fsync, so a caller that never ran the startup
/// preflight still cannot read an artefact as a horizon: RFC0052.13's
/// guarantee is about every artefact `load_all_durable` returns, not
/// about one call site remembering a separate step.
#[test]
fn a_root_that_cannot_be_fsynced_is_not_listed_even_without_the_preflight() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path().join("snapshots");
    std::fs::write(&root, b"not a directory").expect("a file where the root belongs");

    let error = snapshot_store::load_all_durable(&root)
        .expect_err("a root that cannot be fsynced is not listed");

    // The *step* matters, not just the failure: a listing over a file
    // fails on its own, so only naming the fsync proves the listing ran
    // it rather than tripping over the fixture.
    assert!(
        matches!(
            &error,
            snapshot_store::SnapshotStoreError::Io { op, .. } if *op == "fsync(snapshots root)"
        ),
        "the listing fsynced the root itself: {error}",
    );
}

/// The sweep owns `*.snap.tmp` and nothing else, and a failed unlink is
/// raised rather than leaving a stranded temp behind a successful list.
#[test]
fn the_sweep_takes_only_its_own_temps_and_raises_a_failed_unlink() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path().join("snapshots");
    std::fs::create_dir_all(&root).expect("root");
    let foreign = root.join("someone-elses.tmp");
    std::fs::write(&foreign, b"not ours").expect("foreign temp");

    snapshot_store::load_all_durable(&root).expect("list");
    assert!(
        foreign.exists(),
        "a foreign temp is not the sweep's to take"
    );

    // A directory under the store's own suffix: `remove_file` cannot take
    // it, which is the unlink failure the contract promises to raise.
    std::fs::create_dir(root.join("checkout.7.snap.tmp")).expect("undeletable temp");
    let error = snapshot_store::load_all_durable(&root)
        .expect_err("a temp the sweep cannot remove fails the listing");
    assert!(
        matches!(
            &error,
            snapshot_store::SnapshotStoreError::Io { op, .. }
                if *op == "remove_file(stranded snapshot temp)"
        ),
        "the failure names the unlink rather than reading as an empty store: {error}",
    );
}
