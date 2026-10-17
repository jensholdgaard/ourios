//! An age sweep publishes a template's audit event before its row, so a
//! stop in between leaves the restart's re-minted id beside an orphan.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §3.4, "When the
//! renaming is observable".

use std::collections::BTreeSet;

use ourios_core::audit::{AuditPayload, TemplateChange};

use crate::rfc0059_support::{Node, audit_bindings, rows};

const TENANT: &str = "claude-code";
const PROBE: &str = "closed-loop routing probe from claude-code";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_age_swept_audit_event_outlives_its_unpublished_row_across_a_restart() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let rig = Node::rig(tmp.path());
    rig.ingest(TENANT, &[PROBE]).await;

    // The receiver's age sweep: the whole audit buffer, only aged records.
    let drained = rig
        .pipeline
        .with_bound_miner(|_miner| rig.publish.drain_aged());
    assert!(rig.publish.write_ordered(drained, "age"));
    assert!(rig.data_files().is_empty(), "the row has not aged yet");

    let node = Node::stop(rig, tmp.path());
    let before = bound_ids(&node);
    assert_eq!(before.len(), 1, "the first process published its event");

    let restarted = node.restart().expect("recover");
    let replayed: BTreeSet<u64> = restarted.emitted();
    let forwarded: BTreeSet<u64> = restarted
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
        .collect();

    assert!(rows(&node.store.join("data")).is_empty());
    assert_eq!(replayed.len(), 1, "replay re-emits the probe's row");
    assert_eq!(forwarded, replayed, "replay forwards the re-minted binding");
    assert!(
        before.is_disjoint(&replayed),
        "the probe is re-minted under a fresh id: published {before:?}, replayed {replayed:?}; \
         the published id binds the same text and no row will ever carry it"
    );
}

/// The ids the store's audit stream binds to [`PROBE`].
fn bound_ids(node: &Node) -> BTreeSet<u64> {
    audit_bindings(&node.store.join("audit"))
        .into_iter()
        .filter(|(_, texts)| texts.contains(PROBE))
        .map(|((id, _), _)| id)
        .collect()
}
