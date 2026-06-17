//! RFC 0014 — ingest write path (record sink + flush policy) acceptance
//! scenarios (§5).
//!
//! **Status: `red`.** These are the failing stubs that drive the `green`
//! implementation: each encodes one RFC0014.§5 scenario and currently
//! `todo!()`s. They are `#[ignore]`d so the default `cargo test` (and CI)
//! stays green while the buffering `ParquetRecordSink` is built — `green`
//! replaces each body with a real assertion against the sink (per-partition
//! buffers; hybrid size + age + WAL-rotation flush; hard byte-ceiling with
//! blocking backpressure) and removes the `#[ignore]`.
//!
//! Placement may shift at `green`: RFC0014.5 (crash recovery) extends the RFC
//! 0008 WAL harness here in `ourios-ingester`; the buffer-trigger scenarios
//! (.1–.4, .6) may move next to the sink wherever it lands.
//!
//! See `docs/rfcs/0014-ingest-write-path.md` §5/§6.

/// Scenario RFC0014.1 — Size trigger: the emit that crosses the size target
/// flushes the partition to one right-sized Parquet object.
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
#[ignore = "RFC0014.1 — red until the ParquetRecordSink + flush policy land (green)"]
fn rfc0014_1_size_trigger() {
    todo!("RFC0014.1: a partition flushes on the emit that crosses the size target")
}

/// Scenario RFC0014.2 — Age trigger: a sub-target low-volume partition flushes
/// on the next batch-window tick once its oldest record reaches `max_buffer_age`.
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
#[ignore = "RFC0014.2 — red until the ParquetRecordSink + flush policy land (green)"]
fn rfc0014_2_age_trigger() {
    todo!("RFC0014.2: low-volume partition flushes on age")
}

/// Scenario RFC0014.3 — Rotation force-flush: a WAL segment rotation flushes
/// every partition (including sub-threshold ones); nothing un-flushed predates
/// the sealed segment.
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
#[ignore = "RFC0014.3 — red until the ParquetRecordSink + flush policy land (green)"]
fn rfc0014_3_rotation_force_flush() {
    todo!("RFC0014.3: rotation flushes every partition")
}

/// Scenario RFC0014.4 — Bounded memory: the sink early-flushes under pressure
/// and, at the hard ceiling, `emit` blocks so buffered bytes never exceed it.
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
#[ignore = "RFC0014.4 — red until the ParquetRecordSink + flush policy land (green)"]
fn rfc0014_4_bounded_memory() {
    todo!("RFC0014.4: hard ceiling, never exceeded")
}

/// Scenario RFC0014.5 — No acknowledged-data loss: a crash with a non-empty
/// buffer loses nothing — WAL replay re-mines every un-flushed acknowledged
/// record (`CLAUDE.md` §3.4).
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
#[ignore = "RFC0014.5 — red until the ParquetRecordSink + flush policy land (green)"]
fn rfc0014_5_no_acknowledged_data_loss() {
    todo!("RFC0014.5: crash mid-buffer loses no acknowledged data (WAL replay)")
}

/// Scenario RFC0014.6 — Tenant isolation: a flush produces an object holding
/// only one tenant's rows; no buffer or flush crosses tenants (`CLAUDE.md` §3.7).
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
#[ignore = "RFC0014.6 — red until the ParquetRecordSink + flush policy land (green)"]
fn rfc0014_6_tenant_isolation() {
    todo!("RFC0014.6: no cross-tenant buffer or flush")
}
