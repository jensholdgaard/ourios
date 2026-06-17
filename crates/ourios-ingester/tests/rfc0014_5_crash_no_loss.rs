//! RFC0014.5 — No acknowledged-data loss across a crash with a non-empty
//! flush buffer. See `docs/rfcs/0014-ingest-write-path.md` §5.
//!
//! A real-process crash that extends the RFC 0008 harness (`CLAUDE.md` §6.2):
//! `receiver_sink_crash_fixture` ingests one batch (append + fsync = acked)
//! through the full pipeline — miner → [`ParquetRecordSink`] — with a
//! never-flush config, so the records sit in the in-memory buffer, durable
//! only in the WAL. This test confirms the store is empty (nothing flushed),
//! `SIGKILL`s the fixture (discarding the volatile buffer), then runs the
//! recovery driver into a fresh sink: replay re-mines every acknowledged
//! record through `miner.ingest`, which re-emits it into the new buffer, and a
//! flush lands them all in the store. No acknowledged record is lost.
//!
//! Like RFC0003.2, there is no dedup assertion — the contract is *no loss*
//! (at-least-once); a record flushed just before a crash may re-flush on
//! recovery, which the OTLP duplicate-data tradeoff accepts.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use ourios_core::config::MinerConfig;
use ourios_core::record::MinedRecord;
use ourios_ingester::receiver::TenantRule;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery;
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::{Reader, Store};
use ourios_wal::{Wal, WalConfig};

fn wal_config(root: &Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

fn never_flush() -> FlushConfig {
    FlushConfig {
        target_bytes: usize::MAX,
        max_buffer_age: Duration::from_secs(86_400),
        ceiling_bytes: usize::MAX,
    }
}

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

/// Scenario RFC0014.5 — No acknowledged-data loss: a crash with a non-empty
/// buffer loses nothing — WAL replay re-mines every un-flushed acknowledged
/// record (`CLAUDE.md` §3.4).
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
fn rfc0014_5_no_acknowledged_data_loss() {
    // Arrange: disjoint WAL and store roots; the store must exist before the
    // fixture opens a `Store` on it.
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");
    let bucket_root = tmp.path().join("store");
    std::fs::create_dir_all(&bucket_root).expect("create store root");

    // Act: spawn the fixture, wait until it has ingested + fsync'd the batch
    // into the (un-flushed) buffer (READY).
    let mut child = Command::new(env!("CARGO_BIN_EXE_receiver_sink_crash_fixture"))
        .arg(&wal_root)
        .arg(&bucket_root)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sink crash fixture");
    let stdout = child.stdout.take().expect("fixture stdout piped");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read fixture READY");
    assert_eq!(
        line.trim(),
        "READY",
        "fixture must signal READY (got {line:?}) — a different first line \
         means it failed before reaching the post-fsync READY print",
    );

    // The acked records are durable in the WAL but only buffered in memory:
    // nothing has reached the store yet.
    assert!(
        all_rows(&bucket_root).is_empty(),
        "no record is flushed before the crash — the buffer is the only \
         non-WAL copy",
    );

    // Crash: SIGKILL discards the volatile buffer. Only the WAL survives.
    child.kill().expect("SIGKILL fixture");
    child.wait().expect("reap fixture");

    // Recover: replay the WAL into a fresh miner wired to a fresh sink. Replay
    // re-mines each acknowledged record through `miner.ingest`, which re-emits
    // it into the new buffer; a flush then lands them in the store.
    let mut wal = Wal::open(wal_config(&wal_root)).expect("reopen WAL");
    let store = Store::local(&bucket_root).expect("store");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(store, never_flush()));
    let mut miner =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let snapshots_root = wal_root.join("snapshots"); // the fixture wrote none → full replay
    let report = recovery::recover(
        &mut wal,
        &snapshots_root,
        &mut miner,
        &TenantRule::service_name(),
    )
    .expect("startup recovery");
    assert_eq!(
        report.records_fed_to_miner, 2,
        "replay re-mined both records"
    );
    sink.flush_all();

    // Assert: no loss — every acknowledged record is now durable in the store.
    let rows = all_rows(&bucket_root);
    assert_eq!(
        rows.len(),
        2,
        "WAL replay re-mined and flushed every acknowledged record",
    );
    assert!(
        rows.iter().all(|r| r.tenant_id.as_str() == "checkout"),
        "the recovered rows are the fixture's checkout batch, got {:?}",
        rows.iter()
            .map(|r| r.tenant_id.as_str())
            .collect::<Vec<_>>(),
    );
}
