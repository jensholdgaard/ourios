//! A group's first membership: the record's bytes and every refusal, the
//! recording pass, and formation on the in-process cluster.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::num::NonZeroU32;
use std::time::Duration;

use openraft::async_runtime::WatchReceiver as _;
use ourios_parquet::Store;
use uuid::Uuid;

use super::{
    FormError, Formation, Formed, Identifier, Pass, Patience, Reach, Standing, Unreadable, Voter,
    Waiting, form, formed_key, record,
};
use crate::replicated::layout::VotingLayout;
use crate::replicated::raft::cluster::{Cluster, Node, leader, write};
use crate::replicated::raft::transport::{IdentifyError, Identity, Peers};
use crate::replicated::raft::types::NodeId;

const GROUP: u32 = 3;

/// The record §6.1 shows, as written there.
const RFC_EXAMPLE: &str = r#"{"format": 1, "group": 3, "members": [
    {"node_id": "ourios-receiver-0", "replica": "0190d2a8-7c1e-7b3a-9f00-5e1c2d3a4b5c"},
    {"node_id": "ourios-receiver-2", "replica": "0190d2a8-91f4-7c02-8a11-0c9d8e7f6a5b"},
    {"node_id": "ourios-receiver-4", "replica": "0190d2a8-a3b7-7d4e-b622-1f2e3d4c5b6a"}]}"#;

/// The same record as the encoder writes it.
const GOLDEN: &str = concat!(
    r#"{"format":1,"group":3,"members":["#,
    r#"{"node_id":"ourios-receiver-0","replica":"0190d2a8-7c1e-7b3a-9f00-5e1c2d3a4b5c"},"#,
    r#"{"node_id":"ourios-receiver-2","replica":"0190d2a8-91f4-7c02-8a11-0c9d8e7f6a5b"},"#,
    r#"{"node_id":"ourios-receiver-4","replica":"0190d2a8-a3b7-7d4e-b622-1f2e3d4c5b6a"}]}"#
);

/// The example's voters, in a placement order that is not the record's.
const VOTERS: [&str; 3] = [
    "ourios-receiver-2",
    "ourios-receiver-0",
    "ourios-receiver-4",
];

const R0: &str = "0190d2a8-7c1e-7b3a-9f00-5e1c2d3a4b5c";
const R2: &str = "0190d2a8-91f4-7c02-8a11-0c9d8e7f6a5b";
const R4: &str = "0190d2a8-a3b7-7d4e-b622-1f2e3d4c5b6a";

fn uuid(text: &str) -> NodeId {
    NodeId::from(Uuid::parse_str(text).expect("a UUID"))
}

fn decode(json: &str) -> Result<Formation, Unreadable> {
    Formation::decode(json.as_bytes(), GROUP, &VOTERS)
}

#[test]
fn the_rfc_example_decodes_and_encodes_as_the_golden_bytes() {
    let formation = decode(RFC_EXAMPLE).expect("the RFC example is readable");
    assert_eq!(formation.group(), GROUP);
    assert_eq!(
        formation.members(),
        &BTreeMap::from([
            ("ourios-receiver-0".to_owned(), uuid(R0)),
            ("ourios-receiver-2".to_owned(), uuid(R2)),
            ("ourios-receiver-4".to_owned(), uuid(R4)),
        ])
    );
    let encoded = formation.encode().expect("encode");
    assert_eq!(String::from_utf8(encoded).expect("UTF-8"), GOLDEN);
    let again = decode(GOLDEN).expect("the golden bytes are readable");
    assert_eq!(again.encode().expect("encode"), GOLDEN.as_bytes());
}

/// Members are written by `node_id` bytewise, whatever order the voters
/// rank in, each replica as a lowercase hyphenated UUID.
#[test]
fn members_are_written_by_node_id_bytewise_with_lowercase_uuids() {
    let voters = ["b", "a", "B"];
    let replica = |n: u128| {
        NodeId::from(Uuid::from_u128(
            0x0190_D2A8_0000_7000_8000_0000_0000_0000 + n,
        ))
    };
    let members = BTreeMap::from([
        ("b".to_owned(), replica(1)),
        ("a".to_owned(), replica(2)),
        ("B".to_owned(), replica(3)),
    ]);
    let formation = Formation::new(7, members, &voters).expect("the voters' answers");
    let encoded = String::from_utf8(formation.encode().expect("encode")).expect("UTF-8");
    assert_eq!(
        encoded,
        concat!(
            r#"{"format":1,"group":7,"members":["#,
            r#"{"node_id":"B","replica":"0190d2a8-0000-7000-8000-000000000003"},"#,
            r#"{"node_id":"a","replica":"0190d2a8-0000-7000-8000-000000000002"},"#,
            r#"{"node_id":"b","replica":"0190d2a8-0000-7000-8000-000000000001"}]}"#
        )
    );
}

/// Receiver `n`'s member entry.
fn member(n: u8, replica: &str) -> String {
    format!(r#"{{"node_id":"ourios-receiver-{n}","replica":"{replica}"}}"#)
}

fn record_of(format: u8, group: u32, members: &[String]) -> String {
    format!(
        r#"{{"format":{format},"group":{group},"members":[{}]}}"#,
        members.join(",")
    )
}

fn assert_refused(cases: Vec<(&str, String, &str)>) {
    for (case, json, error_type) in cases {
        let refused = decode(&json).expect_err(case);
        assert_eq!(refused.error_type(), error_type, "{case}: {refused}");
    }
}

/// A record that does not parse, is truncated, or is another format's or
/// another group's is refused with its reason.
#[test]
fn an_unparsable_or_foreign_record_is_refused_with_its_reason() {
    let all = [member(0, R0), member(2, R2), member(4, R4)];
    let truncated = &GOLDEN[..GOLDEN.len() - 3];
    assert_refused(vec![
        ("not JSON", "format: 1".to_owned(), "malformed"),
        ("truncated", truncated.to_owned(), "malformed"),
        ("empty", String::new(), "malformed"),
        (
            "an array record",
            format!("[1,{GROUP},[{}]]", all.join(",")),
            "malformed",
        ),
        (
            "an array member",
            GOLDEN.replace(&member(4, R4), &format!(r#"["ourios-receiver-4","{R4}"]"#)),
            "malformed",
        ),
        (
            "another format",
            r#"{"format":2,"voters":[]}"#.to_owned(),
            "unsupported_format",
        ),
        (
            "a string format",
            GOLDEN.replace(r#""format":1"#, r#""format":"1""#),
            "malformed",
        ),
        ("another group", record_of(1, 4, &all), "other_group"),
        (
            "an unknown field",
            GOLDEN.replace(r#""group":3"#, r#""group":3,"extra":0"#),
            "malformed",
        ),
        (
            "a format named twice",
            GOLDEN.replace(r#""format":1"#, r#""format":2,"format":1"#),
            "malformed",
        ),
        (
            "a group named twice",
            GOLDEN.replace(r#""group":3"#, r#""group":4,"group":3"#),
            "malformed",
        ),
        (
            "a member's replica named twice",
            GOLDEN.replacen(
                r#""replica":""#,
                &format!(r#""replica":"{R2}","replica":""#),
                1,
            ),
            "malformed",
        ),
    ]);
    let bad_bytes = Formation::decode(&[0xff, 0xfe], GROUP, &VOTERS).expect_err("bytes");
    assert_eq!(bad_bytes.error_type(), "malformed");
}

/// A record whose members are not exactly the voters, one UUID each in
/// the canonical form, is refused with its reason.
#[test]
fn a_record_of_other_members_is_refused_with_its_reason() {
    let m0 = member(0, R0);
    let m2 = member(2, R2);
    let m4 = member(4, R4);
    let m1 = member(1, "0190d2a8-91f4-7c02-8a11-0c9d8e7f6a5c");
    let shared = member(4, R0);
    let upper = member(4, &R4.to_uppercase());
    let simple = member(4, &R4.replace('-', ""));
    assert_refused(vec![
        (
            "a voter missing",
            record_of(1, GROUP, &[m0.clone(), m2.clone()]),
            "other_voters",
        ),
        (
            "a non-voter",
            record_of(1, GROUP, &[m0.clone(), m1.clone(), m4.clone()]),
            "other_voters",
        ),
        (
            "an extra member",
            record_of(1, GROUP, &[m0.clone(), m1, m2.clone(), m4.clone()]),
            "other_voters",
        ),
        (
            "a voter twice",
            record_of(1, GROUP, &[m0.clone(), m2.clone(), m2.clone(), m4]),
            "other_voters",
        ),
        (
            "one UUID twice",
            record_of(1, GROUP, &[m0.clone(), m2.clone(), shared]),
            "shared_replica",
        ),
        (
            "an uppercase UUID",
            record_of(1, GROUP, &[m0.clone(), m2.clone(), upper]),
            "malformed",
        ),
        (
            "an unhyphenated UUID",
            record_of(1, GROUP, &[m0, m2, simple]),
            "malformed",
        ),
    ]);
}

/// A record names a voter's replica, another one under its `node_id`, or
/// not the voter at all.
#[test]
fn a_voter_stands_by_the_uuid_recorded_for_its_node_id() {
    let formation = decode(GOLDEN).expect("readable");
    let recorded = uuid(R0);
    assert_eq!(
        formation.standing("ourios-receiver-0", recorded),
        Some(Standing::Member)
    );
    assert_eq!(
        formation.standing("ourios-receiver-0", NodeId::from(Uuid::from_u128(9))),
        Some(Standing::Replaced { recorded })
    );
    assert_eq!(formation.standing("ourios-receiver-1", recorded), None);
}

/// The recording voter never writes what a reader would refuse.
#[test]
fn answers_that_are_not_the_voters_are_not_a_formation() {
    let one = NodeId::from(Uuid::from_u128(1));
    let other = Formation::new(GROUP, BTreeMap::from([("x".to_owned(), one)]), &VOTERS);
    assert_eq!(
        other.map(|_| ()).map_err(|e| e.error_type()),
        Err("other_voters")
    );
    let shared = VOTERS.iter().map(|&v| (v.to_owned(), one)).collect();
    let shared = Formation::new(GROUP, shared, &VOTERS);
    assert_eq!(
        shared.map(|_| ()).map_err(|e| e.error_type()),
        Err("shared_replica")
    );
}

/// Peers that answer `Identify` from a table; the rest are unanswered.
struct Answering(BTreeMap<String, NodeId>);

impl Identifier for Answering {
    fn identify(
        &self,
        node_id: &str,
        _timeout: Duration,
    ) -> impl Future<Output = Result<Identity, IdentifyError>> + Send {
        let answer = match self.0.get(node_id) {
            Some(&replica) => Ok(Identity::new(node_id.to_owned(), replica)),
            None => Err(IdentifyError::Unanswered(format!("{node_id} is down"))),
        };
        std::future::ready(answer)
    }
}

const PATIENCE: Patience = Patience {
    identify: Duration::from_secs(2),
    retry: Duration::from_millis(20),
};

async fn recorded(store: &Store, group: u32) -> Option<Vec<u8>> {
    match store.get(&formed_key(group)).await {
        Ok(bytes) => Some(bytes),
        Err(e) if e.is_not_found() => None,
        Err(e) => panic!("read the record: {e}"),
    }
}

/// The example's first-ranked voter, receiver 2.
const RECORDER: Voter<'static> = Voter {
    group: GROUP,
    voters: &VOTERS,
    node_id: "ourios-receiver-2",
};

/// The first-ranked voter writes the record only once every other voter
/// has answered, itself under its own UUID.
#[tokio::test]
async fn the_record_waits_for_every_voter_and_holds_each_answer() {
    let store = Store::in_memory();
    let one = Answering(BTreeMap::from([("ourios-receiver-0".to_owned(), uuid(R0))]));
    let reach = Reach {
        store: &store,
        peers: &one,
        patience: PATIENCE,
    };
    let waited = record(RECORDER, uuid(R2), &reach).await;
    assert!(
        matches!(&waited, Pass::Wait(Waiting::Unanswered { peer, .. }) if peer == "ourios-receiver-4"),
        "{}",
        waited_for(&waited)
    );
    assert_eq!(
        recorded(&store, GROUP).await,
        None,
        "written before every answer"
    );

    let both = Answering(BTreeMap::from([
        ("ourios-receiver-0".to_owned(), uuid(R0)),
        ("ourios-receiver-4".to_owned(), uuid(R4)),
    ]));
    let reach = Reach {
        peers: &both,
        ..reach
    };
    assert!(matches!(
        record(RECORDER, uuid(R2), &reach).await,
        Pass::Recorded
    ));
    assert_eq!(
        recorded(&store, GROUP).await.as_deref(),
        Some(GOLDEN.as_bytes())
    );
}

fn waited_for(pass: &Pass) -> &'static str {
    match pass {
        Pass::Wait(why) => why.as_str(),
        Pass::Recorded => "recorded",
        Pass::Done(_) => "done",
    }
}

/// Answers that are not a membership, two voters under one UUID, are
/// never written, and the pass says why it waits.
#[tokio::test]
async fn answers_under_one_uuid_are_not_recorded() {
    let store = Store::in_memory();
    let answering = Answering(BTreeMap::from([
        ("ourios-receiver-0".to_owned(), uuid(R0)),
        ("ourios-receiver-4".to_owned(), uuid(R0)),
    ]));
    let reach = Reach {
        store: &store,
        peers: &answering,
        patience: PATIENCE,
    };
    let waited = record(RECORDER, uuid(R2), &reach).await;
    assert!(
        matches!(
            &waited,
            Pass::Wait(Waiting::NotAMembership(Unreadable::SharedReplica(_)))
        ),
        "{}",
        waited_for(&waited)
    );
    assert_eq!(recorded(&store, GROUP).await, None);
}

/// A record already there is kept: losing the create race is a pass that
/// reads the winner next.
#[tokio::test]
async fn a_record_already_there_is_kept() {
    let store = Store::in_memory();
    store
        .put(&formed_key(GROUP), GOLDEN.as_bytes().to_vec())
        .await
        .expect("put");
    let other = |n| NodeId::from(Uuid::from_u128(n));
    let answering = Answering(BTreeMap::from([
        ("ourios-receiver-0".to_owned(), other(5)),
        ("ourios-receiver-4".to_owned(), other(6)),
    ]));
    let reach = Reach {
        store: &store,
        peers: &answering,
        patience: PATIENCE,
    };
    assert!(matches!(
        record(RECORDER, other(7), &reach).await,
        Pass::Recorded
    ));
    assert_eq!(
        recorded(&store, GROUP).await.as_deref(),
        Some(GOLDEN.as_bytes())
    );
}

#[test]
fn the_record_lives_beside_the_layout() {
    assert_eq!(formed_key(3), "wal/replicated/groups/3/formed.json");
}

/// The cluster's group-0 voters in placement order: every node votes.
fn layout(cluster: &Cluster, size: usize) -> VotingLayout {
    let names = (0..size).map(|i| cluster.node(i).node_id().to_owned());
    VotingLayout::new(names, NonZeroU32::MIN, size)
}

/// The index of the node running as `node_id`.
fn index_of(cluster: &Cluster, size: usize, node_id: &str) -> usize {
    (0..size)
        .find(|&i| cluster.node(i).node_id() == node_id)
        .expect("a node")
}

async fn form_on(
    peers: &Peers,
    node: &Node,
    voters: &[&str],
    store: &Store,
) -> Result<Formed, FormError> {
    let voter = Voter {
        group: 0,
        voters,
        node_id: node.node_id(),
    };
    let reach = Reach {
        store,
        peers,
        patience: PATIENCE,
    };
    form(voter, node.raft(0), &reach, &mut |_| {}).await
}

/// The reasons `node`'s formation waited under until it was dropped.
async fn waits_of(peers: &Peers, node: &Node, voters: &[&str], store: &Store) -> Vec<&'static str> {
    let voter = Voter {
        group: 0,
        voters,
        node_id: node.node_id(),
    };
    let reach = Reach {
        store,
        peers,
        patience: PATIENCE,
    };
    let mut waits = Vec::new();
    let mut observe = |why: &Waiting| waits.push(why.as_str());
    let forming = form(voter, node.raft(0), &reach, &mut observe);
    let ended = tokio::time::timeout(Duration::from_millis(300), forming).await;
    assert!(ended.is_err(), "formed without every voter: {ended:?}");
    waits
}

/// With one voter down and no record, the first-ranked voter waits for
/// its answer and the other waits for the record, each telling the caller
/// why on every pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_voter_that_waits_reports_why_to_the_caller() {
    let mut cluster = Cluster::start_unformed(3, &[0]).await;
    let store = Store::in_memory();
    let layout = layout(&cluster, 3);
    let voters = layout.voters(0);
    let [first, second, down] = [0, 1, 2].map(|rank| index_of(&cluster, 3, voters[rank]));
    cluster.kill(down).await;

    let waits = waits_of(cluster.peers(), cluster.node(first), &voters, &store).await;
    assert!(
        !waits.is_empty() && waits.iter().all(|&w| w == "unanswered"),
        "{waits:?}"
    );
    let waits = waits_of(cluster.peers(), cluster.node(second), &voters, &store).await;
    assert!(
        !waits.is_empty() && waits.iter().all(|&w| w == "no_record"),
        "{waits:?}"
    );
    assert_eq!(recorded(&store, 0).await, None);
    cluster.shutdown().await;
}

fn voter_ids(node: &Node) -> BTreeSet<NodeId> {
    let metrics = node.raft(0).metrics().borrow_watched().clone();
    metrics.membership_config.membership().voter_ids().collect()
}

/// Three fresh voters form one group from the record and elect a leader;
/// every voter holds exactly the recorded membership.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_fresh_voters_form_one_group_from_the_record() {
    let cluster = Cluster::start_unformed(3, &[0]).await;
    let store = Store::in_memory();
    let layout = layout(&cluster, 3);
    let voters = layout.voters(0);
    let (a, b, c) = tokio::join!(
        form_on(cluster.peers(), cluster.node(0), &voters, &store),
        form_on(cluster.peers(), cluster.node(1), &voters, &store),
        form_on(cluster.peers(), cluster.node(2), &voters, &store),
    );
    for formed in [a, b, c] {
        let formed = formed.expect("formed");
        assert!(
            matches!(formed, Formed::Initialized | Formed::Joined),
            "{formed:?}"
        );
    }
    let bytes = recorded(&store, 0).await.expect("recorded");
    let formation = Formation::decode(&bytes, 0, &voters).expect("readable");
    let expected: BTreeSet<_> = (0..3).map(|i| cluster.id(i)).collect();
    assert_eq!(
        formation
            .membership()
            .keys()
            .copied()
            .collect::<BTreeSet<_>>(),
        expected
    );

    let nodes = [cluster.node(0), cluster.node(1), cluster.node(2)];
    let led = leader(0, &nodes, None).await;
    let i = (0..3)
        .find(|&i| cluster.id(i) == led)
        .expect("a voter leads");
    let index = write(cluster.node(i).raft(0), 1).await;
    for node in nodes {
        node.applied_through(0, index).await;
        assert_eq!(voter_ids(node), expected, "{} holds the record", node.id);
    }
    cluster.shutdown().await;
}

/// Have the first-ranked voter record the membership, then restart it on
/// an empty volume before it initializes. Returns its index, its old UUID
/// and the record.
async fn record_then_wipe(
    cluster: &mut Cluster,
    store: &Store,
    voters: &[&str],
) -> (usize, NodeId, Vec<u8>) {
    let first = index_of(cluster, 3, voters[0]);
    let old = cluster.id(first);
    let voter = Voter {
        group: 0,
        voters,
        node_id: voters[0],
    };
    let reach = Reach {
        store,
        peers: cluster.peers(),
        patience: PATIENCE,
    };
    assert!(matches!(record(voter, old, &reach).await, Pass::Recorded));
    let bytes = recorded(store, 0).await.expect("recorded");

    cluster.kill(first).await;
    cluster.wipe(first, 100);
    cluster.restart(first).await;
    assert_ne!(
        cluster.id(first),
        old,
        "the wiped voter runs as a new replica"
    );
    (first, old, bytes)
}

/// Scenario RFC0065.24 — the interrupted-formation arm (B4, B6): the
/// first-ranked voter records the membership, then restarts on an empty
/// volume before it initializes. The other two initialize with exactly the
/// recorded membership and elect a leader; the restarted voter, now under
/// a new UUID, never initializes. (Its join by replacement is the rejoin
/// arm, `rfc0065_24_a_wiped_voter_rejoins_under_a_new_identity`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_24_a_formation_interrupted_by_a_wiped_voter_still_forms_once() {
    let mut cluster = Cluster::start_unformed(3, &[0]).await;
    let store = Store::in_memory();
    let layout = layout(&cluster, 3);
    let voters = layout.voters(0);
    let (first, old, bytes) = record_then_wipe(&mut cluster, &store, &voters).await;

    let others: Vec<usize> = (0..3).filter(|&i| i != first).collect();
    let (wiped, a, b) = tokio::join!(
        form_on(cluster.peers(), cluster.node(first), &voters, &store),
        form_on(cluster.peers(), cluster.node(others[0]), &voters, &store),
        form_on(cluster.peers(), cluster.node(others[1]), &voters, &store),
    );
    assert_eq!(wiped.ok(), Some(Formed::Replaced { recorded: old }));
    for formed in [a, b] {
        let formed = formed.expect("formed");
        assert!(
            matches!(formed, Formed::Initialized | Formed::Joined),
            "{formed:?}"
        );
    }

    let formed = [cluster.node(others[0]), cluster.node(others[1])];
    let led = leader(0, &formed, None).await;
    let i = (0..3)
        .find(|&i| cluster.id(i) == led)
        .expect("an original voter leads");
    assert_ne!(i, first);
    let index = write(cluster.node(i).raft(0), 1).await;
    let recorded_members = Formation::decode(&bytes, 0, &voters)
        .expect("readable")
        .membership();
    for node in formed {
        node.applied_through(0, index).await;
        assert_eq!(
            voter_ids(node),
            recorded_members.keys().copied().collect(),
            "{} holds exactly the recorded membership",
            node.id
        );
    }
    let restarted = cluster.node(first);
    assert!(
        !restarted.raft(0).is_initialized().await.expect("running"),
        "the wiped voter initialized a second membership"
    );
    assert!(restarted.log(0).await.is_empty());
    assert_eq!(
        recorded(&store, 0).await,
        Some(bytes),
        "the record is written once"
    );
    cluster.shutdown().await;
}

/// Scenario RFC0065.24 — the unreadable-record arm (B4, B6): a
/// `formed.json` that does not parse, is truncated, has another `format`
/// or `group`, or whose `node_id`s are not the group's voters, forms
/// nothing, and each voter reports why. The whole record, untruncated,
/// would form the group, so only the cut makes it unreadable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_24_an_unreadable_formation_record_forms_nothing() {
    let cluster = Cluster::start_unformed(3, &[0]).await;
    let store = Store::in_memory();
    let layout = layout(&cluster, 3);
    let voters = layout.voters(0);
    for (json, error_type) in unreadable_records(&voters) {
        store
            .put(&formed_key(0), json.clone().into_bytes())
            .await
            .expect("put");
        for i in 0..3 {
            let refused = form_on(cluster.peers(), cluster.node(i), &voters, &store)
                .await
                .expect_err("an unreadable record forms nothing");
            assert!(
                matches!(refused, FormError::Unreadable { group: 0, .. }),
                "{refused}"
            );
            assert_eq!(refused.error_type(), error_type, "{json}: {refused}");
        }
        assert_eq!(
            recorded(&store, 0).await,
            Some(json.into_bytes()),
            "the first-ranked voter left the record alone"
        );
    }
    for i in 0..3 {
        let raft = cluster.node(i).raft(0);
        assert!(!raft.is_initialized().await.expect("running"));
        let metrics = raft.metrics().borrow_watched().clone();
        assert_eq!(metrics.current_leader, None);
    }
    cluster.shutdown().await;
}

/// Group 0's unreadable records for `voters`, each with the `error.type`
/// it is refused under.
fn unreadable_records(voters: &[&str]) -> Vec<(String, &'static str)> {
    let members = |ids: &[String]| {
        ids.iter()
            .enumerate()
            .map(|(n, node_id)| {
                let replica = Uuid::from_u128(u128::try_from(n).expect("small") + 1);
                format!(r#"{{"node_id":"{node_id}","replica":"{replica}"}}"#)
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut sorted: Vec<String> = voters.iter().map(|&v| v.to_owned()).collect();
    sorted.sort();
    let mut strangers = sorted.clone();
    strangers[2] = "a-stranger".to_owned();
    let whole = format!(
        r#"{{"format":1,"group":0,"members":[{}]}}"#,
        members(&sorted)
    );
    assert!(Formation::decode(whole.as_bytes(), 0, voters).is_ok());
    vec![
        ("formed.json: not JSON".to_owned(), "malformed"),
        (whole[..whole.len() - 2].to_owned(), "malformed"),
        (
            format!(
                r#"{{"format":2,"group":0,"members":[{}]}}"#,
                members(&sorted)
            ),
            "unsupported_format",
        ),
        (
            format!(
                r#"{{"format":1,"group":1,"members":[{}]}}"#,
                members(&sorted)
            ),
            "other_group",
        ),
        (
            format!(
                r#"{{"format":1,"group":0,"members":[{}]}}"#,
                members(&strangers)
            ),
            "other_voters",
        ),
    ]
}

/// A voter whose log already holds the group has nothing to form and
/// writes no record; a node that is not a voter is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_formed_group_forms_nothing_and_a_non_voter_is_refused() {
    let cluster = Cluster::start(3, &[0]).await;
    let led = cluster.leader(0, None).await;
    let index = write(cluster.node(led).raft(0), 1).await;
    for i in 0..3 {
        cluster.node(i).applied_through(0, index).await;
    }
    let store = Store::in_memory();
    let layout = layout(&cluster, 3);
    let voters = layout.voters(0);
    for i in 0..3 {
        let formed = form_on(cluster.peers(), cluster.node(i), &voters, &store).await;
        assert_eq!(formed.ok(), Some(Formed::Joined));
    }
    assert_eq!(
        recorded(&store, 0).await,
        None,
        "a formed group records nothing"
    );

    let node = cluster.node(0);
    let without: Vec<&str> = voters
        .iter()
        .copied()
        .filter(|&v| v != node.node_id())
        .collect();
    let refused = form_on(cluster.peers(), node, &without, &store)
        .await
        .expect_err("not a voter");
    assert_eq!(refused.error_type(), "not_a_voter");
    cluster.shutdown().await;
}
