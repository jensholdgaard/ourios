//! What a running tail replica serves of what it keeps (RFC 0065 §6.7):
//! its kept rows, and through its miner's task the definitions of every
//! template they reference.

use std::collections::BTreeSet;

use ourios_core::tenant::TenantId;

use super::GROUP;
use super::read_index::{PATIENCE, reading};

/// A tail replica that has mined through the group's last entry keeps the
/// tenant's rows, and its miner defines every template version they use.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tail_replica_defines_every_template_its_kept_rows_use() {
    let (cluster, _, last) = reading().await;
    let tail = &cluster.node(3).host;
    let mut through = tail.mined_through(GROUP).expect("a tail replica mines");
    tokio::time::timeout(PATIENCE, through.wait_for(|mined| *mined >= Some(last)))
        .await
        .expect("mined in time")
        .expect("still mining");
    let tenant = TenantId::new("acme");

    let kept = tail.kept(GROUP).expect("a tail replica keeps");
    let wanted: BTreeSet<_> = kept
        .lock()
        .expect("kept")
        .rows()
        .filter(|row| row.record.tenant_id == tenant)
        .map(|row| (row.record.template_id, row.record.template_version))
        .collect();
    assert!(!wanted.is_empty(), "rows kept");
    let defined = tail
        .definitions(GROUP, &tenant, wanted.clone())
        .await
        .expect("its miner answers");

    assert_eq!(defined.keys().copied().collect::<BTreeSet<_>>(), wanted);
    assert!(
        cluster.node(0).host.kept(GROUP).is_none(),
        "a voter keeps nothing"
    );
    cluster.shutdown().await;
}
