//! The consolidated WAL integration-test harness (RFC 0028 slice 2).
//!
//! One binary instead of 11 (RFC 0028 §1 / epic #382 — per-binary link
//! cost dominates `cargo test` wall time). Files here moved verbatim from
//! `tests/*.rs`; test names gain only this harness's module-path prefix
//! (RFC0028.1). Nothing is exempt (RFC0028.2): the crash tests SIGKILL
//! the `wal_crash_fixture` **child** `[[bin]]` (under `tests/fixtures/`),
//! never their own process.

mod append;
mod open;
mod recovery;
mod rfc0008_1_wal_before_ack;
mod rfc0008_2_crash_recovery;
mod rfc0008_3_recovery_o_n;
mod rfc0008_4_torn_writes;
mod rfc0008_5_corruption;
mod rfc0008_6_rotation;
mod rfc0008_7_checkpoint;
mod rfc0008_9_unflushed_bytes;
mod rfc0046_6_frame_kind_dimension;
mod rfc0052_11_pre_existing_orphan;
mod rfc0052_12_capped_passes;
mod rfc0052_13_floor_pinning;
mod rfc0052_16_temp_sweep;
mod rfc0052_17_reclaim_record;
mod rfc0052_2_truncation_bounds;
mod rfc0052_4_transient_rotation;
mod rfc0052_5_persistent_rotation;
mod rfc0052_record;
mod rfc0052_support;
