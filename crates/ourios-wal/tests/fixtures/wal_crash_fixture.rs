//! Crash-recovery test fixture for `rfc0008_2_crash_recovery`.
//!
//! Not a product binary — declared as a `[[bin]]` (with
//! `publish = false` keeping it out of any release) purely so the
//! integration test can spawn it as a real OS process and
//! `SIGKILL` it mid-life. The crate is `#![deny(unsafe_code)]`
//! via the workspace lint, so a `fork()`-based harness is out; a
//! child process driven by `Child::kill()` (which sends `SIGKILL`
//! on Unix) is the no-`unsafe` way to exercise a genuine process
//! death — the `WAL` is never dropped, so no graceful close
//! runs, exactly as in a crash.
//!
//! Usage: `wal_crash_fixture <wal_root> <op>...`, where each
//! `<op>` is either the literal `SYNC` or `<kind>:<hexpayload>`
//! (`kind` ∈ {`otlp`, `audit`}). The fixture applies the ops in
//! order, prints `READY` to stdout (flushed) so the parent knows
//! every op — including the final sync — is done, then blocks
//! forever so the parent can kill it at that deterministic point.

use std::io::Write;

use ourios_wal::{FrameKind, Wal, WalConfig};

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().expect("fixture: missing <wal_root> arg");
    let config = WalConfig {
        root: root.into(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        macos_full_fsync: false,
    };
    let mut wal = Wal::open(config).expect("fixture: Wal::open");

    for op in args {
        if op == "SYNC" {
            wal.sync().expect("fixture: sync");
        } else {
            let (kind_str, hex) = op
                .split_once(':')
                .expect("fixture: op must be SYNC or <kind>:<hex>");
            let kind = match kind_str {
                "otlp" => FrameKind::OtlpBatch,
                "audit" => FrameKind::AuditEvent,
                other => panic!("fixture: unknown frame kind {other:?}"),
            };
            wal.append(kind, &decode_hex(hex)).expect("fixture: append");
        }
    }

    // Signal the parent that every op (incl. any final sync) is
    // committed, so it kills us at a known point.
    let mut stdout = std::io::stdout();
    writeln!(stdout, "READY").expect("fixture: write READY");
    stdout.flush().expect("fixture: flush READY");

    // Block until the parent SIGKILLs us. `park` avoids a busy
    // spin; SIGKILL is uncatchable, so the process dies here with
    // the `WAL` still open (no graceful close).
    loop {
        std::thread::park();
    }
}

fn decode_hex(s: &str) -> Vec<u8> {
    assert!(
        s.len() % 2 == 0,
        "fixture: hex payload must have even length"
    );
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("fixture: invalid hex byte"))
        .collect()
}
