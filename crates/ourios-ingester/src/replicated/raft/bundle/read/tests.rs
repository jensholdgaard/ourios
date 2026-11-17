use ourios_core::tenant::TenantId;
use ourios_parquet::Store;

use super::super::tests::{AT, marked, sample};
use super::super::write::write;
use super::super::{BundleError, Cut, CutId, bundle_key, snapshot_key};
use super::{ReadError, read};
use crate::replicated::raft::snapshot_record::IdBlock;

const TENANTS: [&str; 2] = ["acme", "beta"];

fn snapshots() -> Vec<Vec<u8>> {
    TENANTS.iter().map(|tenant| marked(tenant)).collect()
}

/// The snapshot record's view of the sample bundle's cut.
fn cut() -> Cut {
    let bundle = sample();
    Cut {
        id: AT,
        latest_block: bundle.ids().latest(),
        config: Some(bundle.tenants()[0].config),
    }
}

/// A store holding the sample bundle, as a flusher wrote it.
fn written() -> Store {
    let store = Store::in_memory();
    write(&store, &sample(), &snapshots()).expect("written");
    store
}

fn snap(tenant: &str) -> String {
    snapshot_key(AT.log, AT.seq, &TenantId::new(tenant))
}

/// RFC 0065 §6.5: a replica restores the bundle at its record's cut: the
/// bundle a flusher wrote, and every tenant's state in it.
#[test]
fn the_bundle_a_flusher_wrote_reads_back_with_every_tenants_state() {
    let store = written();

    let (bundle, states) = read(&store, &cut()).expect("read");

    assert_eq!(bundle, sample());
    assert_eq!(states.len(), TENANTS.len());
    for ((entry, bytes), state) in bundle.tenants().iter().zip(snapshots()).zip(&states) {
        assert_eq!(&bundle.snapshot(entry, &bytes).expect("admitted"), state);
    }
}

/// A bundle missing `bundle.bin`, or any `.snap` it names, is not read.
#[test]
fn a_bundle_missing_an_object_is_not_read() {
    for key in [bundle_key(AT.log, AT.seq), snap("beta")] {
        let store = written();
        store.delete_blocking(&key).expect("delete");

        let refused = read(&store, &cut());

        assert!(
            matches!(&refused, Err(ReadError::Missing { key: missing }) if *missing == key),
            "{key}: {refused:?}"
        );
        assert!(!refused.err().is_some_and(|e| e.retryable()));
    }
}

/// RFC0065.31: a bundle not of the record's cut, or a `.snap` other than
/// the one it names, fails the restore closed.
#[test]
fn a_bundle_of_another_cut_or_a_changed_snapshot_is_refused() {
    let store = written();
    let elsewhere = Cut {
        latest_block: IdBlock::new(0, 1),
        ..cut()
    };
    assert!(matches!(
        read(&store, &elsewhere),
        Err(ReadError::Refused(BundleError::LatestBlock { .. }))
    ));
    let later = Cut {
        id: CutId { seq: 300, ..AT },
        ..cut()
    };
    assert!(
        matches!(read(&store, &later), Err(ReadError::Missing { .. })),
        "a later cut's bundle is never written here"
    );

    store.delete_blocking(&snap("acme")).expect("delete");
    store
        .put_blocking(&snap("acme"), b"other".to_vec())
        .expect("put");
    assert!(matches!(
        read(&store, &cut()),
        Err(ReadError::Refused(BundleError::SnapshotLength { .. }))
    ));
}
