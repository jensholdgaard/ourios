//! RFC0059.6 — a SIGTERM during the template-id bootstrap scan, at the
//! process boundary (#932).
//!
//! Spawns the real `ourios-server` binary, with the receiver authorised to
//! bootstrap, on a one-worker Tokio runtime (`TOKIO_WORKER_THREADS=1`) and
//! with the logs exporter off. The store holds just over 10,000 small data
//! files, which the scan reads first, and then hundreds of data files
//! without statistics, which it must download and decode whole. Once its
//! stderr shows the 10,000-file progress line, the scan is in the slow
//! files; the test sends SIGTERM and asserts that the process exits 0
//! promptly, having written neither the high-water object nor the seated
//! marker, and bound no listener.
#![cfg(unix)]

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::{RecordBatch, UInt64Array};
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_parquet::{PartitionKey, Writer};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

/// Small files, read first: past the 10,000-file progress mark.
const FAST_FILES: usize = 10_050;
/// Statistics-less files, read after them, each decoded whole.
const SLOW_FILES: usize = 2_000;
const SLOW_ROWS: u64 = 500_000;
const PER_DIR: usize = 1_000;

fn record() -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("zz"),
        template_id: 7,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: 1_767_225_600_000_000_000,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: vec![String::new()],
        body: Some("line".to_owned()),
        confidence: 1.0,
        lossy_flag: true,
    }
}

/// `count` hard links to `original` under `prefix`, `PER_DIR` a directory.
fn links(bucket: &Path, original: &Path, prefix: &str, count: usize) {
    for file in 0..count {
        let dir = bucket.join(format!("{prefix}/batch={}", file / PER_DIR));
        if file % PER_DIR == 0 {
            std::fs::create_dir_all(&dir).expect("dir");
        }
        std::fs::hard_link(original, dir.join(format!("f{file}.parquet"))).expect("hard link");
    }
}

/// A `template_id` column with no statistics: the scan downloads and
/// decodes the whole file.
fn write_slow_file(path: &Path) {
    let ids = UInt64Array::from_iter_values(
        (0..SLOW_ROWS).map(|i| i.wrapping_mul(2_654_435_761) % 1_000),
    );
    let batch = RecordBatch::try_from_iter([("template_id", Arc::new(ids) as _)]).expect("batch");
    let props = WriterProperties::builder()
        .set_statistics_enabled(EnabledStatistics::None)
        .build();
    let file = std::fs::File::create(path).expect("create");
    let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(props)).expect("writer");
    writer.write(&batch).expect("write");
    writer.close().expect("close");
}

/// The listing walks the greatest directory name first, depth first: the
/// fast files under `tenant_id=zz` are all handed out before the slow
/// ones under `tenant_id=aa`.
fn store(bucket: &Path) {
    let partition = PartitionKey::derive(&record()).expect("partition");
    let mut writer = Writer::open(bucket, partition).expect("writer");
    writer.append_records(&[record()]).expect("append");
    let fast = writer.close().expect("close").path;
    links(bucket, &fast, "data/tenant_id=zz", FAST_FILES - 1);
    let slow_dir = bucket.join("data/tenant_id=aa");
    std::fs::create_dir_all(&slow_dir).expect("dir");
    let slow = slow_dir.join("slow.parquet");
    write_slow_file(&slow);
    links(bucket, &slow, "data/tenant_id=aa", SLOW_FILES - 1);
}

/// Scenario RFC0059.6 — a SIGTERM mid-scan stops the process cleanly with
/// nothing written, whatever the logs exporter and on one runtime worker.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test]
async fn rfc0059_6_sigterm_mid_scan_exits_cleanly_and_writes_nothing() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let bucket = tmp.path().join("store");
    let wal_root = tmp.path().join("wal");
    store(&bucket);

    let mut child = Command::new(env!("CARGO_BIN_EXE_ourios-server"))
        .env("OURIOS_BUCKET_ROOT", &bucket)
        .env("OURIOS_RECEIVER_ENABLED", "1")
        .env("OURIOS_RECEIVER_GRPC_ADDR", "127.0.0.1:0")
        .env("OURIOS_RECEIVER_HTTP_ADDR", "127.0.0.1:0")
        .env("OURIOS_WAL_ROOT", &wal_root)
        .env("OURIOS_COMPACTION_ENABLED", "false")
        .env("OURIOS_TEMPLATE_IDS_ALLOW_BOOTSTRAP", "true")
        .env("OTEL_LOGS_EXPORTER", "none")
        .env("OTEL_METRICS_EXPORTER", "none")
        .env("OTEL_TRACES_EXPORTER", "none")
        .env("RUST_LOG", "off")
        .env("TOKIO_WORKER_THREADS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ourios-server");

    // RUST_LOG=off silences the tracing copy, so these are the scan's own
    // stderr lines.
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr piped")).lines();
    let mut seen = Vec::new();
    let progressed = timeout(Duration::from_secs(60), async {
        while let Some(line) = stderr.next_line().await.expect("read stderr") {
            let done = line.contains("10000 data and audit footers read");
            seen.push(line);
            if done {
                return true;
            }
        }
        false
    })
    .await;
    assert!(
        matches!(progressed, Ok(true)),
        "no 10,000-file progress line on stderr: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|line| line.starts_with("template-id bootstrap: reading every data")),
        "no start line on stderr: {seen:?}"
    );

    let pid = child.id().expect("server pid");
    let signalled = Instant::now();
    let kill = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await
        .expect("run kill -TERM");
    assert!(kill.success());
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("the server exits promptly after SIGTERM")
        .expect("reap the server");
    let stopped_in = signalled.elapsed();

    let mut rest = String::new();
    stderr
        .into_inner()
        .read_to_string(&mut rest)
        .await
        .expect("drain stderr");
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout piped")
        .read_to_string(&mut stdout)
        .await
        .expect("drain stdout");

    assert!(
        status.success(),
        "a requested stop exits 0, got {status:?}: {rest}"
    );
    assert!(
        rest.contains("interrupted by shutdown"),
        "the scan stopped interrupted, not complete: {rest}"
    );
    assert!(
        stopped_in < Duration::from_secs(5),
        "stopped {stopped_in:?} after SIGTERM"
    );
    assert!(
        !stdout.contains("listening on"),
        "no listener was bound: {stdout}"
    );
    assert!(
        !bucket.join("miner/template_ids.v1.json").exists(),
        "no high-water object is written"
    );
    assert!(
        !wal_root.join("snapshots/TEMPLATE_IDS_SEATED").exists(),
        "nor the seated marker"
    );
}
