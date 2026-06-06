//! RFC0003.2 — Crash-before-ack: at-least-once with retry tolerance `[§3.4]`.
//!
//! A real-process crash: `receiver_crash_fixture` ingests one batch
//! (append + fsync) over a real `Wal`, prints `READY`, and parks; this
//! test `SIGKILL`s it after `READY` — i.e. after the batch is durable but
//! before any transport ack would be sent — then reopens the WAL and
//! replays. The fsync'd `OtlpBatch` frame must survive and recover the
//! input.
//!
//! This is the *no-loss* half of the at-least-once contract: a crash in
//! the sync→ack window does not lose acknowledged-or-about-to-be-acked
//! data. There is deliberately **no** dedup assertion — a client that
//! never saw the ack will retry, producing a duplicate, which the OTLP
//! spec's *duplicate-data* section accepts as the right tradeoff.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use opentelemetry_proto::tonic::common::v1::any_value::Value;
use ourios_ingester::receiver::decode_protobuf;
use ourios_wal::{FrameKind, FrameSink, RecoveryError, Wal, WalConfig};

fn wal_config(root: &std::path::Path) -> WalConfig {
    WalConfig {
        root: root.to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    }
}

#[derive(Default)]
struct CollectingSink(Vec<(FrameKind, Vec<u8>)>);
impl FrameSink for CollectingSink {
    fn consume(&mut self, kind: FrameKind, payload: &[u8]) -> Result<(), RecoveryError> {
        self.0.push((kind, payload.to_vec()));
        Ok(())
    }
}

/// Scenario RFC0003.2 — Crash-before-ack: at-least-once with retry tolerance.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[test]
fn rfc0003_2_fsynced_batch_survives_a_crash_before_ack() {
    // Arrange: a real WAL root the fixture and this test both open.
    let tmp = tempfile::TempDir::new().expect("temp");

    // Act: spawn the fixture, wait until it has ingested + fsync'd
    // (READY), then SIGKILL it before it could ack.
    let mut child = Command::new(env!("CARGO_BIN_EXE_receiver_crash_fixture"))
        .arg(tmp.path())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn crash fixture");
    let stdout = child.stdout.take().expect("fixture stdout piped");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read fixture READY");
    assert_eq!(
        line.trim(),
        "READY",
        "fixture signalled READY (got {line:?}) — it died before fsync",
    );
    child.kill().expect("SIGKILL fixture");
    child.wait().expect("reap fixture");

    // Assert: the fsync'd OtlpBatch frame survived the crash and recovers
    // the input batch's record.
    let mut sink = CollectingSink::default();
    Wal::open(wal_config(tmp.path()))
        .expect("reopen WAL after crash")
        .replay(&mut sink)
        .expect("replay after crash");
    assert_eq!(
        sink.0.len(),
        1,
        "exactly one fsync'd OtlpBatch frame survives"
    );
    assert_eq!(sink.0[0].0, FrameKind::OtlpBatch);
    let recovered = decode_protobuf(&sink.0[0].1).expect("frame payload decodes");
    let body = recovered.resource_logs[0].scope_logs[0].log_records[0]
        .body
        .as_ref()
        .and_then(|b| b.value.as_ref());
    assert!(
        matches!(body, Some(Value::StringValue(s)) if s == "user 1 logged in"),
        "the durable frame recovers the input record's body, got {body:?}",
    );
}
