//! Tests a later behaviour superseded, kept `#[ignore]`d as the record of
//! what they pinned; each names the test that replaces it.

use super::*;

const NOT_AVAILABLE: &str =
    "receiver.wal.backend \"replicated\" is not available yet (RFC 0065); use \"local\"";

#[test]
#[ignore = "superseded: the replicated backend starts, so its pre-flight passes a valid config: a_valid_replicated_config_passes_the_preflight_writing_nothing"]
fn only_the_local_wal_backend_starts() {
    let store = ourios_parquet::Store::in_memory();
    let wal_root = tempfile::TempDir::new().expect("temp");
    assert!(check_wal_backend(&WalBackend::Local, &store, wal_root.path()).is_ok());
    assert!(
        store
            .get_blocking_opt(layout::LAYOUT_KEY)
            .expect("read")
            .is_none()
    );
    assert!(!wal_root.path().join(replica_id::REPLICA_ID_FILE).exists());
    let wal = replicated_wal(THREE_VOTERS);
    assert!(matches!(wal, WalBackend::Replicated(_)));
    let refusal = check_wal_backend(&wal, &store, wal_root.path())
        .expect_err("the replicated backend refuses");
    assert_eq!(startup_exit(refusal), Err(NOT_AVAILABLE.to_owned()));
}

/// RFC0065.1, the layout arm: a node whose `peers`, `groups` or
/// `replication_factor` differs from the recorded layout fails startup
/// naming the field, while a change to the tail pool does not (B24).
/// The store is the in-memory one: the replicated backend needs
/// conditional updates, which the local store the served-binary tests
/// run on lacks.
#[test]
#[ignore = "superseded: a refused start no longer records the layout, so the mismatch is \
                checked against a pre-recorded one in \
                rfc0065_1_a_recorded_layout_mismatch_fails_naming_the_field_and_writes_nothing, \
                #1250"]
fn rfc0065_1_a_layout_mismatch_fails_naming_the_field() {
    let store = ourios_parquet::Store::in_memory();
    let wal_root = tempfile::TempDir::new().expect("temp");
    let start = |leaves: &str| {
        startup_exit(
            check_wal_backend(&replicated_wal(leaves), &store, wal_root.path())
                .expect_err("the replicated backend refuses"),
        )
    };
    assert_eq!(start(THREE_VOTERS), Err(NOT_AVAILABLE.to_owned()));
    let tail_changed = "      peers: c=c:4321,b=b:4320,a=a:4320
      tail_peers: t=t:4320,u=u:4320
      tail_replicas: 2
      tail_clients: querier,other
";
    assert_eq!(start(tail_changed), Err(NOT_AVAILABLE.to_owned()));
    let mismatches = [
        (
            "      peers: a=a:4320,b=b:4320,d=d:4320\n",
            "receiver.wal.replicated.peers",
            "recorded {\"a\", \"b\", \"c\"}, configured {\"a\", \"b\", \"d\"}",
        ),
        (
            "      peers: a=a:4320,b=b:4320,c=c:4320\n      groups: 32\n",
            "receiver.wal.replicated.groups",
            "recorded 16, configured 32",
        ),
        (
            "      peers: a=a:4320,b=b:4320,c=c:4320,d=d:4320,e=e:4320\n      replication_factor: 5\n",
            "receiver.wal.replicated.replication_factor",
            "recorded 3, configured 5",
        ),
    ];
    for (voters, field, values) in mismatches {
        let leaves = format!("{voters}      tail_peers: t=t:4320\n      tail_clients: querier\n");
        let error = start(&leaves).expect_err("a mismatch fails startup");
        assert!(error.contains(&format!("{field} differs")), "{error}");
        assert!(error.contains(values), "{error}");
    }
}

/// A refused replicated start leaves the store and `wal_root` as it
/// found them: no layout record, no identity, no vote log (#1250).
#[test]
#[ignore = "superseded: a valid replicated config is no longer refused; that its pre-flight writes nothing is a_valid_replicated_config_passes_the_preflight_writing_nothing"]
fn a_refused_replicated_start_writes_nothing() {
    let store = ourios_parquet::Store::in_memory();
    let wal_root = tempfile::TempDir::new().expect("temp");
    let refusal = check_wal_backend(&replicated_wal(THREE_VOTERS), &store, wal_root.path())
        .expect_err("the replicated backend refuses");
    assert_eq!(startup_exit(refusal), Err(NOT_AVAILABLE.to_owned()));
    let keys = store.list_blocking(None).expect("list");
    assert!(keys.is_empty(), "the store was written: {keys:?}");
    let entries = wal_root_entries(wal_root.path());
    assert!(entries.is_empty(), "wal_root was written: {entries:?}");
}

/// RFC0065.1, the layout arm, against a layout recorded before: a
/// difference in `peers`, `groups` or `replication_factor` fails naming
/// the field, a tail-pool change does not (B24), and no start writes.
#[test]
#[ignore = "superseded: a matching layout passes the pre-flight: rfc0065_1_a_recorded_layout_mismatch_fails_the_preflight_naming_the_field"]
fn rfc0065_1_a_recorded_layout_mismatch_fails_naming_the_field_and_writes_nothing() {
    let store = ourios_parquet::Store::in_memory();
    let recorded = record_three_voters(&store);
    let wal_root = tempfile::TempDir::new().expect("temp");
    let start = |leaves: &str| {
        startup_exit(
            check_wal_backend(&replicated_wal(leaves), &store, wal_root.path())
                .expect_err("the replicated backend refuses"),
        )
    };
    assert_eq!(start(THREE_VOTERS), Err(NOT_AVAILABLE.to_owned()));
    let tail_changed = "      peers: c=c:4321,b=b:4320,a=a:4320
      tail_peers: t=t:4320,u=u:4320
      tail_replicas: 2
      tail_clients: querier,other
";
    assert_eq!(start(tail_changed), Err(NOT_AVAILABLE.to_owned()));
    let mismatches = [
        ("peers: a=a:4320,b=b:4320,d=d:4320\n", "peers"),
        (
            "peers: a=a:4320,b=b:4320,c=c:4320\n      groups: 32\n",
            "groups",
        ),
        (
            "peers: a=a:4320,b=b:4320,c=c:4320,d=d:4320,e=e:4320\n      replication_factor: 5\n",
            "replication_factor",
        ),
    ];
    for (voters, field) in mismatches {
        let leaves =
            format!("      {voters}      tail_peers: t=t:4320\n      tail_clients: querier\n");
        let error = start(&leaves).expect_err("a mismatch fails startup");
        let named = format!("receiver.wal.replicated.{field} differs");
        assert!(error.contains(&named), "{error}");
    }
    assert_eq!(
        store.list_blocking(None).expect("list"),
        [layout::LAYOUT_KEY]
    );
    assert_eq!(
        store.get_blocking(layout::LAYOUT_KEY).expect("kept"),
        recorded
    );
    let entries = wal_root_entries(wal_root.path());
    assert!(entries.is_empty(), "wal_root was written: {entries:?}");
}

/// A refused start reads an existing `replica.id` without touching it,
/// and still refuses one it cannot read (RFC 0065 §6.1).
#[test]
#[ignore = "superseded: a valid replicated config passes the pre-flight: the_preflight_keeps_an_existing_replica_id_and_refuses_an_unreadable_one"]
fn a_refused_replicated_start_keeps_an_existing_replica_id() {
    let store = ourios_parquet::Store::in_memory();
    let wal_root = tempfile::TempDir::new().expect("temp");
    replica_id::load_or_create(wal_root.path()).expect("an earlier identity");
    let path = wal_root.path().join(replica_id::REPLICA_ID_FILE);
    let first = std::fs::read(&path).expect("created");
    let before = wal_root_entries(wal_root.path());
    let wal = replicated_wal(THREE_VOTERS);
    let refusal = check_wal_backend(&wal, &store, wal_root.path()).expect_err("refused");
    assert_eq!(startup_exit(refusal), Err(NOT_AVAILABLE.to_owned()));
    assert_eq!(std::fs::read(&path).expect("kept"), first);
    assert_eq!(wal_root_entries(wal_root.path()), before);
    std::fs::write(&path, b"garbage").expect("corrupt");
    let error =
        startup_exit(check_wal_backend(&wal, &store, wal_root.path()).expect_err("refused"))
            .expect_err("a failure");
    assert!(error.contains(&path.display().to_string()), "{error}");
    assert_eq!(std::fs::read(&path).expect("kept"), b"garbage");
    assert_eq!(wal_root_entries(wal_root.path()), before);
}

/// The replicated backend keeps its identity in `wal_root/replica.id`
/// across starts and refuses one it cannot read (RFC 0065 §6.1).
#[test]
#[ignore = "superseded: a refused start no longer creates replica.id, so an existing one is \
                checked in a_refused_replicated_start_keeps_an_existing_replica_id, #1250"]
fn the_replicated_backend_keeps_its_replica_id() {
    let store = ourios_parquet::Store::in_memory();
    let wal_root = tempfile::TempDir::new().expect("temp");
    let path = wal_root.path().join(replica_id::REPLICA_ID_FILE);
    let wal = replicated_wal(THREE_VOTERS);
    check_wal_backend(&wal, &store, wal_root.path()).expect_err("not available yet");
    let first = std::fs::read(&path).expect("created");
    check_wal_backend(&wal, &store, wal_root.path()).expect_err("not available yet");
    assert_eq!(std::fs::read(&path).expect("kept"), first);
    std::fs::write(&path, b"garbage").expect("corrupt");
    let error =
        startup_exit(check_wal_backend(&wal, &store, wal_root.path()).expect_err("refused"))
            .expect_err("a failure");
    assert!(error.contains(&path.display().to_string()), "{error}");
    assert_eq!(std::fs::read(&path).expect("kept"), b"garbage");
}
