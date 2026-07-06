//! RFC0008.2 — Crash-recovery completeness `[§3.4 / H3]`.
//! See `docs/rfcs/0008-wal.md` §5.
//!
//! The H3 normative requirement that runs on **every PR**
//! (failure blocks merge).
//!
//! The assertion has two halves: **no fsync'd frame is lost**
//! (un-negotiable) AND **any un-fsync'd frame is handled
//! safely** (per RFC0008.5's three buckets — replayed if
//! complete + CRC-valid, torn-tail truncate on the newest
//! segment if partial, RFC0008.5 corruption if complete with
//! CRC mismatch). The test does *not* assert "exactly the
//! fsync'd frames" — kernel post-mortem flush admits surplus
//! unsynced frames being readable on restart.
//!
//! The crash is a **real** one: `run_fixture_then_sigkill`
//! spawns the `wal_crash_fixture` binary as a child process,
//! waits for it to append + sync + signal `READY`, then sends an
//! uncatchable `SIGKILL` via `Child::kill()`. The fixture never
//! drops its `Wal`, so no graceful close runs — the on-disk
//! state is exactly what a crash leaves. On restart this process
//! reopens the WAL and `replay`s it.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use ourios_wal::{FrameKind, FrameSink, RecoveryError, Wal, WalConfig, WalOffset};

fn default_config(root: &Path) -> WalConfig {
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
struct CollectingSink {
    frames: Vec<(FrameKind, Vec<u8>)>,
}

impl FrameSink for CollectingSink {
    fn consume(
        &mut self,
        _offset: WalOffset,
        kind: FrameKind,
        payload: &[u8],
    ) -> Result<(), RecoveryError> {
        self.frames.push((kind, payload.to_vec()));
        Ok(())
    }
}

/// Spawn the crash fixture against `root`, applying `ops` (see
/// the fixture's docs: `SYNC` or `<kind>:<hex>`), wait for its
/// `READY` line — which guarantees every op including the final
/// sync has committed — then `SIGKILL` it. Models a process
/// death at exactly that point.
fn run_fixture_then_sigkill(root: &Path, ops: &[&str]) {
    let exe = env!("CARGO_BIN_EXE_wal_crash_fixture");
    let mut child = Command::new(exe)
        .arg(root)
        .args(ops)
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
        "fixture must signal READY (got {line:?}) — it died before committing its ops",
    );
    // SIGKILL: uncatchable, no graceful WAL close.
    child.kill().expect("SIGKILL fixture");
    child.wait().expect("reap fixture");
}

/// Reopen the WAL at `root` after the crash and replay it,
/// asserting recovery itself succeeds (no corruption halt).
fn recover(root: &Path) -> Vec<(FrameKind, Vec<u8>)> {
    let mut sink = CollectingSink::default();
    Wal::open(default_config(root))
        .expect("reopen after crash")
        .replay(&mut sink)
        .expect("replay after crash must not error");
    sink.frames
}

/// SIGKILL with a frame fsync'd and a later frame appended but
/// **not** yet synced: the fsync'd frame MUST survive, and the
/// un-fsync'd one is handled safely (recovered whole or absent —
/// never a corruption halt, which `recover` already asserts).
#[test]
fn rfc0008_2_sigkill_between_append_and_sync_loses_no_fsynced_frame() {
    let tmp = tempfile::TempDir::new().expect("temp");
    // A: appended + fsync'd. B: appended, not synced, then crash.
    run_fixture_then_sigkill(tmp.path(), &["otlp:AA", "SYNC", "otlp:BB"]);

    let frames = recover(tmp.path());
    let synced = (FrameKind::OtlpBatch, vec![0xAA]);
    let unsynced = (FrameKind::OtlpBatch, vec![0xBB]);
    assert!(
        frames.contains(&synced),
        "the fsync'd frame must survive the crash; recovered = {frames:?}",
    );
    assert!(
        frames == vec![synced.clone()] || frames == vec![synced, unsynced],
        "the un-fsync'd frame is handled safely (recovered whole or absent), got {frames:?}",
    );
}

/// The §3.4 critical case: a frame is fsync'd, then the process
/// dies before the ack would be sent. The frame MUST be present
/// on restart — otherwise acknowledged-or-about-to-be-acked data
/// would be lost.
#[test]
fn rfc0008_2_sigkill_between_sync_and_ack_loses_no_fsynced_frame() {
    let tmp = tempfile::TempDir::new().expect("temp");
    run_fixture_then_sigkill(tmp.path(), &["otlp:DEADBEEF", "SYNC"]);

    assert_eq!(
        recover(tmp.path()),
        vec![(FrameKind::OtlpBatch, vec![0xDE, 0xAD, 0xBE, 0xEF])],
        "the fsync'd frame survives a crash that struck before the ack",
    );
}

/// `FrameKind::AuditEvent` frames fsync'd before the kill survive
/// identically alongside `OtlpBatch` frames, in order — the
/// RFC 0005 §3.7 audit-durability contract.
#[test]
fn rfc0008_2_audit_event_frames_survive_alongside_otlp_batches() {
    let tmp = tempfile::TempDir::new().expect("temp");
    run_fixture_then_sigkill(tmp.path(), &["otlp:11", "audit:2233", "otlp:44", "SYNC"]);

    assert_eq!(
        recover(tmp.path()),
        vec![
            (FrameKind::OtlpBatch, vec![0x11]),
            (FrameKind::AuditEvent, vec![0x22, 0x33]),
            (FrameKind::OtlpBatch, vec![0x44]),
        ],
        "fsync'd AuditEvent frames survive a SIGKILL alongside OtlpBatch frames, in order",
    );
}
