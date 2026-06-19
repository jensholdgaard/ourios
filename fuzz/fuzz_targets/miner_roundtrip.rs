#![no_main]

//! RFC0015.1 — the §3.3 bit-identical-reconstruction oracle.
//!
//! Build a `String`-body `OtlpLogRecord` from the fuzz input, ingest it,
//! drain the emitted `MinedRecord`, look up the leaf's template tokens,
//! and render the record back. For a string body the rendered bytes must
//! equal the original body whether `render` reports `Faithful` (rebuilt
//! from the template) or `RetainedVerbatim` (original body retained) — a
//! mismatch in either case is a §3.3 violation and crashes the target.

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use ourios_core::config::MinerConfig;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::reconstruct;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(line) = String::arbitrary(&mut u) else {
        return;
    };

    let sink = SharedRecordSink::new();
    let mut miner =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));

    let tenant = TenantId::new("fuzz");
    let record = OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(line.clone())),
        ..Default::default()
    };

    miner.ingest(&record);

    let mined = sink.drain();
    let Some(mined) = mined.first() else {
        return;
    };

    // Tokens for this record's (template_id, template_version). If the
    // leaf can't be found the harness can't validate — skip rather than
    // raise a false positive.
    let leaves = miner.templates_for(&tenant);
    let Some(leaf) = leaves.iter().find(|l| {
        l.template_id == mined.template_id && l.template_version == mined.template_version
    }) else {
        return;
    };

    let (rendered, _signal) = reconstruct::render(mined, &leaf.template);

    assert_eq!(
        rendered,
        line.as_bytes(),
        "RFC0015.1 / CLAUDE.md §3.3: string-body reconstruction must be byte-identical"
    );
});
