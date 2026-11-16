use std::time::Instant;

use ourios_parquet::Store;

use super::*;
use crate::replicated::raft::bundle::tests::{marked, sample};
use crate::replicated::raft::bundle::{bundle_key, pointer, snapshot_key};
use crate::replicated::raft::kept::tests::{entries, mined_log};
use crate::replicated::raft::publication::tests::{Stores, fail_audit, kept_of, published, stores};
use crate::replicated::raft::telemetry::tests::Reader as Metrics;

const GROUP: u32 = 9;
const CHECKPOINT_DURATION: &str = ourios_semconv::OURIOS_WAL_REPLICATION_CHECKPOINT_DURATION;

fn captured() -> Captured {
    Captured {
        bundle: sample(),
        snapshots: vec![marked("acme"), marked("beta")],
    }
}

/// `stores`' publication with ten entries' rows buffered, unpublished.
fn buffered(stores: &Stores) {
    let kept = kept_of(&mined_log(&entries(10)));
    stores.publication.publish_kept(&kept);
    assert!(
        published(&stores.data).is_empty(),
        "held back until the cut"
    );
}

fn run(stores: &Stores, store: &Store, metrics: &Metrics) -> Result<Published, Failed> {
    let checkpoints = Checkpoints::on(&metrics.meter());
    Checkpoint {
        group: GROUP,
        publication: &stores.publication,
        store,
        checkpoints: &checkpoints,
    }
    .run(
        &captured(),
        (Instant::now(), stores.publication.generation()),
    )
}

fn bundle_written(store: &Store) -> bool {
    let cut = sample().cut();
    store
        .get_blocking_opt(&bundle_key(cut.log, cut.seq))
        .expect("read")
        .is_some()
}

/// RFC 0065 §6.5: the barrier publishes everything buffered through the
/// cut, then writes the cut's bundle and `checkpoint.json`, and records
/// one duration with no `error.type`.
#[test]
fn a_checkpoint_publishes_what_is_buffered_then_writes_its_bundle() {
    let (stores, store, metrics) = (stores(), Store::in_memory(), Metrics::new());
    buffered(&stores);

    let published_cut = run(&stores, &store, &metrics);

    assert!(
        matches!(published_cut, Ok(Published::Advanced)),
        "{published_cut:?}"
    );
    assert_eq!(published(&stores.data).len(), 50, "every buffered row");
    assert!(bundle_written(&store));
    let cut = sample().cut();
    let object = store
        .get_blocking_opt(&pointer::key(cut.log))
        .expect("read")
        .expect("checkpoint.json");
    assert_eq!(pointer::decode(&object, cut.log), Ok(cut));
    assert_eq!(
        metrics.counts(CHECKPOINT_DURATION),
        [(vec![GROUP.to_string()], 1)]
    );
}

/// A publish that fails writes no bundle, since the bundle may only name
/// a cut whose rows are all published; it records `store`.
#[test]
fn a_failed_publish_writes_no_bundle() {
    let (stores, store, metrics) = (stores(), Store::in_memory(), Metrics::new());
    buffered(&stores);
    fail_audit(&stores);

    let failed = run(&stores, &store, &metrics);

    assert!(matches!(failed, Err(Failed::Publish)), "{failed:?}");
    assert!(!bundle_written(&store));
    assert_eq!(
        metrics.counts(CHECKPOINT_DURATION),
        [(vec!["store".to_owned(), GROUP.to_string()], 1)]
    );
}

/// A bundle object already holding other bytes fails the checkpoint
/// with `object_differs`.
#[test]
fn a_failed_bundle_write_records_its_error_type() {
    let (stores, store, metrics) = (stores(), Store::in_memory(), Metrics::new());
    let cut = sample().cut();
    store
        .put_blocking(
            &snapshot_key(
                cut.log,
                cut.seq,
                &ourios_core::tenant::TenantId::new("acme"),
            ),
            b"other".to_vec(),
        )
        .expect("put");

    let failed = run(&stores, &store, &metrics);

    assert!(
        matches!(failed, Err(Failed::Write(WriteError::Differs { .. }))),
        "{failed:?}"
    );
    assert_eq!(
        metrics.counts(CHECKPOINT_DURATION),
        [(vec!["object_differs".to_owned(), GROUP.to_string()], 1)]
    );
}

/// Each failure's `error.type` is one the registry documents.
#[test]
fn each_failure_names_a_documented_error_type() {
    let store_error = || WriteError::Store {
        key: "k".to_owned(),
        source: ourios_parquet::StoreError::Config("down".to_owned()),
    };
    let cases = [
        (Failed::Publish, "store"),
        (Failed::Discarded, "_OTHER"),
        (Failed::Write(store_error()), "store"),
        (
            Failed::Write(WriteError::Differs {
                key: "k".to_owned(),
            }),
            "object_differs",
        ),
        (
            Failed::Write(WriteError::Unreadable(pointer::PointerError::Format(2))),
            "pointer_unreadable",
        ),
        (
            Failed::Write(WriteError::NoCompareAndSwap),
            "no_compare_and_swap",
        ),
        (Failed::Write(WriteError::Untagged), "_OTHER"),
    ];
    for (failed, error_type) in cases {
        assert_eq!(failed.error_type(), error_type, "{failed:?}");
    }
}

/// A publication discarded after the cut, as a replay does when a later
/// step overflows what is kept, may have dropped rows through the cut, so
/// the checkpoint writes no bundle even once rows are buffered again.
#[test]
fn a_publication_discarded_after_the_cut_writes_no_bundle() {
    let (stores, store, metrics) = (stores(), Store::in_memory(), Metrics::new());
    buffered(&stores);
    let at_cut = stores.publication.generation();
    stores.publication.discard();
    stores.publication.resume();
    buffered(&stores);
    let checkpoints = Checkpoints::on(&metrics.meter());

    let failed = Checkpoint {
        group: GROUP,
        publication: &stores.publication,
        store: &store,
        checkpoints: &checkpoints,
    }
    .run(&captured(), (Instant::now(), at_cut));

    assert!(matches!(failed, Err(Failed::Discarded)), "{failed:?}");
    assert!(!bundle_written(&store));
    assert_eq!(
        metrics.counts(CHECKPOINT_DURATION),
        [(vec!["_OTHER".to_owned(), GROUP.to_string()], 1)]
    );
}
