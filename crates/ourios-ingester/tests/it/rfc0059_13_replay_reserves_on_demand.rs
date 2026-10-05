//! RFC0059.13 — Startup replay reserves on demand, past the ready blocks.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::template_ids::{BLOCK, HIGH_WATER_KEY, mark_seated};
use ourios_miner::cluster::MinerCluster;

use crate::ingest_support::{open_pipeline, request};
use crate::rfc0059_support::{Node, assert_equivalent_up_to_renaming, structured_logs};

const TENANT: &str = "checkout";
const HIGH_WATER: u64 = 10;

/// Scenario RFC0059.13 — a replay that mints more than two blocks of
/// templates on a healthy store fails none, and stays restore-equivalent
/// up to renaming.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_13_a_replay_minting_past_two_blocks_fails_no_template() {
    // Given a seated root whose WAL holds more first-seen templates than
    // the two blocks startup reserves.
    let tmp = tempfile::TempDir::new().expect("temp");
    let node = Node::empty(tmp.path());
    node.put(
        HIGH_WATER_KEY,
        format!(r#"{{"reserved_through": {HIGH_WATER}}}"#).as_bytes(),
    );
    mark_seated(&node.snapshots, HIGH_WATER).expect("seated");
    let templates = usize::try_from(2 * BLOCK + 500).expect("fits");
    // A hundred templates to a frame keeps the WAL turns few.
    let names: Vec<String> = (0..templates).map(|i| format!("event.{i}")).collect();
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

    // When the node restarts and replays every frame.
    let restarted = node.restart().expect("recover");

    // Then no template failed for want of an id, and the state equals a
    // from-scratch rebuild up to renaming.
    assert_eq!(restarted.miner.parse_failures_total(), 0);
    assert_eq!(
        restarted.miner.template_count(&TenantId::new(TENANT)),
        templates
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
