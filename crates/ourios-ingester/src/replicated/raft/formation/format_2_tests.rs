//! The format-2 record (RFC 0065 §6.1): its bytes and every refusal, the
//! log id the recording voter allocates from `log_ids.json` (§6.6), and
//! formation under it on the in-process cluster.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use openraft::async_runtime::WatchReceiver as _;
use ourios_parquet::Store;
use uuid::Uuid;

use super::tests::{
    Answering, GROUP, PATIENCE, R0, R2, R4, RECORDER, VOTERS, assert_refused, decode, form_on,
    index_of, layout, member, record_then_wipe, recorded, uuid, voter_ids, waited_for,
};
use super::{FormError, Formation, Formed, Pass, Reach, Standing, Waiting, formed_key, record};
use crate::replicated::layout::VotingLayout;
use crate::replicated::raft::cluster::{Cluster, leader, write};
use crate::replicated::raft::log_ids::{LOG_IDS_KEY, LogIds, Refused, allocate};
use crate::replicated::raft::types::NodeId;

/// The record §6.1 shows, as written there.
const RFC_EXAMPLE: &str = r#"{"format": 2, "group": 3, "log": 7, "members": [
    {"node_id": "ourios-receiver-0", "replica": "0190d2a8-7c1e-7b3a-9f00-5e1c2d3a4b5c"},
    {"node_id": "ourios-receiver-2", "replica": "0190d2a8-91f4-7c02-8a11-0c9d8e7f6a5b"},
    {"node_id": "ourios-receiver-4", "replica": "0190d2a8-a3b7-7d4e-b622-1f2e3d4c5b6a"}]}"#;

/// The same record as the encoder writes it.
const GOLDEN: &str = concat!(
    r#"{"format":2,"group":3,"log":7,"members":["#,
    r#"{"node_id":"ourios-receiver-0","replica":"0190d2a8-7c1e-7b3a-9f00-5e1c2d3a4b5c"},"#,
    r#"{"node_id":"ourios-receiver-2","replica":"0190d2a8-91f4-7c02-8a11-0c9d8e7f6a5b"},"#,
    r#"{"node_id":"ourios-receiver-4","replica":"0190d2a8-a3b7-7d4e-b622-1f2e3d4c5b6a"}]}"#
);

/// The example's record in the first slice's format 1, without a log id.
const FORMAT_1: &str = concat!(
    r#"{"format":1,"group":3,"members":["#,
    r#"{"node_id":"ourios-receiver-0","replica":"0190d2a8-7c1e-7b3a-9f00-5e1c2d3a4b5c"},"#,
    r#"{"node_id":"ourios-receiver-2","replica":"0190d2a8-91f4-7c02-8a11-0c9d8e7f6a5b"},"#,
    r#"{"node_id":"ourios-receiver-4","replica":"0190d2a8-a3b7-7d4e-b622-1f2e3d4c5b6a"}]}"#
);

fn record_of(log: u32, group: u32, members: &[String]) -> String {
    format!(
        r#"{{"format":2,"group":{group},"log":{log},"members":[{}]}}"#,
        members.join(",")
    )
}

#[test]
fn the_format_2_rfc_example_decodes_and_encodes_as_the_golden_bytes() {
    let formation = decode(RFC_EXAMPLE).expect("the RFC example is readable");
    assert_eq!(formation.group(), GROUP);
    assert_eq!(formation.log(), 7);
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
    let max = decode(&GOLDEN.replace(r#""log":7"#, r#""log":4294967295"#)).expect("u32::MAX");
    assert_eq!(max.log(), u32::MAX);
}

/// Members are written by `node_id` bytewise, whatever order the voters
/// rank in, each replica as a lowercase hyphenated UUID, after the log id.
#[test]
fn format_2_members_are_written_by_node_id_bytewise() {
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
    let formation = Formation::new(7, 12, members, &voters).expect("the voters' answers");
    let encoded = String::from_utf8(formation.encode().expect("encode")).expect("UTF-8");
    assert_eq!(
        encoded,
        concat!(
            r#"{"format":2,"group":7,"log":12,"members":["#,
            r#"{"node_id":"B","replica":"0190d2a8-0000-7000-8000-000000000003"},"#,
            r#"{"node_id":"a","replica":"0190d2a8-0000-7000-8000-000000000002"},"#,
            r#"{"node_id":"b","replica":"0190d2a8-0000-7000-8000-000000000001"}]}"#
        )
    );
}

/// A record that does not parse, is truncated, is another format's
/// (format 1 among them) or another group's, or whose log id is not a
/// `u32`, is refused with its reason.
#[test]
fn an_unparsable_or_foreign_format_2_record_is_refused_with_its_reason() {
    let all = [member(0, R0), member(2, R2), member(4, R4)];
    let truncated = &GOLDEN[..GOLDEN.len() - 3];
    let log = |text: &str| GOLDEN.replace(r#""log":7"#, text);
    assert_refused(vec![
        ("not JSON", "format: 2".to_owned(), "malformed"),
        ("truncated", truncated.to_owned(), "malformed"),
        ("empty", String::new(), "malformed"),
        (
            "an array record",
            format!("[2,{GROUP},7,[{}]]", all.join(",")),
            "malformed",
        ),
        (
            "an array member",
            GOLDEN.replace(&member(4, R4), &format!(r#"["ourios-receiver-4","{R4}"]"#)),
            "malformed",
        ),
        ("format 1", FORMAT_1.to_owned(), "unsupported_format"),
        (
            "another format",
            r#"{"format":3,"voters":[]}"#.to_owned(),
            "unsupported_format",
        ),
        (
            "a string format",
            GOLDEN.replace(r#""format":2"#, r#""format":"2""#),
            "malformed",
        ),
        ("another group", record_of(7, 4, &all), "other_group"),
        (
            "an unknown field",
            GOLDEN.replace(r#""group":3"#, r#""group":3,"extra":0"#),
            "malformed",
        ),
        ("no log id", log(""), "malformed"),
        ("a string log id", log(r#""log":"7""#), "malformed"),
        ("a negative log id", log(r#""log":-1"#), "malformed"),
        ("a log id past u32", log(r#""log":4294967296"#), "malformed"),
    ]);
    let bad_bytes = Formation::decode(&[0xff, 0xfe], GROUP, &VOTERS).expect_err("bytes");
    assert_eq!(bad_bytes.error_type(), "malformed");
}

/// A record naming a field twice is ambiguous, whichever copy a reader
/// would keep, so it is refused.
#[test]
fn a_format_2_record_naming_a_field_twice_is_refused() {
    assert_refused(vec![
        (
            "a format named twice",
            GOLDEN.replace(r#""format":2"#, r#""format":1,"format":2"#),
            "malformed",
        ),
        (
            "a group named twice",
            GOLDEN.replace(r#""group":3"#, r#""group":4,"group":3"#),
            "malformed",
        ),
        (
            "a log id named twice",
            GOLDEN.replace(r#""log":7"#, r#""log":8,"log":7"#),
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
}

/// A record whose members are not exactly the voters, one UUID each in
/// the canonical form, is refused with its reason.
#[test]
fn a_format_2_record_of_other_members_is_refused_with_its_reason() {
    let m0 = member(0, R0);
    let m2 = member(2, R2);
    let m4 = member(4, R4);
    let m1 = member(1, "0190d2a8-91f4-7c02-8a11-0c9d8e7f6a5c");
    assert_refused(vec![
        (
            "a voter missing",
            record_of(7, GROUP, &[m0.clone(), m2.clone()]),
            "other_voters",
        ),
        (
            "a non-voter",
            record_of(7, GROUP, &[m0.clone(), m1.clone(), m4.clone()]),
            "other_voters",
        ),
        (
            "an extra member",
            record_of(7, GROUP, &[m0.clone(), m1, m2.clone(), m4.clone()]),
            "other_voters",
        ),
        (
            "a voter twice",
            record_of(7, GROUP, &[m0.clone(), m2.clone(), m2.clone(), m4]),
            "other_voters",
        ),
        (
            "one UUID twice",
            record_of(7, GROUP, &[m0.clone(), m2.clone(), member(4, R0)]),
            "shared_replica",
        ),
        (
            "an uppercase UUID",
            record_of(
                7,
                GROUP,
                &[m0.clone(), m2.clone(), member(4, &R4.to_uppercase())],
            ),
            "malformed",
        ),
        (
            "an unhyphenated UUID",
            record_of(7, GROUP, &[m0, m2, member(4, &R4.replace('-', ""))]),
            "malformed",
        ),
    ]);
}

#[test]
fn a_voter_stands_by_the_uuid_its_format_2_record_holds() {
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

/// The lock a seeded store holds, with `next` its next log id.
fn seeded(next: u32) -> LogIds {
    LogIds {
        bootstrapper: "ourios-receiver-0".to_owned(),
        seeded: true,
        next,
    }
}

async fn put_lock(store: &Store, lock: &LogIds) {
    let bytes = lock.encode().expect("encode");
    store.put(LOG_IDS_KEY, bytes).await.expect("put the lock");
}

/// The lock's `next`, the number of log ids allocated.
async fn next(store: &Store) -> u32 {
    let bytes = store.get(LOG_IDS_KEY).await.expect("the lock");
    LogIds::decode(&bytes).expect("readable").next
}

fn both_answer() -> Answering {
    Answering(BTreeMap::from([
        ("ourios-receiver-0".to_owned(), uuid(R0)),
        ("ourios-receiver-4".to_owned(), uuid(R4)),
    ]))
}

/// The first-ranked voter allocates a log id only once every other voter
/// has answered, and records it with each answer.
#[tokio::test]
async fn the_record_holds_a_log_id_allocated_once_every_voter_answered() {
    let store = Store::in_memory();
    put_lock(&store, &seeded(7)).await;
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
    assert_eq!(next(&store).await, 7, "an id spent before every answer");

    let both = both_answer();
    let reach = Reach {
        peers: &both,
        ..reach
    };
    let recorded_pass = record(RECORDER, uuid(R2), &reach).await;
    assert_eq!(waited_for(&recorded_pass), "recorded");
    assert_eq!(
        recorded(&store, GROUP).await.as_deref(),
        Some(GOLDEN.as_bytes())
    );
    assert_eq!(next(&store).await, 8);
}

/// Answers that are not a membership spend no log id.
#[tokio::test]
async fn answers_under_one_uuid_spend_no_log_id() {
    let store = Store::in_memory();
    put_lock(&store, &seeded(7)).await;
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
    assert_eq!(waited_for(&waited), "not_a_membership");
    assert_eq!(next(&store).await, 7);
    assert_eq!(recorded(&store, GROUP).await, None);
}

/// A record already there is kept; the id this voter allocated for the
/// create it lost is spent, never handed out again.
#[tokio::test]
async fn a_record_already_there_is_kept_and_its_allocated_id_is_spent() {
    let store = Store::in_memory();
    put_lock(&store, &seeded(8)).await;
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
    let pass = record(RECORDER, other(7), &reach).await;
    assert_eq!(waited_for(&pass), "recorded");
    assert_eq!(
        recorded(&store, GROUP).await.as_deref(),
        Some(GOLDEN.as_bytes())
    );
    assert_eq!(next(&store).await, 9, "the lost create's id is spent");
}

/// No log id exists before the high-water object is seeded: the recording
/// voter waits, writing nothing.
#[tokio::test]
async fn a_recording_voter_waits_for_an_unseeded_lock() {
    let store = Store::in_memory();
    let unseeded = LogIds {
        seeded: false,
        ..seeded(0)
    };
    put_lock(&store, &unseeded).await;
    let both = both_answer();
    let reach = Reach {
        store: &store,
        peers: &both,
        patience: PATIENCE,
    };
    let pass = record(RECORDER, uuid(R2), &reach).await;
    assert_eq!(waited_for(&pass), "log_ids_unseeded");
    assert_eq!(recorded(&store, GROUP).await, None);
    assert_eq!(next(&store).await, 0);
}

/// A deleted or unreadable lock fails the recording step closed: no
/// record, and the lock is neither re-created nor replaced.
#[tokio::test]
async fn a_deleted_or_unreadable_lock_records_nothing() {
    let store = Store::in_memory();
    let both = both_answer();
    let reach = Reach {
        store: &store,
        peers: &both,
        patience: PATIENCE,
    };
    let pass = record(RECORDER, uuid(R2), &reach).await;
    assert!(
        matches!(
            &pass,
            Pass::Fail(FormError::LogIds {
                group: GROUP,
                reason: Refused::Deleted
            })
        ),
        "{}",
        waited_for(&pass)
    );
    assert_eq!(recorded(&store, GROUP).await, None);
    assert!(store.get(LOG_IDS_KEY).await.is_err(), "re-created");

    let garbage = b"{}".to_vec();
    store.put(LOG_IDS_KEY, garbage.clone()).await.expect("put");
    let pass = record(RECORDER, uuid(R2), &reach).await;
    assert_eq!(waited_for(&pass), "log_ids_unreadable");
    assert_eq!(recorded(&store, GROUP).await, None);
    assert_eq!(store.get(LOG_IDS_KEY).await.ok(), Some(garbage));
}

/// Three fresh voters form one group from a record holding the one log
/// id the first-ranked voter allocated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_fresh_voters_form_one_group_under_one_log_id() {
    let cluster = Cluster::start_unformed(3, &[0]).await;
    let store = Store::in_memory();
    put_lock(&store, &seeded(3)).await;
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
    assert_eq!(formation.log(), 3);
    assert_eq!(next(&store).await, 4, "one id allocated");
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

/// Scenario RFC0065.24 — the interrupted-formation arm (B4, B6), on a
/// format-2 record: the wiped first-ranked voter never initializes, the
/// other two form from the record, and the record and its log id are
/// written once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_24_a_format_2_formation_interrupted_by_a_wiped_voter_still_forms_once() {
    let mut cluster = Cluster::start_unformed(3, &[0]).await;
    let store = Store::in_memory();
    put_lock(&store, &seeded(0)).await;
    let layout = layout(&cluster, 3);
    let voters = layout.voters(0);
    let (first, old, bytes) = record_then_wipe(&mut cluster, &store, &voters).await;
    let formation = Formation::decode(&bytes, 0, &voters).expect("readable");
    assert_eq!(formation.log(), 0);

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
    for node in formed {
        node.applied_through(0, index).await;
        assert_eq!(
            voter_ids(node),
            formation.membership().keys().copied().collect(),
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
    assert_eq!(recorded(&store, 0).await, Some(bytes), "written once");
    assert_eq!(next(&store).await, 1, "one id allocated");
    cluster.shutdown().await;
}

/// Group 0's unreadable records for `voters`, each with the `error.type`
/// it is refused under; the whole record would form the group.
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
    let with = |format: u32, group: u32, log: &str, ids: &[String]| {
        format!(
            r#"{{"format":{format},"group":{group},{log}"members":[{}]}}"#,
            members(ids)
        )
    };
    let whole = with(2, 0, r#""log":0,"#, &sorted);
    assert!(Formation::decode(whole.as_bytes(), 0, voters).is_ok());
    vec![
        ("formed.json: not JSON".to_owned(), "malformed"),
        (whole[..whole.len() - 2].to_owned(), "malformed"),
        (with(1, 0, "", &sorted), "unsupported_format"),
        (with(3, 0, r#""log":0,"#, &sorted), "unsupported_format"),
        (with(2, 1, r#""log":0,"#, &sorted), "other_group"),
        (with(2, 0, r#""log":0,"#, &strangers), "other_voters"),
        (with(2, 0, "", &sorted), "malformed"),
    ]
}

/// Scenario RFC0065.24 — the unreadable-record arm (B4, B6), on format 2:
/// a record that does not parse, is truncated, is format 1 or another
/// group's, names other voters or carries no log id forms nothing, and
/// each voter reports why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_24_an_unreadable_format_2_record_forms_nothing() {
    let cluster = Cluster::start_unformed(3, &[0]).await;
    let store = Store::in_memory();
    put_lock(&store, &seeded(0)).await;
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
    assert_eq!(next(&store).await, 0, "no id allocated");
    cluster.shutdown().await;
}

/// The cluster's voters of `group` under a layout of `groups` groups:
/// every node votes in each.
fn voters_of(cluster: &Cluster, groups: u32, group: u32) -> Vec<String> {
    let names = (0..3).map(|i| cluster.node(i).node_id().to_owned());
    let groups = NonZeroU32::new(groups).expect("non-zero");
    let layout = VotingLayout::new(names, groups, 3);
    layout.voters(group).iter().map(|&v| v.to_owned()).collect()
}

/// Form `group` on every node of `cluster`, returning its record.
async fn form_group(cluster: &Cluster, store: &Store, voters: &[&str], group: u32) -> Formation {
    let reach = Reach {
        store,
        peers: cluster.peers(),
        patience: PATIENCE,
    };
    let forming = (0..3).map(|i| {
        let node = cluster.node(i);
        let voter = super::Voter {
            group,
            voters,
            node_id: node.node_id(),
        };
        let reach = &reach;
        async move { super::form(voter, node.raft(group), reach, &mut |_| {}).await }
    });
    for formed in futures::future::join_all(forming).await {
        let formed = formed.expect("formed");
        assert!(
            matches!(formed, Formed::Initialized | Formed::Joined),
            "{formed:?}"
        );
    }
    let bytes = recorded(store, group).await.expect("recorded");
    Formation::decode(&bytes, group, voters).expect("readable")
}

/// Scenario RFC0065.24 — the log-id arm (B13): the group's `formed.json`
/// is format 2 and carries a log id allocated from `log_ids.json`. A
/// recording voter killed between the allocation and its write is stood in
/// for by an allocation with no write after it: the record then holds a
/// later id. Two groups are recorded under distinct ids, and a format-1
/// record forms nothing. (Every replica writing it as `ingest_group` comes
/// with publication.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rfc0065_24_the_formation_record_carries_the_groups_log_id() {
    let cluster = Cluster::start_unformed(3, &[0, 1]).await;
    let store = Store::in_memory();
    put_lock(&store, &seeded(0)).await;
    // An allocation with no write after it, what a recording voter killed
    // between the two leaves behind: id 0 is spent.
    assert_eq!(allocate(&store).await.ok(), Some(0));

    let g0 = voters_of(&cluster, 2, 0);
    let g0: Vec<&str> = g0.iter().map(String::as_str).collect();
    let first = form_group(&cluster, &store, &g0, 0).await;
    assert_eq!(first.log(), 1, "a later id than the one never recorded");
    let bytes = recorded(&store, 0).await.expect("recorded");
    assert!(
        bytes.starts_with(br#"{"format":2,"group":0,"log":1,"#),
        "{}",
        String::from_utf8_lossy(&bytes)
    );

    let g1 = voters_of(&cluster, 2, 1);
    let g1: Vec<&str> = g1.iter().map(String::as_str).collect();
    store
        .put(&formed_key(1), format_1_of(&cluster, &g1).into_bytes())
        .await
        .expect("put");
    let refused = form_on_group(&cluster, &store, &g1, 1).await;
    assert_eq!(refused, "unsupported_format", "a format-1 record forms");
    store.delete(&formed_key(1)).await.expect("delete");
    let second = form_group(&cluster, &store, &g1, 1).await;
    assert_eq!(second.log(), 2, "no id recorded twice");
    assert_eq!(next(&store).await, 3);
    cluster.shutdown().await;
}

/// A format-1 record of `voters` with the cluster's own UUIDs, one that
/// would form the group if format 1 were read.
fn format_1_of(cluster: &Cluster, voters: &[&str]) -> String {
    let mut members: Vec<(String, NodeId)> = voters
        .iter()
        .map(|&v| (v.to_owned(), cluster.id(index_of(cluster, 3, v))))
        .collect();
    members.sort();
    let members: Vec<String> = members
        .iter()
        .map(|(node_id, replica)| {
            let replica = replica.uuid().hyphenated();
            format!(r#"{{"node_id":"{node_id}","replica":"{replica}"}}"#)
        })
        .collect();
    format!(
        r#"{{"format":1,"group":1,"members":[{}]}}"#,
        members.join(",")
    )
}

/// The `error.type` each voter of `group` refuses its record under, the
/// same for all.
async fn form_on_group(
    cluster: &Cluster,
    store: &Store,
    voters: &[&str],
    group: u32,
) -> &'static str {
    let reach = Reach {
        store,
        peers: cluster.peers(),
        patience: PATIENCE,
    };
    let mut refusals = BTreeSet::new();
    for i in 0..3 {
        let node = cluster.node(i);
        let voter = super::Voter {
            group,
            voters,
            node_id: node.node_id(),
        };
        let refused = super::form(voter, node.raft(group), &reach, &mut |_| {})
            .await
            .expect_err("forms nothing");
        refusals.insert(refused.error_type());
    }
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    refusals.pop_first().expect("one")
}
