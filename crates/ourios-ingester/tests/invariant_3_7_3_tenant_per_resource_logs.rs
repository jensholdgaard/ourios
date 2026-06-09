//! Invariant §3.7.3 — tenant derivation runs per `ResourceLogs`.
//!
//! `CLAUDE.md` §3.7 requires every data path to be scoped per tenant, with
//! the tenant derived once per `ResourceLogs` (RFC 0003 §6.3), not once per
//! export batch. This is a receiver/pipeline behaviour, so the scenario
//! lives here rather than in `ourios-miner/tests` (the miner crate cannot
//! decode a wire `ExportLogsServiceRequest` or fan it out) — the same
//! relocation the miner-side stub's own comment anticipated ("lives with
//! RFC 0003 once the receiver crate exists"). The receiver-side fan-out is
//! also covered by RFC0003.3; this scenario carries it the rest of the way,
//! through the miner, to prove no record lands in the wrong tenant's tree.

mod ingest_support;

use ingest_support::{request, resource_logs, wal_config};
use ourios_core::config::MinerConfig;
use ourios_core::record::{MinedRecord, SharedRecordSink};
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::{IngestPipeline, TenantRule};
use ourios_miner::cluster::MinerCluster;
use ourios_wal::Wal;

/// `service.name` of the record carried by a mined string-body row, read
/// back from its resource attributes (the fan-out copies the Resource's
/// attributes onto every record, and the default tenant rule keys off this
/// same attribute — so it traces a record to the `ResourceLogs` it came
/// from independently of the tenant tag).
fn service_name(record: &MinedRecord) -> &str {
    record
        .resource_attributes
        .iter()
        .find(|kv| kv.key == "service.name")
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| match v.value.as_ref() {
            Some(ourios_core::otlp::any_value::Value::StringValue(s)) => Some(s.as_str()),
            _ => None,
        })
        .expect("mined record carries its Resource's service.name")
}

/// Scenario §3.7.3 — Tenant derivation runs per `ResourceLogs`, not per export batch.
/// See `docs/rfcs/0001-template-miner.md` §5.
///
/// One `ExportLogsServiceRequest` carries two `ResourceLogs` whose
/// `Resource.attributes` resolve (default rule = `service.name`) to distinct
/// tenants `a` and `b`, each with two records. Driven through the receiver
/// pipeline (decode is upstream; this starts from the decoded request),
/// every record under `ResourceLogs[0]` must be mined under tenant `a` and
/// every record under `ResourceLogs[1]` under tenant `b`, with no record in
/// the wrong tenant's tree.
#[test]
fn invariant_3_7_3_tenant_derivation_runs_per_resource_logs() {
    // Arrange — a record sink to inspect what each tenant's tree mined, a
    // real WAL (the pipeline fsyncs before mining), and a pipeline with the
    // default `service.name` tenant rule. The two groups use distinct body
    // text per tenant so each tenant's tree allocates its own templates.
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal = Wal::open(wal_config(tmp.path())).expect("open WAL");
    let sink = SharedRecordSink::new();
    let miner = MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let mut pipeline = IngestPipeline::new(Box::new(wal), miner, TenantRule::service_name());

    let export = request(vec![
        resource_logs("a", &["a line one", "a line two"]),
        resource_logs("b", &["b line one", "b line two"]),
    ]);

    // Act
    let ingested = pipeline.ingest(export).expect("the batch is acked");

    // Assert — four records ingested; partition the mined records by the
    // tenant the miner tagged them with.
    assert_eq!(ingested, 4, "two records per ResourceLogs group");
    let emitted = sink.drain();
    assert_eq!(emitted.len(), 4, "every record reached the miner");

    let tenant_a = TenantId::new("a");
    let tenant_b = TenantId::new("b");
    let group_a: Vec<&MinedRecord> = emitted.iter().filter(|r| r.tenant_id == tenant_a).collect();
    let group_b: Vec<&MinedRecord> = emitted.iter().filter(|r| r.tenant_id == tenant_b).collect();

    // Each ResourceLogs group's two records landed under its own tenant …
    assert_eq!(group_a.len(), 2, "ResourceLogs[0]'s records are tenant `a`");
    assert_eq!(group_b.len(), 2, "ResourceLogs[1]'s records are tenant `b`");

    // … and no record is cross-tagged: tenant `a`'s records all carry the
    // service.name `a` (group 0's Resource), tenant `b`'s all carry `b`.
    for r in &group_a {
        assert_eq!(
            service_name(r),
            "a",
            "no group-1 record leaked into tenant `a`"
        );
    }
    for r in &group_b {
        assert_eq!(
            service_name(r),
            "b",
            "no group-0 record leaked into tenant `b`"
        );
    }

    // The per-tenant trees are scoped (`CLAUDE.md` §3.7): each tenant holds
    // exactly its two distinct templates, and neither tree saw the other's.
    assert_eq!(
        pipeline.miner().template_count(&tenant_a),
        2,
        "tenant `a`'s tree mined only its own two lines",
    );
    assert_eq!(
        pipeline.miner().template_count(&tenant_b),
        2,
        "tenant `b`'s tree mined only its own two lines",
    );
}
