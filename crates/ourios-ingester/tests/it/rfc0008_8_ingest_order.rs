//! RFC0008.8 — concurrent ingest preserves WAL-append order at the miner.
//! See `docs/rfcs/0008-wal.md` §5 + `CLAUDE.md` §3.4.
//!
//! Batched fsync makes `ingest` concurrent (the whole point — fold N
//! fsyncs into one window). But the miner must still see records in
//! **WAL-append order**: template ids are assigned first-seen, so an
//! out-of-order live tree would not match a WAL-order replay
//! (snapshot-restore §3.5.3). The `CommitCoordinator`'s in-order
//! hand-off (`await_ingest_turn` / `complete_ingest`) guarantees it.
//!
//! This test fires many concurrent ingests of *structurally distinct*
//! templates (so the resulting template-id assignment is order-sensitive)
//! and asserts the live miner's per-tenant snapshot equals a control
//! miner that replayed the resulting WAL frames in order. Whatever order
//! the concurrent appends happened to land in the WAL, the live miner and
//! the WAL-order control agree — which is exactly "miner order == WAL
//! order". Without the hand-off the live order would be commit-completion
//! order and the two would diverge.

use crate::ingest_support::{replay_frames, request, resource_logs, shared_wal_pipeline};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_config::MinerConfig;
use ourios_ingester::receiver::{TenantRule, fan_out};
use ourios_miner::cluster::MinerCluster;
use ourios_wal::FrameKind;
use prost::Message;

const N: usize = 24;

/// Distinct templates: a single-letter fixed token (`a`, `b`, …) keeps
/// each body structurally unique (letters aren't masked like numbers
/// are), so the miner mints a separate `template_id` per body and the
/// assignment order is observable.
fn distinct_body(i: usize) -> String {
    let tag = char::from(b'a' + u8::try_from(i).expect("N fits u8"));
    format!("event kind {tag} completed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0008_8_concurrent_ingest_preserves_wal_order_at_the_miner() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let pipeline = shared_wal_pipeline(tmp.path());

    // Fire N concurrent ingests, each a distinct template under one tenant.
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let pipeline = pipeline.clone();
        handles.push(tokio::spawn(async move {
            let body = distinct_body(i);
            let export = request(vec![resource_logs("checkout", &[body.as_str()])]);
            pipeline.ingest(export).await
        }));
    }
    for h in handles {
        h.await
            .expect("task joins")
            .expect("each concurrent ingest acks");
    }

    // Snapshot the live miner's per-tenant state, then release the WAL.
    let live: Vec<_> = pipeline.with_miner(|m| {
        m.tenant_ids()
            .into_iter()
            .map(|t| {
                let state = m.snapshot_state(&t);
                (t, state)
            })
            .collect()
    });
    drop(pipeline);

    // Build a control miner by replaying the WAL frames in order — this is
    // the authoritative WAL-append order, whatever the concurrent race
    // produced.
    let rule = TenantRule::service_name();
    let mut control = MinerCluster::new(MinerConfig::default());
    for (kind, payload) in replay_frames(tmp.path()) {
        assert_eq!(kind, FrameKind::OtlpBatch);
        let request = ExportLogsServiceRequest::decode(payload.as_slice()).expect("decode frame");
        for record in fan_out(request, &rule).expect("fan out") {
            control.ingest(&record);
        }
    }

    assert_eq!(
        live.len(),
        control.tenant_ids().len(),
        "same tenant set live and replayed",
    );
    for (tenant, live_state) in live {
        assert_eq!(
            live_state,
            control.snapshot_state(&tenant),
            "tenant {tenant:?}: the live (concurrently-ingested) miner diverges from the \
             WAL-order replay — records reached the miner out of WAL order",
        );
    }
}
