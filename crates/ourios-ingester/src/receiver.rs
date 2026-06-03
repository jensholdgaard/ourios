//! OTLP receiver — **placeholder** (RFC 0003, `drafted`).
//!
//! The ingest front door (OTLP logs over gRPC/HTTP), the Drain-derived
//! mining pipeline, and the WAL-before-ack durability path
//! (`CLAUDE.md` §3.4, RFC 0008) live here once RFC 0003 reaches `red`.
//! Nothing is implemented yet: this scaffold lands the crate and the
//! background compaction runner ([`crate::compactor`]) only, so the
//! receiver is deliberately empty rather than a half-built ingest path
//! ahead of its RFC.
