//! RFC0045.10 — The WAL tail keeps its rule epoch. See
//! `docs/rfcs/0045-composite-tenant-derivation.md` §3.3 / §5.
//!
//! Reuses the RFC0014.5 crash fixture: it acknowledges a `service.name:
//! checkout` batch (no `k8s.cluster.name`) under the default rule and is
//! `SIGKILL`ed before any flush, so the frames survive only in the WAL. The
//! "restart" then runs recovery with the composite rule configured. Under
//! RFC 0045 §3.3 the frames derive under the rule they were acknowledged
//! under: startup succeeds although the frames lack the new key, the rows
//! land only in `checkout`, no composite tenant appears, and the epoch log
//! gains one entry — which a second restart honours.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use ourios_config::MinerConfig;
use ourios_core::record::MinedRecord;
use ourios_ingester::receiver::TenantRule;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_ingester::recovery;
use ourios_ingester::rule_epochs::{FILE_NAME, RuleEpochs};
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

/// One "restart": recover the WAL into a fresh miner + sink under the
/// configured `rule`, flush, and return the store's rows.
fn restart_with(wal_root: &Path, bucket_root: &Path, rule: &TenantRule) -> Vec<MinedRecord> {
    let mut wal = Wal::open(wal_config(wal_root)).expect("reopen WAL");
    let store = Store::local(bucket_root).expect("store");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(store, never_flush()));
    let mut miner =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let mut epochs = RuleEpochs::load(wal_root).expect("epoch log loads");
    let report = recovery::recover(&mut wal, &wal_root.join("snapshots"), &mut miner, &epochs)
        .expect("startup recovery succeeds under the acknowledged-under rule");
    epochs
        .advance(rule, report.max_delivered)
        .expect("epoch log persists");
    sink.flush_all();
    all_rows(bucket_root)
}

/// Scenario RFC0045.10 — WAL tail keeps its epoch.
/// See `docs/rfcs/0045-composite-tenant-derivation.md` §5.
#[test]
fn rfc0045_10_wal_tail_keeps_its_epoch() {
    // Arrange
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");
    let bucket_root = tmp.path().join("store");
    std::fs::create_dir_all(&bucket_root).expect("create store root");
    let composite = TenantRule::from_keys(["k8s.cluster.name", "service.name"]).expect("rule");

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
        "fixture must signal READY (got {line:?})"
    );
    child.kill().expect("SIGKILL fixture");
    child.wait().expect("reap fixture");
    assert!(
        !wal_root.join(FILE_NAME).exists(),
        "a pre-RFC WAL has no epoch log — the implicit [service.name] epoch"
    );

    // Act: restart under the composite rule.
    let rows = restart_with(&wal_root, &bucket_root, &composite);

    // Assert: the acknowledged frames derived under [service.name] — only
    // `checkout`, nothing composite, nothing lost, nothing duplicated.
    let tenants: Vec<&str> = rows.iter().map(|r| r.tenant_id.as_str()).collect();
    assert_eq!(
        rows.len(),
        2,
        "both acknowledged records recovered: {tenants:?}"
    );
    assert!(
        tenants.iter().all(|t| *t == "checkout"),
        "old-epoch frames stay in their original tenant, got {tenants:?}"
    );
    let epochs = RuleEpochs::load(&wal_root).expect("epoch log");
    assert_eq!(epochs.epochs().len(), 2, "one epoch appended");
    assert_eq!(epochs.current(), &composite);
    assert!(
        epochs.epochs()[1].after.is_some(),
        "the boundary is the highest replayed offset"
    );

    // Act again: a second restart under the same composite rule replays the
    // same frames — the persisted epoch log keeps them under [service.name].
    let bucket_root_2 = tmp.path().join("store2");
    std::fs::create_dir_all(&bucket_root_2).expect("create store root");
    let rows = restart_with(&wal_root, &bucket_root_2, &composite);
    assert!(
        rows.iter().all(|r| r.tenant_id.as_str() == "checkout"),
        "the persisted epoch log keeps old frames in their epoch"
    );
    assert_eq!(
        RuleEpochs::load(&wal_root)
            .expect("epoch log")
            .epochs()
            .len(),
        2,
        "an unchanged rule appends nothing"
    );
}
