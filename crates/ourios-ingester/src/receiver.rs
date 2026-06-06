//! OTLP receiver — **placeholder** (RFC 0003, `specified` → `red`).
//!
//! The ingest front door (OTLP logs over gRPC/HTTP), the Drain-derived
//! mining pipeline, and the WAL-before-ack durability path
//! (`CLAUDE.md` §3.4, RFC 0008) live here. The RFC is now `specified`,
//! and its §5 acceptance criteria (RFC0003.1–.15) are enumerated as
//! `#[ignore]`'d tests under `crates/ourios-ingester/tests/rfc0003_*` —
//! the `red` gate. This module stays deliberately empty until the green
//! slices land the ingest pipeline (wire decode → tenant fan-out →
//! WAL-before-ack → miner), rather than a half-built path ahead of
//! review.
