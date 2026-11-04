//! SPIKE (RFC 0068 §4.3): interleaved A/B of the durable synced offset.
//!
//! Criterion runs one variant after another, so disk-latency drift on a
//! shared runner lands between variants. Here every round runs each mode once,
//! in a rotating order, against a steady-state WAL per mode, and per-op
//! latencies are pooled. Usage: `rfc0068_ab [rounds]`.
#![allow(clippy::cast_precision_loss)] // µs reporting only

use std::time::Instant;

use ourios_wal::{FrameKind, SyncedOffsetMode, Wal, WalConfig};

const SHAPES: [(usize, usize); 3] = [(4 * 1024, 1), (64 * 1024, 1), (4 * 1024, 16)];
const MODES: [(&str, Option<SyncedOffsetMode>); 4] = [
    ("off", None),
    ("sidecar_seq", Some(SyncedOffsetMode::SidecarSequential)),
    (
        "sidecar_concurrent",
        Some(SyncedOffsetMode::SidecarConcurrent),
    ),
    ("in_segment", Some(SyncedOffsetMode::InSegment)),
];
const REOPEN_EVERY: usize = 256;

struct Slot {
    _dir: tempfile::TempDir,
    wal: Wal,
    ops: usize,
}

fn fresh(mode: Option<SyncedOffsetMode>, payload: &[u8]) -> Slot {
    let dir = tempfile::TempDir::new().expect("temp");
    let mut wal = Wal::open(WalConfig {
        root: dir.path().to_path_buf(),
        batch_window_ms: 100,
        segment_size_bytes: 128 * 1024 * 1024,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    })
    .expect("open");
    if let Some(mode) = mode {
        wal.enable_synced_offset_spike(mode).expect("synced offset");
    }
    wal.append(FrameKind::OtlpBatch, payload).expect("warm");
    wal.sync().expect("warm sync");
    Slot {
        _dir: dir,
        wal,
        ops: 0,
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    let i = ((sorted.len() - 1) as f64 * p).round();
    // Index is within bounds by construction.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    sorted[i as usize]
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    println!("rounds={rounds} (per-op µs; overhead vs off on p50 / mean)");
    println!(
        "{:<10} {:<19} {:>8} {:>8} {:>8} {:>8} {:>8} {:>9} {:>9}",
        "shape", "mode", "p10", "p50", "p90", "p99", "mean", "Δp50", "Δmean"
    );
    for (frame_len, frames) in SHAPES {
        let payload = vec![0xA5u8; frame_len];
        let mut slots: Vec<Slot> = MODES.iter().map(|(_, m)| fresh(*m, &payload)).collect();
        let mut samples: Vec<Vec<f64>> = vec![Vec::with_capacity(rounds); MODES.len()];
        for round in 0..rounds {
            for k in 0..MODES.len() {
                let m = (round + k) % MODES.len();
                if slots[m].ops == REOPEN_EVERY {
                    slots[m] = fresh(MODES[m].1, &payload);
                }
                let slot = &mut slots[m];
                let start = Instant::now();
                for _ in 0..frames {
                    slot.wal
                        .append(FrameKind::OtlpBatch, &payload)
                        .expect("append");
                }
                std::hint::black_box(slot.wal.sync().expect("sync"));
                samples[m].push(start.elapsed().as_secs_f64() * 1e6);
                slot.ops += 1;
            }
        }
        let stats: Vec<(f64, f64, [f64; 4])> = samples
            .iter_mut()
            .map(|s| {
                s.sort_by(f64::total_cmp);
                let mean = s.iter().sum::<f64>() / s.len() as f64;
                let p = [pct(s, 0.1), pct(s, 0.5), pct(s, 0.9), pct(s, 0.99)];
                (p[1], mean, p)
            })
            .collect();
        let (base_p50, base_mean, _) = stats[0];
        for ((name, _), (p50, mean, p)) in MODES.iter().zip(&stats) {
            println!(
                "{:<10} {:<19} {:>8.0} {:>8.0} {:>8.0} {:>8.0} {:>8.0} {:>+8.1}% {:>+8.1}%",
                format!("{}k_x{frames}", frame_len / 1024),
                name,
                p[0],
                p50,
                p[2],
                p[3],
                mean,
                (p50 / base_p50 - 1.0) * 100.0,
                (mean / base_mean - 1.0) * 100.0
            );
        }
    }
}
