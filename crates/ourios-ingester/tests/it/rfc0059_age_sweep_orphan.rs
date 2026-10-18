//! RFC0059.20 — A crash after an age sweep leaves an orphan id; a cut does
//! not. The age sweep publishes a template's audit event before its row, so
//! a stop in between leaves the restart's re-minted id beside an id no row
//! carries. A cut, the drain and snapshot a graceful shutdown's
//! `flush_then_snapshot` also runs, leaves none.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §3.4 and §5.

use std::collections::BTreeSet;

use ourios_core::audit::{AuditPayload, TemplateChange};

use crate::rfc0052_barrier_support::BarrierRig;
use crate::rfc0059_support::{Node, Restarted, audit_bindings, cut, rows};

const TENANT: &str = "claude-code";
const PROBE: &str = "closed-loop routing probe from claude-code";

/// Scenario RFC0059.20 — the crash arm: the published id binds the probe's
/// text and no row; the replayed row carries a re-minted id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_age_swept_audit_event_outlives_its_unpublished_row_across_a_restart() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Node::rig(tmp.path());
    rig.ingest(TENANT, &[PROBE]).await;
    age_sweep(&rig);
    assert!(rig.data_files().is_empty(), "the row has not aged yet");

    let node = Node::stop(rig, tmp.path());
    let before = bound_ids(&node);
    assert_eq!(before.len(), 1, "the first process published its event");

    let restarted = node.restart().expect("recover");
    let replayed: BTreeSet<u64> = restarted.emitted();
    let forwarded = created_ids(&restarted);

    assert!(rows(&node.store.join("data")).is_empty());
    assert_eq!(replayed.len(), 1, "replay re-emits the probe's row");
    assert_eq!(forwarded, replayed, "replay forwards the re-minted binding");
    assert!(
        before.is_disjoint(&replayed),
        "the probe is re-minted under a fresh id: published {before:?}, replayed {replayed:?}; \
         the published id binds the same text and no row will ever carry it"
    );
}

/// Scenario RFC0059.20 — the cut arm: with a cut before the stop, the
/// restart restores the template under its id, so one id binds the text
/// and a published row carries it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_before_the_stop_leaves_no_orphan_id() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Node::rig(tmp.path());
    rig.ingest(TENANT, &[PROBE]).await;
    age_sweep(&rig);
    cut(&rig).await;

    let node = Node::stop(rig, tmp.path());
    let restarted = node.restart().expect("recover");

    let bound = bound_ids(&node);
    let carried: BTreeSet<u64> = rows(&node.store.join("data"))
        .iter()
        .map(|row| row.template_id)
        .collect();
    assert_eq!(bound.len(), 1, "one id binds the probe's text: {bound:?}");
    assert_eq!(carried, bound, "a published row carries that id");
    assert!(restarted.emitted().is_empty(), "replay re-emits no row");
    assert!(
        created_ids(&restarted).is_empty(),
        "replay forwards no new binding"
    );
}

/// The receiver's age sweep: the whole audit buffer, only aged records.
fn age_sweep(rig: &BarrierRig) {
    let drained = rig
        .pipeline
        .with_bound_miner(|_miner| rig.publish.drain_aged());
    assert!(rig.publish.write_ordered(drained, "age"));
}

/// The ids of the `Created` events recovery forwarded.
fn created_ids(restarted: &Restarted) -> BTreeSet<u64> {
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

/// The ids the store's audit stream binds to [`PROBE`].
fn bound_ids(node: &Node) -> BTreeSet<u64> {
    audit_bindings(&node.store.join("audit"))
        .into_iter()
        .filter(|(_, texts)| texts.contains(PROBE))
        .map(|((id, _), _)| id)
        .collect()
}
