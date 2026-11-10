//! The recorded voting layout (RFC 0065 §6.1).
//!
//! The first replicated start records the voter ids, `groups` and
//! `replication_factor` in [`LAYOUT_KEY`] with a create-if-absent; every
//! later start compares its configuration with that record and refuses
//! to start on a difference, since changing the voting layout is a
//! migration RFC 0065 does not define. The tail pool (`tail_peers`,
//! `tail_replicas`, `tail_clients`) is not part of the layout (B24).

use std::collections::BTreeSet;
use std::num::NonZeroU32;

use ourios_parquet::{Store, StoreError};
use serde::{Deserialize, Serialize};

use super::placement::{group_of, place};

/// The layout object's key under the store root.
pub const LAYOUT_KEY: &str = "wal/replicated/cluster.json";

/// The layout format this build reads and writes. Any change to the
/// recorded fields bumps it, and a reader rejects every other format.
pub const LAYOUT_FORMAT: u32 = 1;

/// The voting layout a node is configured with, or the one recorded.
///
/// `peers` is the set of voter ids: config order carries no meaning, so
/// the record keeps them sorted bytewise with duplicates removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VotingLayout {
    format: u32,
    peers: BTreeSet<String>,
    groups: NonZeroU32,
    replication_factor: usize,
}

impl VotingLayout {
    /// The layout of `peers` voter ids, `groups` and `replication_factor`.
    #[must_use]
    pub fn new<I, S>(peers: I, groups: NonZeroU32, replication_factor: usize) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            format: LAYOUT_FORMAT,
            peers: peers.into_iter().map(Into::into).collect(),
            groups,
            replication_factor,
        }
    }

    /// `group`'s voters, in placement order (§6.1).
    #[must_use]
    pub fn voters(&self, group: u32) -> Vec<&str> {
        place(
            group,
            self.peers.iter().map(String::as_str),
            self.replication_factor,
        )
    }

    /// The group `tenant` belongs to (§6.1).
    #[must_use]
    pub fn group_of(&self, tenant: &str) -> u32 {
        group_of(tenant.as_bytes(), self.groups)
    }

    /// The groups `node_id` votes in, ascending.
    #[must_use]
    pub fn groups_of(&self, node_id: &str) -> Vec<u32> {
        (0..self.groups.get())
            .filter(|&group| self.voters(group).contains(&node_id))
            .collect()
    }

    /// The bytes [`LAYOUT_KEY`] holds.
    ///
    /// # Errors
    ///
    /// [`LayoutError::Encode`] if serialisation fails (not expected for
    /// this shape).
    pub fn encode(&self) -> Result<Vec<u8>, LayoutError> {
        serde_json::to_vec(self).map_err(|e| LayoutError::Encode(e.to_string()))
    }

    /// Decode the bytes of [`LAYOUT_KEY`].
    ///
    /// # Errors
    ///
    /// [`LayoutError::Malformed`] if they are not a layout, or
    /// [`LayoutError::UnsupportedFormat`] if they are another format's.
    pub fn decode(bytes: &[u8]) -> Result<Self, LayoutError> {
        // The format is read on its own first: another format may add or
        // drop fields, and that is a version mismatch, not a corrupt file.
        #[derive(Deserialize)]
        struct Header {
            format: u32,
        }
        let malformed = |e: serde_json::Error| LayoutError::Malformed {
            detail: e.to_string(),
        };
        match serde_json::from_slice::<Header>(bytes)
            .map_err(malformed)?
            .format
        {
            LAYOUT_FORMAT => serde_json::from_slice(bytes).map_err(malformed),
            found => Err(LayoutError::UnsupportedFormat(found)),
        }
    }

    fn mismatches(&self, configured: &Self) -> Vec<Mismatch> {
        let fields = [
            (
                LayoutField::Peers,
                format!("{:?}", self.peers),
                format!("{:?}", configured.peers),
            ),
            (
                LayoutField::Groups,
                self.groups.to_string(),
                configured.groups.to_string(),
            ),
            (
                LayoutField::ReplicationFactor,
                self.replication_factor.to_string(),
                configured.replication_factor.to_string(),
            ),
        ];
        fields
            .into_iter()
            .filter(|(_, recorded, configured)| recorded != configured)
            .map(|(field, recorded, configured)| Mismatch {
                field,
                recorded,
                configured,
            })
            .collect()
    }
}

/// A recorded layout field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutField {
    Peers,
    Groups,
    ReplicationFactor,
}

impl LayoutField {
    /// The configuration key the field is set by.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Peers => "receiver.wal.replicated.peers",
            Self::Groups => "receiver.wal.replicated.groups",
            Self::ReplicationFactor => "receiver.wal.replicated.replication_factor",
        }
    }
}

/// One field whose configured value differs from the recorded one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub field: LayoutField,
    pub recorded: String,
    pub configured: String,
}

/// How a start's layout check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutCheck {
    /// This start recorded the layout.
    Recorded,
    /// The recorded layout matches the configured one.
    Matches,
}

/// Why the layout could not be recorded or does not match.
#[derive(Debug)]
pub enum LayoutError {
    /// The store has no conditional update, which the replicated backend
    /// needs for the layout and its checkpoints (RFC 0065 §6.2).
    NoConditionalUpdate,
    /// The configuration differs from the recorded layout.
    Mismatch(Vec<Mismatch>),
    /// The recorded object is not a layout.
    Malformed { detail: String },
    /// The recorded object is a format this build does not read.
    UnsupportedFormat(u32),
    /// A create-if-absent lost its race but no layout is there to read.
    Vanished,
    /// The layout could not be serialised.
    Encode(String),
    /// A store call failed.
    Store {
        op: &'static str,
        source: Box<StoreError>,
    },
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConditionalUpdate => write!(
                f,
                "storage.backend: the replicated WAL backend (RFC 0065) needs S3 or an \
                 S3-compatible store with conditional updates, and this store has none"
            ),
            Self::Mismatch(mismatches) => write_mismatches(f, mismatches),
            Self::Malformed { detail } => {
                write!(f, "{LAYOUT_KEY} is not a replicated WAL layout: {detail}")
            }
            Self::UnsupportedFormat(found) => write!(
                f,
                "{LAYOUT_KEY} has layout format {found}, and this build reads only \
                 {LAYOUT_FORMAT}"
            ),
            Self::Vanished => write!(
                f,
                "{LAYOUT_KEY} existed when this start tried to record it, then could not \
                 be read"
            ),
            Self::Encode(detail) => write!(f, "encode {LAYOUT_KEY}: {detail}"),
            Self::Store { op, source } => write!(f, "{op} {LAYOUT_KEY}: {source}"),
        }
    }
}

fn write_mismatches(f: &mut std::fmt::Formatter<'_>, mismatches: &[Mismatch]) -> std::fmt::Result {
    for (i, m) in mismatches.iter().enumerate() {
        let separator = if i == 0 { "" } else { "; " };
        write!(
            f,
            "{separator}{} differs from the layout recorded in {LAYOUT_KEY}: recorded {}, \
             configured {}",
            m.field.key(),
            m.recorded,
            m.configured
        )?;
    }
    write!(
        f,
        " (changing the voting layout is a migration RFC 0065 §6.1 does not define)"
    )
}

impl std::error::Error for LayoutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Record `configured` as the cluster's voting layout on the first start,
/// or check it against the recorded one on every later start. A start
/// that loses the create race checks against the winner's record.
///
/// # Errors
///
/// [`LayoutError::NoConditionalUpdate`] on a store without conditional
/// updates; [`LayoutError::Mismatch`] naming every differing field;
/// otherwise a store, decode or encode failure.
pub fn record_or_check(
    store: &Store,
    configured: &VotingLayout,
) -> Result<LayoutCheck, LayoutError> {
    if !store.supports_conditional_update() {
        return Err(LayoutError::NoConditionalUpdate);
    }
    match read(store)? {
        Some(recorded) => compare(&recorded, configured),
        None => create(store, configured),
    }
}

/// Check `configured` against the recorded layout without recording it.
///
/// # Errors
///
/// As [`record_or_check`].
pub fn check_recorded(store: &Store, configured: &VotingLayout) -> Result<(), LayoutError> {
    record_or_check(store, configured).map(|_| ())
}

/// Create the record, or check against the one a concurrent first start
/// created between this start's read and its write.
fn create(store: &Store, configured: &VotingLayout) -> Result<LayoutCheck, LayoutError> {
    match store.put_if_absent_blocking(LAYOUT_KEY, configured.encode()?) {
        Ok(()) => Ok(LayoutCheck::Recorded),
        Err(e) if e.is_already_exists() => match read(store)? {
            Some(recorded) => compare(&recorded, configured),
            None => Err(LayoutError::Vanished),
        },
        Err(e) => Err(store_err("write", e)),
    }
}

fn read(store: &Store) -> Result<Option<VotingLayout>, LayoutError> {
    store
        .get_blocking_opt(LAYOUT_KEY)
        .map_err(|e| store_err("read", e))?
        .map(|bytes| VotingLayout::decode(&bytes))
        .transpose()
}

fn compare(recorded: &VotingLayout, configured: &VotingLayout) -> Result<LayoutCheck, LayoutError> {
    let mismatches = recorded.mismatches(configured);
    if mismatches.is_empty() {
        Ok(LayoutCheck::Matches)
    } else {
        Err(LayoutError::Mismatch(mismatches))
    }
}

fn store_err(op: &'static str, source: StoreError) -> LayoutError {
    LayoutError::Store {
        op,
        source: Box::new(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    fn layout(peers: &[&str], g: u32, rf: usize) -> VotingLayout {
        VotingLayout::new(peers.iter().copied(), groups(g), rf)
    }

    fn mismatch_fields(err: &LayoutError) -> Vec<LayoutField> {
        match err {
            LayoutError::Mismatch(m) => m.iter().map(|m| m.field).collect(),
            other => panic!("expected a mismatch, got {other}"),
        }
    }

    #[test]
    fn the_persisted_format_is_pinned() {
        let bytes = layout(&["c", "a", "b"], 16, 3).encode().expect("encode");
        assert_eq!(
            String::from_utf8(bytes.clone()).expect("utf-8"),
            r#"{"format":1,"peers":["a","b","c"],"groups":16,"replication_factor":3}"#
        );
        assert_eq!(
            VotingLayout::decode(&bytes).expect("decode"),
            layout(&["a", "b", "c"], 16, 3)
        );
    }

    #[test]
    fn a_node_votes_in_the_groups_that_place_it() {
        let peers = [
            "receiver-0",
            "receiver-1",
            "receiver-2",
            "receiver-3",
            "receiver-4",
        ];
        let layout = layout(&peers, 4, 3);
        assert_eq!(layout.voters(0), ["receiver-4", "receiver-1", "receiver-0"]);
        assert_eq!(layout.voters(3), ["receiver-2", "receiver-4", "receiver-1"]);
        assert_eq!(layout.groups_of("receiver-4"), [0, 2, 3]);
        assert_eq!(layout.groups_of("receiver-3"), [1]);
        assert!(layout.groups_of("tail-0").is_empty());
        let votes: usize = peers.iter().map(|p| layout.groups_of(p).len()).sum();
        assert_eq!(votes, 4 * 3, "every group has replication_factor voters");
    }

    #[test]
    fn a_tenant_routes_by_the_layouts_group_count() {
        let layout = layout(&["a", "b", "c"], 16, 3);
        for tenant in ["acme", "globex", "initech"] {
            assert_eq!(
                layout.group_of(tenant),
                group_of(tenant.as_bytes(), groups(16))
            );
        }
    }

    #[test]
    fn decode_rejects_other_formats_and_unknown_fields() {
        let later = br#"{"format":2,"peers":["a"],"groups":1,"replication_factor":3}"#;
        assert!(matches!(
            VotingLayout::decode(later),
            Err(LayoutError::UnsupportedFormat(2))
        ));
        let reshaped = br#"{"format":2,"voters":["a"],"groups":1,"zones":2}"#;
        assert!(matches!(
            VotingLayout::decode(reshaped),
            Err(LayoutError::UnsupportedFormat(2))
        ));
        let extra = br#"{"format":1,"peers":["a"],"groups":1,"replication_factor":3,"x":1}"#;
        assert!(matches!(
            VotingLayout::decode(extra),
            Err(LayoutError::Malformed { .. })
        ));
        let zero_groups = br#"{"format":1,"peers":["a"],"groups":0,"replication_factor":3}"#;
        assert!(matches!(
            VotingLayout::decode(zero_groups),
            Err(LayoutError::Malformed { .. })
        ));
    }

    #[test]
    fn the_first_start_records_and_later_starts_match() {
        let store = Store::in_memory();
        let configured = layout(&["a", "b", "c"], 16, 3);
        assert_eq!(
            record_or_check(&store, &configured).expect("record"),
            LayoutCheck::Recorded
        );
        let reordered = layout(&["c", "b", "a"], 16, 3);
        assert_eq!(
            record_or_check(&store, &reordered).expect("check"),
            LayoutCheck::Matches
        );
        let stored = store.get_blocking(LAYOUT_KEY).expect("recorded");
        assert_eq!(stored, configured.encode().expect("encode"));
    }

    #[test]
    fn each_differing_field_fails_naming_it() {
        let store = Store::in_memory();
        record_or_check(&store, &layout(&["a", "b", "c"], 16, 3)).expect("record");
        let cases = [
            (layout(&["a", "b", "d"], 16, 3), LayoutField::Peers),
            (layout(&["a", "b", "c", "d"], 16, 3), LayoutField::Peers),
            (layout(&["a", "b", "c"], 32, 3), LayoutField::Groups),
            (
                layout(&["a", "b", "c"], 16, 5),
                LayoutField::ReplicationFactor,
            ),
        ];
        for (configured, field) in cases {
            let err = record_or_check(&store, &configured).expect_err("mismatch");
            assert_eq!(mismatch_fields(&err), vec![field]);
            assert!(err.to_string().contains(field.key()), "{err}");
        }
    }

    #[test]
    fn a_mismatch_names_the_recorded_and_configured_values() {
        let store = Store::in_memory();
        record_or_check(&store, &layout(&["a", "b", "c"], 16, 3)).expect("record");
        let err = record_or_check(&store, &layout(&["a", "b", "d"], 8, 3)).expect_err("mismatch");
        assert_eq!(
            err.to_string(),
            "receiver.wal.replicated.peers differs from the layout recorded in \
             wal/replicated/cluster.json: recorded {\"a\", \"b\", \"c\"}, configured \
             {\"a\", \"b\", \"d\"}; receiver.wal.replicated.groups differs from the layout \
             recorded in wal/replicated/cluster.json: recorded 16, configured 8 (changing \
             the voting layout is a migration RFC 0065 §6.1 does not define)"
        );
    }

    #[test]
    fn a_lost_create_race_checks_against_the_winner() {
        let store = Store::in_memory();
        let winner = layout(&["a", "b", "c"], 16, 3);
        store
            .put_if_absent_blocking(LAYOUT_KEY, winner.encode().expect("encode"))
            .expect("winner");
        assert_eq!(
            create(&store, &winner).expect("same layout"),
            LayoutCheck::Matches
        );
        let different = create(&store, &layout(&["a", "b", "c"], 16, 5));
        assert_eq!(
            mismatch_fields(&different.expect_err("different layout")),
            vec![LayoutField::ReplicationFactor]
        );
    }

    #[test]
    fn two_concurrent_first_starts_agree_on_one_record() {
        let store = Store::in_memory();
        let configured = layout(&["a", "b", "c"], 16, 3);
        let outcomes: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| s.spawn(|| record_or_check(&store, &configured).expect("check")))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("join"))
                .collect()
        });
        let recorded = outcomes
            .iter()
            .filter(|o| **o == LayoutCheck::Recorded)
            .count();
        assert_eq!(recorded, 1, "{outcomes:?}");
    }

    #[test]
    fn a_store_without_conditional_updates_is_refused() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("local store");
        assert!(!store.supports_conditional_update());
        let err = record_or_check(&store, &layout(&["a", "b", "c"], 16, 3)).expect_err("refused");
        assert!(matches!(err, LayoutError::NoConditionalUpdate));
        assert!(err.to_string().starts_with("storage.backend:"), "{err}");
        assert!(store.get_blocking_opt(LAYOUT_KEY).expect("read").is_none());
    }

    #[test]
    fn a_malformed_record_fails_startup() {
        let store = Store::in_memory();
        store
            .put_blocking(LAYOUT_KEY, b"not json".to_vec())
            .expect("put");
        let err = record_or_check(&store, &layout(&["a"], 1, 3)).expect_err("malformed");
        assert!(matches!(err, LayoutError::Malformed { .. }), "{err}");
    }

    fn keys(store: &Store) -> Vec<String> {
        store.list_blocking(None).expect("list")
    }

    #[test]
    fn checking_without_a_record_records_nothing() {
        let store = Store::in_memory();
        check_recorded(&store, &layout(&["a", "b", "c"], 16, 3)).expect("nothing to differ from");
        assert!(keys(&store).is_empty(), "{:?}", keys(&store));
    }

    #[test]
    fn checking_against_a_matching_record_leaves_it() {
        let store = Store::in_memory();
        record_or_check(&store, &layout(&["a", "b", "c"], 16, 3)).expect("record");
        let recorded = store.get_blocking(LAYOUT_KEY).expect("recorded");
        check_recorded(&store, &layout(&["c", "b", "a"], 16, 3)).expect("matches");
        assert_eq!(keys(&store), [LAYOUT_KEY]);
        assert_eq!(store.get_blocking(LAYOUT_KEY).expect("kept"), recorded);
    }

    #[test]
    fn checking_against_a_different_record_fails_as_recording_does() {
        let store = Store::in_memory();
        record_or_check(&store, &layout(&["a", "b", "c"], 16, 3)).expect("record");
        let recorded = store.get_blocking(LAYOUT_KEY).expect("recorded");
        let configured = layout(&["a", "b", "d"], 8, 3);
        let checked = check_recorded(&store, &configured).expect_err("mismatch");
        let recording = record_or_check(&store, &configured).expect_err("mismatch");
        assert_eq!(
            mismatch_fields(&checked),
            [LayoutField::Peers, LayoutField::Groups]
        );
        assert_eq!(checked.to_string(), recording.to_string());
        assert_eq!(keys(&store), [LAYOUT_KEY]);
        assert_eq!(store.get_blocking(LAYOUT_KEY).expect("kept"), recorded);
    }

    #[test]
    fn checking_a_malformed_record_fails_and_keeps_it() {
        let store = Store::in_memory();
        store
            .put_blocking(LAYOUT_KEY, b"not json".to_vec())
            .expect("put");
        let err = check_recorded(&store, &layout(&["a"], 1, 3)).expect_err("malformed");
        assert!(matches!(err, LayoutError::Malformed { .. }), "{err}");
        assert_eq!(store.get_blocking(LAYOUT_KEY).expect("kept"), b"not json");
    }

    #[test]
    fn checking_on_a_store_without_conditional_updates_is_refused() {
        let dir = tempfile::TempDir::new().expect("temp");
        let store = Store::local(dir.path()).expect("local store");
        let err = check_recorded(&store, &layout(&["a", "b", "c"], 16, 3)).expect_err("refused");
        assert!(matches!(err, LayoutError::NoConditionalUpdate), "{err}");
        assert!(keys(&store).is_empty(), "{:?}", keys(&store));
    }
}
