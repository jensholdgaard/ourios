//! RFC 0009 §3.4 erasure note (RFC0009.20–.23): an erasure covers late
//! files, keeps the current hour writable, confirms by partition generation
//! before advancing, and waits `erasure_settle` before its rows phase.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use ourios_core::record::MinedRecord;
use ourios_parquet::{PartitionKey, Store, Writer, compact_partition};

use super::tests::{HOUR, TS0, rec, store_at};
use super::*;

/// Well past hour 10's end + grace.
const NOW_SEALED: u64 = TS0 + 2 * HOUR;

/// The conversation an erasure in these tests targets: template 1's rows.
fn erase_template_1(record: &MinedRecord, _: &str) -> bool {
    record.template_id == 1
}

fn partition_of(ts_ns: u64) -> PartitionKey {
    PartitionKey::derive(&rec("a", 1, ts_ns)).expect("derive")
}

/// Write `rows` as one flush file; its key.
fn flush(store: &Store, rows: &[MinedRecord]) -> String {
    let partition = PartitionKey::derive(&rows[0]).expect("derive");
    let mut writer = Writer::open_in(store, partition).expect("open writer");
    writer.append_records(rows).expect("append");
    writer.close().expect("close").key
}

/// The template ids `partition` reads, sorted.
fn templates(store: &Store, partition: &PartitionKey) -> Vec<u64> {
    let mut ids = Vec::new();
    ourios_parquet::compaction::visit_partition_rows(store, partition, |batch| {
        ids.extend(batch.iter().map(|r| r.template_id));
    })
    .expect("visit");
    ids.sort_unstable();
    ids
}

/// One sweep with `matcher` as the erasure match, at `clock`.
fn sweep_at(
    store: &Store,
    clock: SweepClock,
    policy: &CompactionPolicy,
    matcher: &ErasureMatch<'_>,
) -> SweepReport {
    let mut hooks = SweepHooks {
        observe: None,
        erasure_match: Some(matcher),
        gauges: None,
    };
    run_sweep_committing(
        store,
        clock,
        policy,
        &PromotedAttributes::default(),
        &mut hooks,
        &mut |_: &PartitionCommitted<'_>| {},
    )
    .expect("sweep")
}

fn sweep(store: &Store, matcher: &ErasureMatch<'_>) -> SweepReport {
    sweep_at(
        store,
        SweepClock::sealed_at(NOW_SEALED),
        &CompactionPolicy::default(),
        matcher,
    )
}

fn phases(report: &SweepReport) -> Vec<ErasurePhase> {
    report.erasures.iter().map(|o| o.phase).collect()
}

/// RFC0009.20 — a late file holding the conversation's rows, landed in a
/// compacted partition before the erasure, is rewritten and consumed.
#[test]
fn rfc0009_20_erasure_rewrites_a_late_file_that_predates_it() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let partition = partition_of(TS0);
    flush(&store, &[rec("a", 2, TS0)]);
    flush(&store, &[rec("a", 3, TS0 + 1_000)]);
    compact_partition(&store, &partition).expect("compact");
    let late = flush(&store, &[rec("a", 1, TS0 + 2_000)]);
    request_erasure(&store, "a", "c-1").expect("request");

    let report = sweep(&store, &erase_template_1);

    assert_eq!(phases(&report), [ErasurePhase::Tuples], "{report:?}");
    assert_eq!(
        templates(&store, &partition),
        [2, 3],
        "the late row is erased"
    );
    let manifest = ourios_parquet::Manifest::read_with_etag(
        &store,
        &format!(
            "{}/manifest.json",
            late.rsplit_once('/').expect("partition key").0
        ),
    )
    .expect("read")
    .expect("manifest")
    .0;
    let late_name = late.rsplit_once('/').expect("partition key").1;
    assert!(
        manifest.consumed.iter().any(|n| n == late_name),
        "the late file is consumed: {manifest:?}"
    );
}

/// RFC0009.21 — an erasure rewrites the current, unsealed hour too; a flush
/// into that hour afterwards is read.
#[test]
fn rfc0009_21_a_flush_after_an_erasure_rewrite_is_read() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let current = TS0 + 62 * 60 * 1_000_000_000;
    let partition = partition_of(current);
    flush(
        &store,
        &[rec("a", 1, current), rec("a", 2, current + 1_000)],
    );
    request_erasure(&store, "a", "c-1").expect("request");
    let report = sweep(&store, &erase_template_1);
    assert_eq!(phases(&report), [ErasurePhase::Tuples], "{report:?}");

    flush(&store, &[rec("a", 4, current + 2_000)]);

    assert_eq!(
        templates(&store, &partition),
        [2, 4],
        "the new flush is read"
    );
}

/// RFC0009.22 — a flush of the conversation's rows that lands while the rows
/// phase runs is caught by the confirming pass.
#[test]
fn rfc0009_22_confirming_pass_rewrites_a_flush_during_the_rows_phase() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let partition = partition_of(TS0);
    flush(&store, &[rec("a", 1, TS0), rec("a", 2, TS0 + 1_000)]);
    request_erasure(&store, "a", "c-1").expect("request");
    let landed = AtomicBool::new(false);
    let matcher = |record: &MinedRecord, id: &str| {
        if !landed.swap(true, Ordering::SeqCst) {
            flush(&store, &[rec("a", 1, TS0 + 5_000)]);
        }
        erase_template_1(record, id)
    };

    let report = sweep(&store, &matcher);

    assert_eq!(phases(&report), [ErasurePhase::Tuples], "{report:?}");
    assert_eq!(
        report.erasures[0].partitions_rewritten, 2,
        "rewritten again"
    );
    assert_eq!(
        templates(&store, &partition),
        [2],
        "no row of the conversation"
    );
}

/// RFC0009.22 — a second compactor folds a late file of the conversation
/// into a new compacted file after the erasure's commit: the confirming pass
/// sees the generation move and rewrites the partition again.
#[test]
fn rfc0009_22_confirming_pass_catches_a_consolidation_since_the_erasure() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let partition = partition_of(TS0);
    flush(&store, &[rec("a", 1, TS0), rec("a", 2, TS0 + 1_000)]);
    request_erasure(&store, "a", "c-1").expect("request");
    let mut peer_ran = false;
    let mut on_commit = |_: &PartitionCommitted<'_>| {
        if !peer_ran {
            peer_ran = true;
            flush(&store, &[rec("a", 1, TS0 + 5_000)]);
            compact_partition(&store, &partition).expect("the peer consolidates");
        }
    };
    let mut hooks = SweepHooks {
        observe: None,
        erasure_match: Some(&erase_template_1),
        gauges: None,
    };

    let report = run_sweep_committing(
        &store,
        SweepClock::sealed_at(NOW_SEALED),
        &CompactionPolicy::default(),
        &PromotedAttributes::default(),
        &mut hooks,
        &mut on_commit,
    )
    .expect("sweep");

    assert_eq!(phases(&report), [ErasurePhase::Tuples], "{report:?}");
    assert_eq!(
        templates(&store, &partition),
        [2],
        "no row of the conversation"
    );
}

/// RFC0009.22 — a partition holding only a `*.parquet.tmp` object has
/// nothing to rewrite; it is checked at generation 0 and does not hold the
/// marker back.
#[test]
fn rfc0009_22_a_tmp_only_partition_does_not_block_the_marker() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    flush(&store, &[rec("a", 1, TS0), rec("a", 2, TS0 + 1_000)]);
    let other = TS0 + 3 * HOUR;
    let prefix = format!("{}", partition_of(other).data_path(Path::new("")).display());
    store
        .put_blocking(&format!("{prefix}/stray.parquet.tmp"), b"x".to_vec())
        .expect("tmp");
    request_erasure(&store, "a", "c-1").expect("request");

    let report = sweep(&store, &erase_template_1);

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(phases(&report), [ErasurePhase::Tuples], "{report:?}");
}

/// RFC0009.22 — while flushes of the conversation keep arriving, the rows
/// phase stops after the bounded confirming passes and keeps the marker.
#[test]
fn rfc0009_22_rows_phase_stays_open_while_flushes_keep_arriving() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let partition = partition_of(TS0);
    flush(&store, &[rec("a", 1, TS0), rec("a", 2, TS0 + 1_000)]);
    request_erasure(&store, "a", "c-1").expect("request");
    let next = AtomicU64::new(10_000);
    let matcher = |record: &MinedRecord, id: &str| {
        let matched = erase_template_1(record, id);
        if matched {
            let ts = TS0 + next.fetch_add(1_000, Ordering::SeqCst);
            flush(&store, &[rec("a", 1, ts)]);
        }
        matched
    };

    let report = sweep(&store, &matcher);

    assert_eq!(phases(&report), [ErasurePhase::Rows], "{report:?}");
    assert!(
        report.errors.iter().any(|e| e.contains("still changing")),
        "{:?}",
        report.errors
    );
    assert!(!templates(&store, &partition).is_empty());
}

/// A store backend that runs `before` once, just before the erasure marker
/// is rewritten to the `tuples` phase.
#[derive(Debug)]
struct BeforeAdvance {
    inner: Arc<dyn object_store::ObjectStore>,
    before: Arc<Mutex<Option<PathBuf>>>,
    bytes: Vec<u8>,
}

impl std::fmt::Display for BeforeAdvance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BeforeAdvance({})", self.inner)
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for BeforeAdvance {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        if location.as_ref().starts_with("erasure/") {
            let target = self
                .before
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(target) = target {
                std::fs::write(target, &self.bytes).expect("publish the late flush");
            }
        }
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// RFC0009.22, the residual accepted under decision D3: a flush of a row
/// acknowledged before the request that publishes after the last confirming
/// pass, just before the marker advances, is not erased.
#[test]
fn rfc0009_22_residual_a_publish_after_the_last_pass_is_not_erased() {
    let bucket = tempfile::tempdir().expect("temp");
    let plain = store_at(bucket.path());
    let partition = partition_of(TS0);
    flush(&plain, &[rec("a", 1, TS0), rec("a", 2, TS0 + 1_000)]);
    request_erasure(&plain, "a", "c-1").expect("request");
    let staged = flush(&plain, &[rec("a", 1, TS0 + 9_000)]);
    let bytes = std::fs::read(bucket.path().join(&staged)).expect("staged bytes");
    std::fs::remove_file(bucket.path().join(&staged)).expect("unstage");
    let target = bucket.path().join(&staged);
    let store = plain.wrap_backend(|inner| {
        Arc::new(BeforeAdvance {
            inner,
            before: Arc::new(Mutex::new(Some(target))),
            bytes,
        })
    });

    let report = sweep(&store, &erase_template_1);

    assert_eq!(phases(&report), [ErasurePhase::Tuples], "{report:?}");
    assert_eq!(
        templates(&store, &partition),
        [1, 2],
        "the late-published row stays (the accepted residual)"
    );
}

/// Far past any marker this test suite writes.
fn far_future() -> u64 {
    u64::MAX / 2
}

/// RFC0009.23 — the rows phase waits until the marker is older than
/// `erasure_settle`, by the store's clock against the sweep's.
#[test]
fn rfc0009_23_rows_phase_waits_for_erasure_settle() {
    let bucket = tempfile::tempdir().expect("temp");
    let store = store_at(bucket.path());
    let partition = partition_of(TS0);
    flush(&store, &[rec("a", 1, TS0), rec("a", 2, TS0 + 1_000)]);
    request_erasure(&store, "a", "c-1").expect("request");
    let policy = CompactionPolicy {
        erasure_settle_nanos: HOUR,
        ..CompactionPolicy::default()
    };

    let young = sweep_at(
        &store,
        SweepClock::sealed_at(NOW_SEALED),
        &policy,
        &erase_template_1,
    );
    let still_rows = pending_erasures(&store).expect("pending");
    let settled_clock = SweepClock {
        sealed_at: NOW_SEALED,
        now: far_future,
    };
    let old = sweep_at(&store, settled_clock, &policy, &erase_template_1);

    assert_eq!(young.erasures_deferred.len(), 1, "{young:?}");
    assert!(young.erasures.is_empty(), "nothing rewritten while young");
    assert_eq!(still_rows[0].phase, ErasurePhase::Rows);
    assert_eq!(phases(&old), [ErasurePhase::Tuples], "{old:?}");
    assert_eq!(templates(&store, &partition), [2]);
}

/// A file another process writes: its path and its bytes.
type PeerWrite = (PathBuf, Vec<u8>);

/// A store backend that runs `change` once, just before the `nth` listing of
/// a prefix containing `scope` is answered.
#[derive(Debug)]
struct OnListing {
    inner: Arc<dyn object_store::ObjectStore>,
    scope: String,
    nth: u64,
    seen: AtomicU64,
    change: Mutex<Option<Vec<PeerWrite>>>,
}

impl std::fmt::Display for OnListing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OnListing({})", self.inner)
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for OnListing {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        let in_scope = prefix.is_some_and(|p| p.as_ref().contains(&self.scope));
        if in_scope && self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.nth {
            let writes = self
                .change
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            for (path, bytes) in writes.into_iter().flatten() {
                std::fs::write(path, bytes).expect("the peer's write");
            }
        }
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// RFC0009.22 — a partition with nothing to rewrite (only a `*.parquet.tmp`)
/// is checked at the generation seen before its rewrite. A peer that then
/// publishes the conversation's rows there and consolidates them, before the
/// rows phase reads the partition's generation, moves the partition past
/// that generation, so the confirming pass rewrites it and the rows go.
#[test]
fn rfc0009_22_a_peer_commit_after_a_no_op_rewrite_is_still_erased() {
    let bucket = tempfile::tempdir().expect("temp");
    let plain = store_at(bucket.path());
    flush(&plain, &[rec("a", 2, TS0)]);
    let other_ts = TS0 + 3 * HOUR;
    let other = partition_of(other_ts);
    let prefix = format!("{}", other.data_path(Path::new("")).display());
    plain
        .put_blocking(&format!("{prefix}/stray.parquet.tmp"), b"x".to_vec())
        .expect("tmp");
    let scratch = tempfile::tempdir().expect("scratch");
    let staged = flush(&store_at(scratch.path()), &[rec("a", 1, other_ts)]);
    let output = staged.replace(".parquet", ".g1.parquet");
    let bytes = std::fs::read(scratch.path().join(&staged)).expect("peer output");
    let manifest = ourios_parquet::Manifest::new(
        1,
        vec![output.rsplit_once('/').expect("key").1.to_owned()],
        Vec::new(),
    );
    let change = vec![
        (bucket.path().join(&output), bytes),
        (
            bucket.path().join(&prefix).join("manifest.json"),
            manifest.to_json().expect("json"),
        ),
    ];
    request_erasure(&plain, "a", "c-1").expect("request");
    let store = plain.wrap_backend(|inner| {
        Arc::new(OnListing {
            inner,
            scope: prefix.clone(),
            nth: 2,
            seen: AtomicU64::new(0),
            change: Mutex::new(Some(change)),
        })
    });

    let report = sweep(&store, &erase_template_1);

    assert_eq!(phases(&report), [ErasurePhase::Tuples], "{report:?}");
    assert!(
        !templates(&store, &other).contains(&1),
        "the peer's row of the conversation is erased"
    );
}
