//! RFC0059.13 — Startup replay reserves on demand, past the ready blocks.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::recovery::RecoveryDriverError;
use ourios_ingester::template_ids::{BLOCK, HIGH_WATER_KEY, TemplateIdsError, mark_seated};
use ourios_miner::cluster::MinerCluster;

use crate::ingest_support::{open_pipeline, request};
use crate::rfc0059_support::{Hooks, Node, assert_equivalent_up_to_renaming, structured_logs};

const TENANT: &str = "checkout";
const HIGH_WATER: u64 = 10;
/// More first-seen templates than the current block and the two ready
/// blocks startup reserves.
const TEMPLATES: u64 = 3 * BLOCK + 500;

/// A seated root whose WAL holds [`TEMPLATES`] first-seen templates,
/// a hundred to a frame, and the batches it ingested.
async fn seated_root_past_the_startup_blocks(
    tmp: &std::path::Path,
) -> (Node, Vec<ExportLogsServiceRequest>) {
    let node = Node::empty(tmp);
    node.put(
        HIGH_WATER_KEY,
        format!(r#"{{"reserved_through": {HIGH_WATER}}}"#).as_bytes(),
    );
    mark_seated(&node.snapshots, HIGH_WATER).expect("seated");
    let names: Vec<String> = (0..TEMPLATES).map(|i| format!("event.{i}")).collect();
    let batches: Vec<_> = names
        .chunks(100)
        .map(|chunk| request(chunk.iter().map(|e| structured_logs(TENANT, e)).collect()))
        .collect();
    let pipeline = open_pipeline(&node.wal);
    for batch in &batches {
        pipeline
            .ingest(batch.clone(), TenantId::new(TENANT))
            .await
            .expect("ack");
    }
    drop(pipeline);
    (node, batches)
}

/// Scenario RFC0059.13 — a replay that mints past the three blocks
/// startup reserves fails no template, takes the ids beyond them from its
/// own synchronous reservations (the refiller only starts once replay
/// ends), and stays restore-equivalent up to renaming.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_13_a_replay_minting_past_the_startup_blocks_fails_no_template() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (node, batches) = seated_root_past_the_startup_blocks(tmp.path()).await;

    let restarted = node.restart().expect("recover");

    let templates = usize::try_from(TEMPLATES).expect("fits");
    assert_eq!(restarted.miner.parse_failures_total(), 0);
    assert_eq!(
        restarted.miner.template_count(&TenantId::new(TENANT)),
        templates
    );
    assert!(
        restarted.miner.highest_allocated() > HIGH_WATER + 3 * BLOCK,
        "ids past the startup blocks came from replay's synchronous reservations"
    );
    let mut control = MinerCluster::new(MinerConfig::default());
    for batch in batches {
        for record in assign(batch, &TenantId::new(TENANT)) {
            control.ingest(&record);
        }
    }
    let renamed = assert_equivalent_up_to_renaming(&restarted.miner, &control, HIGH_WATER);
    assert_eq!(
        renamed.len(),
        templates,
        "every id came from a reserved block"
    );
}

/// Scenario RFC0059.13 — when replay drains the startup blocks and its
/// next reservation fails, recovery fails with that error before the
/// frame is folded or published, rather than replaying the templates as
/// parse failures.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_13_a_failed_reservation_during_replay_fails_recovery() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (node, _) = seated_root_past_the_startup_blocks(tmp.path()).await;
    let hooks = Hooks::default();
    // Startup's three reservations land; replay's first one fails.
    hooks
        .high_water_puts_until_failure
        .store(4, std::sync::atomic::Ordering::Release);

    let Err(err) = node.restart_over(hooks.wrap(node.store())) else {
        panic!("a reservation replay cannot make must fail recovery");
    };

    assert!(
        matches!(
            err,
            RecoveryDriverError::TemplateIds(TemplateIdsError::Store { .. })
        ),
        "{err}"
    );
}
