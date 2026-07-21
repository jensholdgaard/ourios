//! Issue #578 crash arm (the publish half of the RFC0035.2 barrier): a
//! process `SIGKILL`ed while the age sweep's off-lock publish is in flight —
//! the acked records drained **out** of the flush buffers, durable nowhere
//! but the WAL — replays them on recovery.
//!
//! Extends the RFC0014.5 harness: `receiver_sink_crash_fixture sweep` parks
//! inside the exact window (post-drain, pre-`write_ordered`), the parent
//! kills it, and the recovery driver must re-mine both records into a fresh
//! sink. This arm proves the window's on-disk state is replayable; the
//! companion arm in `ourios-server`
//! (`rfc0035_2_rotation_stamp_waits_for_the_sweeps_in_flight_publish`)
//! proves no `wal_high_water` stamp can land *during* the window to
//! foreclose that replay. Together: no acked-data loss (`CLAUDE.md` §3.4).

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use ourios_config::MinerConfig;
use ourios_core::record::MinedRecord;
use ourios_ingester::receiver::TenantRule;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{Reader, Store};
use ourios_wal::Wal;

use crate::ingest_support::wal_config;

/// Every mined row across every `*.parquet` object under `root`.
fn all_rows(root: &Path) -> Vec<MinedRecord> {
    let mut rows = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                rows.extend(
                    Reader::open_file(&path)
                        .expect("open_file")
                        .read_all()
                        .expect("read_all"),
                );
            }
        }
    }
    rows
}

#[test]
fn rfc0035_2_crash_during_the_sweeps_in_flight_publish_replays_the_records() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");
    let bucket_root = tmp.path().join("store");
    std::fs::create_dir_all(&bucket_root).expect("create store root");

    // Spawn the fixture in `sweep` mode: it ingests + fsyncs one batch, runs
    // the age sweep's atomic drain (records leave the buffers), and parks
    // with the off-lock write never started — the #578 window.
    let mut child = Command::new(env!("CARGO_BIN_EXE_receiver_sink_crash_fixture"))
        .arg(&wal_root)
        .arg(&bucket_root)
        .arg("sweep")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sweep crash fixture");
    let stdout = child.stdout.take().expect("fixture stdout piped");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read fixture READY");
    assert_eq!(
        line.trim(),
        "READY",
        "fixture must signal READY (got {line:?}) — a different first line \
         means it failed before reaching the post-drain READY print",
    );

    // Mid-window: the drained records are durable nowhere but the WAL.
    assert!(
        all_rows(&bucket_root).is_empty(),
        "nothing reached the store — the sweep's publish is still in flight",
    );

    // Crash: SIGKILL discards the drained in-memory snapshot mid-publish.
    child.kill().expect("SIGKILL fixture");
    child.wait().expect("reap fixture");

    // Recover into a fresh miner + sink. No snapshot was stamped during the
    // window (the fix's stamp-waits arm pins that), so replay covers the
    // acked frames and re-mines the drained records.
    let mut wal = Wal::open(wal_config(&wal_root)).expect("reopen WAL");
    let store = Store::local(&bucket_root).expect("store");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(
        store,
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: std::time::Duration::from_secs(86_400),
            ceiling_bytes: usize::MAX,
        },
    ));
    let mut miner =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let snapshots_root = wal_root.join("snapshots"); // none written → full replay
    let report = recovery::recover(
        &mut wal,
        &snapshots_root,
        &mut miner,
        &TenantRule::service_name(),
    )
    .expect("startup recovery");
    assert_eq!(
        report.records_fed_to_miner, 2,
        "replay re-mined both records the crash caught mid-publish"
    );
    sink.flush_all();

    let rows = all_rows(&bucket_root);
    assert_eq!(
        rows.len(),
        2,
        "no acked record was lost in the sweep-publish window (issue #578)",
    );
    assert!(
        rows.iter().all(|r| r.tenant_id.as_str() == "checkout"),
        "the recovered rows are the fixture's checkout batch, got {:?}",
        rows.iter()
            .map(|r| r.tenant_id.as_str())
            .collect::<Vec<_>>(),
    );
}
