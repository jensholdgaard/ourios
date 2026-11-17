//! RFC 0065 §6.6: a start holding a group's Raft state, a log or a snapshot
//! record, opens its groups without the bootstrap lock; one holding none
//! takes it. A `wal_root` it cannot read fails the start.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use super::super::log_store::{group_dir, holds_segments};
use super::{RaftState, raft_state};

const GROUPS: [u32; 2] = [0, 3];

#[test]
fn a_node_with_no_group_state_holds_none() {
    let root = tempfile::tempdir().expect("tempdir");
    let absent = root.path().join("never-created");

    assert_eq!(raft_state(&absent, &GROUPS).ok(), Some(RaftState::Empty));
    assert_eq!(
        raft_state(root.path(), &GROUPS).ok(),
        Some(RaftState::Empty)
    );
    // A group directory with neither a segment nor a record holds nothing.
    fs::create_dir_all(group_dir(root.path(), 3)).expect("group dir");
    assert_eq!(
        raft_state(root.path(), &GROUPS).ok(),
        Some(RaftState::Empty)
    );
}

#[test]
fn a_group_log_or_snapshot_record_is_raft_state() {
    // The WAL reader takes a segment's extension in any case.
    for file in [
        "00000000000000000001.wal",
        "00000000000000000001.WAL",
        "snapshot",
    ] {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = group_dir(root.path(), 3);
        fs::create_dir_all(&dir).expect("group dir");
        fs::write(dir.join(file), b"").expect("state");

        assert_eq!(
            raft_state(root.path(), &GROUPS).ok(),
            Some(RaftState::Held),
            "{file}"
        );
    }
}

/// Only this node's groups count: another group's directory, as a node
/// placed elsewhere earlier would have left, is not this start's state.
#[test]
fn another_groups_state_is_not_this_nodes() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = group_dir(root.path(), 7);
    fs::create_dir_all(&dir).expect("group dir");
    fs::write(dir.join("snapshot"), b"").expect("state");

    assert_eq!(
        raft_state(root.path(), &GROUPS).ok(),
        Some(RaftState::Empty)
    );
}

/// RFC0065.24's unreadable-disk arm: a group directory that exists but
/// cannot be read fails the start, rather than reading as empty and
/// bootstrapping over state the node holds.
#[test]
fn an_unreadable_group_directory_is_an_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = group_dir(root.path(), 0);
    fs::create_dir_all(&dir).expect("group dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).expect("chmod");

    let read = raft_state(root.path(), &GROUPS);

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("restore");
    assert!(read.is_err(), "{read:?}");
}

/// Every group is read: an unreadable group after one holding state still
/// fails the start, whatever the order of the node's groups.
#[test]
fn an_unreadable_group_after_a_held_one_is_still_an_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let held = group_dir(root.path(), 0);
    fs::create_dir_all(&held).expect("group dir");
    fs::write(held.join("snapshot"), b"").expect("state");
    let unreadable = group_dir(root.path(), 3);
    fs::create_dir_all(&unreadable).expect("group dir");
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("chmod");

    let read = raft_state(root.path(), &GROUPS);

    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o755)).expect("restore");
    assert!(read.is_err(), "{read:?}");
}

/// The lost-log check on open counts a segment as the WAL reader does, in
/// any case, so it and this check agree on what a group holds.
#[test]
fn the_lost_log_check_takes_a_segment_in_any_case() {
    let root = tempfile::tempdir().expect("tempdir");
    fs::write(root.path().join("00000000000000000001.WAL"), b"").expect("segment");

    assert!(holds_segments(root.path()).expect("readable"));
}
