#![no_main]

//! RFC0015.1 / RFC0015.10 — the §3.3 bit-identical-reconstruction and the
//! §3.1 no-silent-merge oracles over a *sequence* of lines.
//!
//! The input is split on `\n`; every line is ingested as a `String`-body
//! record into one `MinerCluster` for one tenant, so the fuzzer drives the
//! Drain walk through creation, clean reuse, widening and type expansion,
//! not just one line against an empty tree. Right after each ingest the leaf
//! the record was stamped with is snapshotted by `(template_id,
//! template_version)` — the read path's registry key (RFC 0017 §3.5) — so a
//! row is later rendered against its own version's tokens, not the widened
//! ones. Then, for every emitted record:
//!
//! - **(a)** `render` equals the line byte for byte, or the row was diverted
//!   (`NO_TEMPLATE`) and carries the line verbatim with `lossy_flag` set;
//! - **(b)** the line agrees with its template's literals at every
//!   non-wildcard position, and versions of one template only ever widen
//!   (the `assert_no_silent_merge` oracle of the RFC 0024 property suite);
//! - **(c)** each ingest emitted exactly one record;
//! - **(d)** a clean row (not lossy, no §6.5 overflow) was rebuilt from its
//!   template (`Reconstruction::Faithful`), never surfaced from a body;
//! - **(e)** every template an ingest created, widened or type-expanded is
//!   accounted for by its audit events, old and new templates matching the
//!   tree before and after (§3.1 "every merge emits an audit event", RFC 0001
//!   H1.3); checked after each ingest against an observable audit sink.
//!
//! Every violation panics with a greppable `RFC0015.N / CLAUDE.md §3.x`
//! message, which libFuzzer reports as a crash.

use libfuzzer_sys::fuzz_target;
use ourios_config::MinerConfig;
use ourios_core::audit::SharedAuditSink;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};

#[path = "../common/oracles.rs"]
mod oracles;
#[path = "../common/telemetry.rs"]
mod telemetry;

// Realistic log lines are small; cap each candidate line so a pathological
// multi-MB input can't dominate iteration time / memory during fuzzing.
// 4 KiB still exercises tokenization, the 256-byte param overflow, and
// reconstruction comfortably.
const MAX_LINE_BYTES: usize = 4096;

// Enough lines to create, widen, type-expand and re-attach several templates;
// few enough that one input cannot grow a tree the run spends its budget
// walking (the per-tenant store is otherwise bounded only by the RFC 0023
// defaults, 20 000 leaves).
const MAX_LINES: usize = 64;

fuzz_target!(init: telemetry::init(), |data: &[u8]| {
    // Lossy UTF-8 per line, never discarding an input: a line cut at the cap
    // becomes U+FFFD at the cut, which is still a line the receiver could
    // deliver.
    let lines: Vec<String> = data
        .split(|&b| b == b'\n')
        .take(MAX_LINES)
        .map(|raw| String::from_utf8_lossy(&raw[..raw.len().min(MAX_LINE_BYTES)]).into_owned())
        .collect();

    let sink = SharedRecordSink::new();
    let audit = SharedAuditSink::new();
    let mut miner = MinerCluster::with_audit_sink(MinerConfig::default(), Box::new(audit.clone()))
        .with_record_sink(Box::new(sink.clone()));
    let tenant = TenantId::new("fuzz");

    let mut templates = oracles::Templates::new();
    let mut records = Vec::with_capacity(lines.len());
    let mut leaves = oracles::Leaves::new();
    for (index, line) in lines.iter().enumerate() {
        let record = OtlpLogRecord {
            tenant_id: tenant.clone(),
            body: Some(Body::String(line.clone())),
            ..Default::default()
        };
        miner.ingest(&record);

        // (e) — every template the ingest created or moved forward is audited.
        let after = oracles::leaves(&miner, &tenant);
        oracles::assert_changes_audited(index, &leaves, &after, audit.drain());
        leaves = after;

        // (c) — one string-body ingest, one emitted row.
        let emitted = sink.drain();
        let Ok([mined]) = <[_; 1]>::try_from(emitted) else {
            panic!("RFC0015.10: line {index}: one string-body ingest must emit exactly one record");
        };
        if mined.template_id != NO_TEMPLATE {
            oracles::snapshot_leaf(&miner, &tenant, &mined, &mut templates);
        }
        records.push(mined);
    }

    for (index, (line, record)) in lines.iter().zip(&records).enumerate() {
        if record.template_id != NO_TEMPLATE {
            oracles::assert_no_silent_merge(index, line, record, &templates);
        }
        oracles::assert_reconstructs(index, line, record, &templates);
    }
    oracles::assert_widening_is_monotone(&templates);
});
