//! RFC0059.9 — A template replay mints afresh at or below the checkpoint
//! publishes its mapping before any listener opens.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.

use std::path::Path;

use ourios_core::audit::{AuditPayload, TemplateChange};

use crate::rfc0059_support::{Node, Restarted, cut, publish};

const TENANT: &str = "checkout";
const KEPT: &str = "user alice logged in";
const TAIL: &str = "disk sda1 is 91 percent full";

/// How the restart comes to re-mint at or below the checkpoint `X`.
#[derive(Clone, Copy, Debug)]
enum Remint {
    /// The snapshot is discarded, so every retained frame replays.
    Discarded,
    /// The snapshot lags the checkpoint (`S < X`), so `(S, X]` replays.
    Lagging,
}

/// Scenario RFC0059.9 — for a discarded snapshot and for `S < X`, every
/// template replay mints afresh from frames at or below `X` has its
/// events published, those frames' rows stay withheld, and a row
/// ingested after startup resolves through a published binding.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0059_9_a_reminted_replay_id_publishes_its_mapping() {
    for remint in [Remint::Discarded, Remint::Lagging] {
        let tmp = tempfile::TempDir::new().expect("temp");
        let node = node(tmp.path(), remint).await;

        let mut restarted = node.restart().expect("recover");
        let seated = restarted.report.template_ids.high_water;
        let published = created_ids(&restarted);
        assert!(
            restarted.records.drain().is_empty(),
            "{remint:?}: rows at or below X stay withheld"
        );
        assert!(
            published.iter().all(|id| *id > seated),
            "{remint:?}: only fresh ids are published, never a duplicate: {published:?}"
        );

        let tail = restarted.mine(TENANT, TAIL);
        assert!(tail > seated, "{remint:?}: {tail} was minted by the replay");
        assert!(
            published.contains(&tail),
            "{remint:?}: the row's id {tail} has a published binding ({published:?})"
        );
    }
}

/// A node that published `KEPT`, cut, published `TAIL`, cut again, and
/// stopped with every frame retained, its snapshot then made to replay.
async fn node(tmp: &Path, remint: Remint) -> Node {
    let rig = Node::rig(tmp);
    publish(&rig, TENANT, &[KEPT], &[]).await;
    cut(&rig).await;
    let lagging = std::fs::read(rig.snapshots_root.join(format!("{TENANT}.snap")))
        .expect("the first cut's snapshot");
    publish(&rig, TENANT, &[TAIL], &[]).await;
    cut(&rig).await;
    let node = Node::stop(rig, tmp);
    match remint {
        Remint::Discarded => node.overwrite_snapshot(TENANT, &[]),
        Remint::Lagging => node.overwrite_snapshot(TENANT, &lagging),
    }
    node
}

/// The ids of every `Created` event published since the restart.
fn created_ids(restarted: &Restarted) -> Vec<u64> {
    restarted
        .audit
        .drain()
        .into_iter()
        .filter_map(|event| match event.payload {
            AuditPayload::Template {
                template_id,
                change: TemplateChange::Created { .. },
                ..
            } => Some(template_id),
            _ => None,
        })
        .collect()
}
