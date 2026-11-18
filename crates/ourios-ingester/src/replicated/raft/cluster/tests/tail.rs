//! What a running tail replica serves of what it keeps (RFC 0065 §6.7):
//! its kept rows, and through its miner's task the definitions of every
//! template they reference.

use std::collections::BTreeSet;

use ourios_core::tenant::TenantId;

use super::super::super::mining::tests::{batch_at, corpus, time};
use super::GROUP;
use super::mining::commit_all;
use super::read_index::{PATIENCE, reading};

/// A tail replica that has mined through the group's last entry keeps the
/// tenant's rows, and defines every template version they use: a version
/// a later widening replaced included, which only its kept audit events
/// still bind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tail_replica_defines_every_template_its_kept_rows_use() {
    let (cluster, leader, _) = reading().await;
    let lines = corpus();
    let more: Vec<_> = (11..)
        .zip(lines[10..].chunks(5))
        .map(|(n, chunk)| batch_at(time(n), "acme", chunk))
        .collect();
    let last = commit_all(&cluster, leader, &more).await;
    let tail = &cluster.node(3).host;
    let mut through = tail.mined_through(GROUP).expect("a tail replica mines");
    tokio::time::timeout(PATIENCE, through.wait_for(|mined| *mined >= Some(last)))
        .await
        .expect("mined in time")
        .expect("still mining");
    let tenant = TenantId::new("acme");

    let wanted: BTreeSet<_> = tail
        .read_kept(GROUP, |kept| {
            kept.rows()
                .filter(|row| row.record.tenant_id == tenant)
                .map(|row| (row.record.template_id, row.record.template_version))
                .collect()
        })
        .expect("a tail replica keeps");
    let widened = wanted
        .iter()
        .any(|&(id, version)| wanted.contains(&(id, version + 1)));
    assert!(widened, "rows of a template before and after a widening");
    let defined = tail
        .definitions(GROUP, &tenant, wanted.clone())
        .await
        .expect("its miner answers");

    assert_eq!(defined.keys().copied().collect::<BTreeSet<_>>(), wanted);
    assert!(
        cluster.node(0).host.read_kept(GROUP, |_| ()).is_none(),
        "a voter keeps nothing"
    );
    cluster.shutdown().await;
}
