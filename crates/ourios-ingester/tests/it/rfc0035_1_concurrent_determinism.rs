//! RFC0035.1 — determinism is preserved under the ordered/concurrent
//! split (the load-bearing guard). See
//! `docs/rfcs/0035-ingest-concurrency.md` §5.
//!
//! The multi-tenant extension of
//! `rfc0008_8_concurrent_ingest_preserves_wal_order_at_the_miner`
//! (which stays green unchanged, and is single-tenant — it would not
//! catch cross-tenant reordering): N tenants ingest concurrently
//! through the pooled pipeline, sharing structurally identical bodies
//! across tenants so the **cluster-wide** template-id counter's
//! interleaving is observable (RFC 0001 §6.1 / §3.7.2 — a cross-tenant
//! reorder changes assigned id values). Every tenant's live
//! `snapshot_state` must equal a control that replayed the same WAL
//! frames in strict global order.
//!
//! The proptest arm makes the "any interleaving ⇒ same ids" claim
//! adversarial rather than example-based: random tenant counts, batch
//! mixes, and template selections, each case run concurrently and
//! compared to its serial WAL-order-replay control.

use crate::ingest_support::{pooled_wal_pipeline, replay_frames, request, resource_logs};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_config::MinerConfig;
use ourios_ingester::receiver::{TenantRule, fan_out};
use ourios_miner::cluster::MinerCluster;
use ourios_wal::FrameKind;
use proptest::prelude::*;
use prost::Message;

/// Replay `root`'s WAL frames in order into a fresh control miner and
/// assert every tenant's live state equals the control's.
fn assert_equals_wal_order_replay(
    root: &std::path::Path,
    live: Vec<(
        ourios_core::tenant::TenantId,
        ourios_miner::snapshot::SnapshotState,
    )>,
) {
    let rule = TenantRule::service_name();
    let mut control = MinerCluster::new(MinerConfig::default());
    for (kind, payload) in replay_frames(root) {
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
            "tenant {tenant:?}: the live (concurrently-ingested, pooled) miner diverges \
             from the WAL-order replay — an assigned template id or tree was perturbed",
        );
    }
}

/// Snapshot every tenant's live state from the pipeline.
fn live_states(
    pipeline: &ourios_ingester::receiver::SharedPipeline,
) -> Vec<(
    ourios_core::tenant::TenantId,
    ourios_miner::snapshot::SnapshotState,
)> {
    pipeline.with_miner(|m| {
        m.tenant_ids()
            .into_iter()
            .map(|t| {
                let state = m.snapshot_state(&t);
                (t, state)
            })
            .collect()
    })
}

const TENANTS: usize = 8;
const BATCHES_PER_TENANT: usize = 6;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0035_1_multi_tenant_concurrent_ingest_matches_wal_order_replay() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let wal_root = tmp.path().join("wal");
    std::fs::create_dir_all(&wal_root).expect("wal root");
    let (pipeline, _sink) = pooled_wal_pipeline(&wal_root, &tmp.path().join("store"), 4);

    // Structurally identical bodies across tenants: each tenant's k-th
    // batch mints (or attaches to) the same template shape, so the
    // shared counter's cross-tenant interleaving decides the id values.
    let mut handles = Vec::with_capacity(TENANTS * BATCHES_PER_TENANT);
    for t in 0..TENANTS {
        for k in 0..BATCHES_PER_TENANT {
            let pipeline = pipeline.clone();
            handles.push(tokio::spawn(async move {
                let service = format!("svc-{t}");
                let tag = char::from(b'a' + u8::try_from(k).expect("k fits u8"));
                let body = format!("event kind {tag} completed");
                let export = request(vec![resource_logs(&service, &[body.as_str()])]);
                pipeline.ingest(export).await
            }));
        }
    }
    for h in handles {
        h.await
            .expect("task joins")
            .expect("each concurrent ingest acks");
    }

    let live = live_states(&pipeline);
    assert_eq!(live.len(), TENANTS, "every tenant allocated");
    drop(pipeline); // release the WAL for the control replay
    assert_equals_wal_order_replay(&wal_root, live);
}

/// One generated batch: a tenant index and the indices of the template
/// shapes its records draw from.
#[derive(Clone, Debug)]
struct GenBatch {
    tenant: usize,
    bodies: Vec<usize>,
}

/// The template-shape pool the generator draws from: distinct token
/// structures (id-assignment order-sensitive), some with numeric params
/// (attach/widen paths).
fn body_text(shape: usize, record: usize) -> String {
    match shape % 6 {
        0 => format!("user {record} logged in"),
        1 => format!("user {record} logged out"),
        2 => format!("charge {record} EUR accepted"),
        3 => "cache warmed".to_owned(),
        4 => format!("event kind {} completed", ["a", "b", "c"][record % 3]),
        _ => format!("job {record} retried after {record} ms"),
    }
}

proptest! {
    // Each case spins a real WAL + multi-thread runtime; keep the case
    // count bounded (PROPTEST_CASES overrides, ourios-testgen convention).
    #![proptest_config(ProptestConfig {
        cases: ourios_testgen::proptest_cases(16),
        ..ProptestConfig::default()
    })]

    /// RFC0035.1 property: for ANY tenant/batch/template interleaving,
    /// concurrent pooled ingest yields per-tenant state identical to the
    /// serial WAL-order replay.
    #[test]
    fn rfc0035_1_any_interleaving_matches_wal_order_replay(
        tenants in 2usize..=4,
        batches in proptest::collection::vec(
            (0usize..4, proptest::collection::vec(0usize..6, 1..=3)),
            1..=12,
        ),
    ) {
        let batches: Vec<GenBatch> = batches
            .into_iter()
            .map(|(tenant, bodies)| GenBatch { tenant: tenant % tenants, bodies })
            .collect();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let tmp = tempfile::TempDir::new().expect("temp");
            let wal_root = tmp.path().join("wal");
            std::fs::create_dir_all(&wal_root).expect("wal root");
            let (pipeline, _sink) =
                pooled_wal_pipeline(&wal_root, &tmp.path().join("store"), 3);

            let mut handles = Vec::with_capacity(batches.len());
            for (i, batch) in batches.into_iter().enumerate() {
                let pipeline = pipeline.clone();
                handles.push(tokio::spawn(async move {
                    let service = format!("svc-{}", batch.tenant);
                    let bodies: Vec<String> = batch
                        .bodies
                        .iter()
                        .map(|&shape| body_text(shape, i))
                        .collect();
                    let refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
                    let export = request(vec![resource_logs(&service, &refs)]);
                    pipeline.ingest(export).await
                }));
            }
            for h in handles {
                h.await.expect("task joins").expect("ingest acks");
            }

            let live = live_states(&pipeline);
            drop(pipeline);
            assert_equals_wal_order_replay(&wal_root, live);
        });
    }
}
