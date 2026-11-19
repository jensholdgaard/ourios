//! What a running tail replica serves of what it keeps (RFC 0065 §6.7):
//! its kept rows, and through its miner's task the definitions of every
//! template they reference.

use std::collections::BTreeSet;
use std::sync::Arc;

use ourios_core::record::{IngestPosition, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_parquet::ipc;
use ourios_parquet::record_batch::mined_records_to_batch;

use super::super::super::kept::Kept;
use super::super::super::mining::tests::{batch_at, block, corpus, time};
use super::super::super::reads::Reads;
use super::super::super::tailing::TailReads;
use super::super::super::telemetry::Meters;
use super::super::super::transport::tail::{
    Refusal, ScanQuery, TailPoint, TailQuery, TailTarget as _,
};
use super::super::Shape;
use super::mining::{commit_all, with_learners};
use super::publication::{LOG, formed_store, publishing};
use super::read_index::{PATIENCE, reading};
use super::{Cluster, GROUP};

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

/// A group of three voters and two tail replicas, its store formed under
/// [`LOG`] and no flusher assigned, so no checkpoint moves its tail; two
/// tenants' batches committed. The cluster and the last committed index.
async fn serving() -> (Cluster, u64) {
    let shape = Shape {
        publishing: vec![
            Arc::new(publishing(formed_store())),
            Arc::new(publishing(formed_store())),
        ],
        ..Shape::from((3, 2))
    };
    let (cluster, _mined) = Cluster::start_mining(shape, &[GROUP], |_| Meters::global()).await;
    for i in 0..5 {
        let host = &cluster.node(i).host;
        host.server()
            .serve_read_indexes(Arc::new(Reads::new(host)) as _);
    }
    let leader = with_learners(&cluster, 3..5).await;
    let mut entries = vec![block(0, 10_000)];
    let lines = corpus();
    entries.extend(lines[..40].chunks(5).enumerate().map(|(n, chunk)| {
        let tenant = if n % 2 == 0 { "acme" } else { "globex" };
        batch_at(time(n as u64 + 1), tenant, chunk)
    }));
    let last = commit_all(&cluster, leader, &entries).await;
    (cluster, last)
}

/// The rows `tail`'s replica keeps for `tenant`, as served: under [`LOG`].
fn kept_rows(cluster: &Cluster, tail: usize, tenant: &TenantId) -> Vec<MinedRecord> {
    let rows = |kept: &Kept| {
        kept.rows()
            .filter(|row| &row.record.tenant_id == tenant)
            .map(|row| MinedRecord {
                ingest_position: Some(IngestPosition {
                    group: LOG,
                    seq: row.position.seq,
                    index: row.position.index,
                }),
                ..row.record.clone()
            })
            .collect()
    };
    cluster
        .node(tail)
        .host
        .read_kept(GROUP, rows)
        .expect("kept")
}

fn acme_scan(
    read: u64,
    after: Option<super::super::super::transport::tail::WirePosition>,
) -> ScanQuery {
    ScanQuery {
        group: GROUP,
        tenant: "acme".to_owned(),
        from: 0,
        read,
        range: None,
        after,
    }
}

/// A page bound that holds a few of the corpus' rows: each page's stream
/// carries the record schema, about 6.5 KiB, as well as its rows.
const PAGE: usize = 8 * 1024;

/// RFC 0065 §6.7 on a running group: a tail replica fixes a confirmed
/// read point, then serves the tenant's unpublished rows at it a page at
/// a time, each row under the group's log id and each page with the
/// definitions of exactly the templates its rows reference.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tail_replica_serves_a_tenants_rows_page_by_page() {
    let (cluster, last) = serving().await;
    let reads =
        TailReads::new(&cluster.node(4).host, cluster.peers().clone()).with_page_bytes(PAGE);
    let query = TailQuery {
        group: GROUP,
        tenant: "acme".to_owned(),
        from: 0,
    };

    let TailPoint { read, published } = reads.tail(query).await.expect("a read point");
    assert!(read >= last, "{read} is behind {last}");
    assert!(published.is_empty(), "nothing published yet");

    let expected = kept_rows(&cluster, 4, &TenantId::new("acme"));
    assert!(!kept_rows(&cluster, 4, &TenantId::new("globex")).is_empty());
    let (mut after, mut offset, mut pages) = (None, 0, 0);
    loop {
        let page = reads.scan(acme_scan(read, after)).await.expect("a page");
        pages += 1;
        let batches = ipc::decode(&page.rows).expect("an arrow stream");
        let [batch] = &batches[..] else {
            panic!("one batch a page: {}", batches.len());
        };
        let rows = &expected[offset..offset + batch.num_rows()];
        assert!(!rows.is_empty(), "a page holds rows");
        assert_eq!(batch, &mined_records_to_batch(rows).expect("batch"));
        offset += rows.len();
        let referenced: BTreeSet<_> = rows
            .iter()
            .map(|r| (r.template_id, r.template_version))
            .collect();
        let defined: BTreeSet<_> = page
            .templates
            .iter()
            .map(|t| (t.template_id, t.template_version))
            .collect();
        assert_eq!(defined, referenced, "page {pages}");
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert!(pages > 1, "{pages} page");
    assert_eq!(offset, expected.len(), "every row once");
    cluster.shutdown().await;
}

/// A voter mines nothing, so it serves no tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_voter_serves_no_tail() {
    let (cluster, _) = serving().await;
    let reads = TailReads::new(&cluster.node(0).host, cluster.peers().clone());
    let query = TailQuery {
        group: GROUP,
        tenant: "acme".to_owned(),
        from: 0,
    };
    assert_eq!(reads.tail(query).await, Err(Refusal::NotHosted));
    cluster.shutdown().await;
}
