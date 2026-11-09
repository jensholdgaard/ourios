use std::sync::Arc;
use std::time::{Duration, SystemTime};

use openraft::EntryPayload;
use openraft::storage::RaftStateMachine as _;
use openraft::vote::leader_id_std::CommittedLeaderId;
use opentelemetry_proto::tonic::common::v1::{AnyValue, any_value::Value};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use ourios_core::audit::{AuditPayload, TemplateChange};
use ourios_wal::{Control, MinerConfigV1, UpstreamTemplates};
use proptest::prelude::*;
use tokio::sync::mpsc;

use super::super::snapshot_record::GroupState;
use super::super::state_machine::GroupSm;
use super::super::types::{Entry, GroupEntry, TenantBatchBytes};
use super::*;

const CORPUS: [&str; 2] = [
    include_str!("../../../../../../testdata/corpus/seed.txt"),
    include_str!("../../../../../../testdata/corpus/tekton.txt"),
];

const BASE: MinerConfigV1 = MinerConfigV1 {
    similarity_threshold: 0.7,
    similarity_floor: 0.4,
    param_byte_limit: 256,
    prefix_depth: 2,
    max_node_children: 100,
    max_templates: 20_000,
    max_line_tokens: 512,
    upstream_templates: UpstreamTemplates::Ignore,
    upstream_template_byte_limit: 8192,
    upstream_association_limit: 4,
};

fn corpus() -> Vec<&'static str> {
    CORPUS
        .iter()
        .flat_map(|text| text.lines())
        .filter(|line| !line.is_empty())
        .collect()
}

fn log_id(index: u64) -> LogId {
    LogId {
        leader_id: CommittedLeaderId::new(2),
        index,
    }
}

fn export(lines: &[&str]) -> Vec<u8> {
    let log_records = lines
        .iter()
        .map(|line| LogRecord {
            body: Some(AnyValue {
                value: Some(Value::StringValue((*line).to_owned())),
            }),
            ..Default::default()
        })
        .collect();
    let request = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    request.encode_to_vec()
}

fn framed(tenant: &str, protobuf: &[u8]) -> GroupEntry {
    framed_at(BatchTime::default(), tenant, protobuf)
}

fn framed_at(time: BatchTime, tenant: &str, protobuf: &[u8]) -> GroupEntry {
    let payload = TenantBatch::encode(tenant, protobuf).expect("tenant");
    GroupEntry::batch(time, TenantBatchBytes::new(payload).expect("prefix"))
}

fn batch(tenant: &str, lines: &[&str]) -> GroupEntry {
    framed(tenant, &export(lines))
}

fn batch_at(time: BatchTime, tenant: &str, lines: &[&str]) -> GroupEntry {
    framed_at(time, tenant, &export(lines))
}

fn block(after: u64, through: u64) -> GroupEntry {
    GroupEntry::Control(Control::IdBlock { after, through })
}

fn with_threshold(similarity_threshold: f32) -> MinerConfigV1 {
    MinerConfigV1 {
        similarity_threshold,
        ..BASE
    }
}

fn configured(similarity_threshold: f32) -> GroupEntry {
    GroupEntry::Control(Control::MinerConfig(with_threshold(similarity_threshold)))
}

fn in_force(body: MinerConfigV1) -> GroupMinerConfig {
    GroupMinerConfig::new(body).expect("valid")
}

/// The events a mining replica's state machine sends for `entries`,
/// applied from index 1, through a channel of `capacity` drained as they
/// come.
fn events_of(entries: &[GroupEntry], capacity: usize) -> Vec<Applied> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let (events, mut received) = mpsc::channel(capacity);
        let mut sm = GroupSm::open_mining(dir.path(), Arc::default(), events).expect("open");
        let consumer = tokio::spawn(async move {
            let mut applied = Vec::new();
            while let Some(event) = received.recv().await {
                applied.push(event);
            }
            applied
        });
        let stream = (1..).zip(entries.iter().cloned()).map(|(index, data)| {
            let entry = Entry {
                log_id: log_id(index),
                payload: EntryPayload::Normal(data),
            };
            Ok((entry, None))
        });
        sm.apply(futures_util::stream::iter(stream))
            .await
            .expect("apply");
        drop(sm);
        consumer.await.expect("join")
    })
}

/// What `miner` mined from `events`, one [`Mined`] per event.
fn mined_each(mut miner: GroupMiner, events: Vec<Applied>) -> Vec<Mined> {
    events
        .into_iter()
        .map(|applied| miner.apply(applied).expect("mined"))
        .collect()
}

fn mined(events: Vec<Applied>) -> Vec<MinedRow> {
    mined_each(GroupMiner::new(), events)
        .into_iter()
        .flat_map(|mined| mined.rows)
        .collect()
}

/// Two replicas applying `entries`: their events and their rows.
fn two_replicas(entries: &[GroupEntry]) -> [(Vec<Applied>, Vec<MinedRow>); 2] {
    [64, 1].map(|capacity| {
        let events = events_of(entries, capacity);
        (events.clone(), mined(events))
    })
}

fn ids(rows: &[MinedRow]) -> Vec<u64> {
    rows.iter().map(|row| row.record.template_id).collect()
}

/// A row that attached cleanly to the template of `first`.
fn attached_cleanly(first: &MinedRow, row: &MinedRow) -> bool {
    row.record.template_id == first.record.template_id && row.record.body.is_none()
}

/// The id blocks the state machine applied.
fn applied_blocks(events: &[Applied]) -> Vec<IdBlock> {
    events
        .iter()
        .filter_map(|applied| match applied.event {
            AppliedEvent::IdBlock(block) => Some(block),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone)]
enum Op {
    Batch { globex: bool, lines: Vec<usize> },
    Block { after: u64, len: u64 },
    Config(f32),
}

fn op() -> impl Strategy<Value = Op> {
    let lines = corpus().len();
    prop_oneof![
        6 => (any::<bool>(), prop::collection::vec(0..lines, 1..6))
            .prop_map(|(globex, lines)| Op::Batch { globex, lines }),
        2 => (0u64..60, 1u64..8).prop_map(|(after, len)| Op::Block { after, len }),
        1 => prop::sample::select(vec![0.5_f32, 0.7, 0.9]).prop_map(Op::Config),
    ]
}

fn entry_of(op: &Op, corpus: &[&str]) -> GroupEntry {
    match op {
        Op::Batch { globex, lines } => {
            let tenant = if *globex { "globex" } else { "acme" };
            let lines: Vec<&str> = lines.iter().map(|&i| corpus[i]).collect();
            batch(tenant, &lines)
        }
        Op::Block { after, len } => block(*after, after + len),
        Op::Config(threshold) => configured(*threshold),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(ourios_testgen::proptest_cases(64)))]

    /// Two replicas applying the same log of corpus batches for two
    /// tenants, id blocks the state machine applies or ignores and
    /// configuration changes, the first anywhere or nowhere, mine the
    /// same rows at the same positions with the same template ids. Every
    /// id comes from a block the group applied, and every record is
    /// mined once a configuration has applied, in log order.
    #[test]
    fn two_replicas_applying_the_same_log_mine_the_same_rows(
        ops in prop::collection::vec(op(), 1..40)
    ) {
        let corpus = corpus();
        let entries: Vec<GroupEntry> = ops.iter().map(|op| entry_of(op, &corpus)).collect();
        let [(events, rows), (other_events, other_rows)] = two_replicas(&entries);
        prop_assert_eq!(&events, &other_events);
        prop_assert_eq!(&rows, &other_rows);

        let positions: Vec<IngestPosition> = rows.iter().map(|row| row.position).collect();
        prop_assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        let configured = ops.iter().any(|op| matches!(op, Op::Config(_)));
        let records: usize = ops
            .iter()
            .map(|op| match op {
                Op::Batch { lines, .. } => lines.len(),
                Op::Block { .. } | Op::Config(_) => 0,
            })
            .sum();
        prop_assert_eq!(rows.len(), if configured { records } else { 0 });

        let blocks = applied_blocks(&events);
        for id in ids(&rows).into_iter().filter(|&id| id != cluster::NO_TEMPLATE) {
            let from_a_block = blocks.iter().any(|b| (b.after() + 1..=b.through()).contains(&id));
            prop_assert!(from_a_block, "id {} is in no applied block", id);
        }
    }
}

/// The whole corpus, alternating between two tenants, mines alike on two
/// replicas, every record with a template id.
#[test]
fn two_replicas_mine_the_corpus_alike() {
    let mut entries = vec![configured(0.7), block(0, 10_000)];
    let tenants = ["acme", "globex"];
    entries.extend(
        corpus()
            .chunks(5)
            .zip(tenants.iter().cycle())
            .map(|(lines, tenant)| batch(tenant, lines)),
    );
    let [(_, rows), (_, other_rows)] = two_replicas(&entries);
    assert_eq!(rows, other_rows);
    assert_eq!(rows.len(), corpus().len());
    assert!(rows.iter().all(|row| row.record.template_id != 0));
    let tenant_of = |row: &MinedRow| row.record.tenant_id.as_str().to_owned();
    assert_eq!(tenant_of(&rows[0]), "acme");
    assert_eq!(tenant_of(&rows[5]), "globex");
    assert_eq!(rows[5].position, IngestPosition { seq: 4, index: 0 });
}

const WORDS: [&str; 7] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf",
];

/// Ids come from the applied blocks in log order, never from one the
/// state machine ignored; once they run out a new template takes no id
/// and keeps its body, at the same position on both replicas, until a
/// later block applies.
#[test]
fn ids_come_only_from_the_blocks_the_group_applied() {
    let entries = vec![
        configured(0.7),
        block(0, 2),
        block(1, 100),
        block(500, 502),
        batch("acme", &WORDS[..6]),
        block(600, 700),
        batch("acme", &WORDS[6..]),
    ];
    let [(events, rows), (_, other_rows)] = two_replicas(&entries);
    assert_eq!(rows, other_rows);
    assert_eq!(applied_blocks(&events).len(), 3, "(1, 100] is ignored");
    assert_eq!(ids(&rows), [1, 2, 501, 502, 0, 0, 601]);
    let exhausted: Vec<IngestPosition> = rows
        .iter()
        .filter(|row| row.record.template_id == 0)
        .map(|row| row.position)
        .collect();
    assert_eq!(
        exhausted,
        [
            IngestPosition { seq: 5, index: 4 },
            IngestPosition { seq: 5, index: 5 }
        ]
    );
    assert!(rows[4].record.body.is_some(), "the body is kept");
}

/// Batches before the first configuration wait for it and are mined
/// under it, in log order, when it applies.
#[test]
fn batches_before_the_first_configuration_are_mined_under_it() {
    let log = |threshold| {
        vec![
            batch("acme", &["a b c d e"]),
            block(0, 100),
            batch("acme", &["a b c d x"]),
            configured(threshold),
            batch("acme", &["a b c d y"]),
        ]
    };
    let mut miner = GroupMiner::new();
    let per_event: Vec<Vec<IngestPosition>> = events_of(&log(0.7), 8)
        .into_iter()
        .map(|applied| {
            let out = miner.apply(applied).expect("mined");
            out.rows.iter().map(|row| row.position).collect()
        })
        .collect();
    let at = |seq| IngestPosition { seq, index: 0 };
    assert_eq!(
        per_event,
        [vec![], vec![], vec![], vec![at(1), at(3)], vec![at(5)]]
    );

    let [(_, lenient), _] = two_replicas(&log(0.7));
    assert!(attached_cleanly(&lenient[0], &lenient[1]), "0.8 >= 0.7");
    let [(_, strict), (_, other_strict)] = two_replicas(&log(0.95));
    assert_eq!(strict, other_strict);
    assert!(!attached_cleanly(&strict[0], &strict[1]), "0.8 < 0.95");
}

/// A configuration takes effect from its own position for every tenant
/// of the group: a group miner has no per-tenant override.
#[test]
fn every_tenant_mines_under_the_groups_configuration_from_its_position() {
    let early = ["p q r s t", "p q r s u"];
    let late = ["w x y z t", "w x y z u"];
    let entries = vec![
        configured(0.7),
        block(0, 100),
        batch("acme", &early),
        batch("globex", &early),
        configured(0.95),
        batch("acme", &late),
        batch("globex", &late),
    ];
    let [(_, rows), (_, other_rows)] = two_replicas(&entries);
    assert_eq!(rows, other_rows);
    let pairs: Vec<bool> = rows
        .chunks(2)
        .map(|pair| attached_cleanly(&pair[0], &pair[1]))
        .collect();
    assert_eq!(pairs, [true, true, false, false]);
}

/// A batch whose export does not decode stops the miner at its entry,
/// before or after the first configuration, and the miner stays stopped.
#[test]
fn an_undecodable_batch_stops_the_miner() {
    let garbled = framed("acme", &[0xff, 0xff]);
    for entries in [
        vec![garbled.clone(), configured(0.7)],
        vec![configured(0.7), garbled.clone(), batch("acme", &["ok"])],
    ] {
        let events = events_of(&entries, 8);
        let mut miner = GroupMiner::new();
        let outcomes: Vec<_> = events.into_iter().map(|e| miner.apply(e)).collect();
        let first_error = outcomes.iter().find_map(|outcome| outcome.clone().err());
        let Some(GroupMinerError::Undecodable { at, .. }) = first_error.clone() else {
            panic!("expected an undecodable batch, got {first_error:?}");
        };
        let garbled_at = 1 + entries.iter().position(|e| e == &garbled).expect("in log");
        assert_eq!(at.index, garbled_at as u64);
        let after: Vec<_> = outcomes.into_iter().skip(garbled_at).collect();
        assert!(
            after
                .iter()
                .all(|outcome| outcome.clone().err() == first_error)
        );
    }
}

fn applied(index: u64, event: AppliedEvent) -> Applied {
    Applied {
        at: log_id(index),
        event,
    }
}

/// A configuration changing the tree's shape, which the state machine
/// refuses first, is an error rather than a panic.
#[test]
fn a_shape_change_is_an_error() {
    let mut miner = GroupMiner::new();
    let first = AppliedEvent::MinerConfig(in_force(BASE));
    miner.apply(applied(1, first)).expect("first");
    let deeper = in_force(MinerConfigV1 {
        prefix_depth: 3,
        ..BASE
    });
    let error = miner
        .apply(applied(2, AppliedEvent::MinerConfig(deeper)))
        .expect_err("shape");
    assert!(
        matches!(error, GroupMinerError::ShapeChanged { .. }),
        "{error}"
    );
}

/// An event that does not follow the last, or a snapshot installed over
/// the log, stops the miner.
#[test]
fn an_event_out_of_order_or_an_install_stops_the_miner() {
    let mut miner = GroupMiner::new();
    miner
        .apply(applied(2, AppliedEvent::MinerConfig(in_force(BASE))))
        .expect("first");
    let repeated = miner.apply(applied(2, AppliedEvent::Flusher(1.into())));
    assert!(matches!(repeated, Err(GroupMinerError::OutOfOrder { .. })));

    let mut miner = GroupMiner::new();
    let installed = miner.apply(applied(5, AppliedEvent::Installed(GroupState::default())));
    assert_eq!(installed, Err(GroupMinerError::Installed { at: log_id(5) }));
    let later = miner.apply(applied(6, AppliedEvent::MinerConfig(in_force(BASE))));
    assert_eq!(later, installed, "stopped");
}

/// `LogBlocks` hands out the blocks in the order pushed, then refuses.
#[test]
fn log_blocks_hand_out_blocks_in_order_then_refuse() {
    let mut blocks = LogBlocks::default();
    blocks.push(IdBlock::new(0, 5).expect("valid"));
    blocks.push(IdBlock::new(10, 12).expect("valid"));
    assert_eq!(blocks.remaining(), 2);
    let first = blocks.reserve(0).expect("first");
    assert_eq!((first.after(), first.through()), (0, 5));
    let second = blocks.reserve(5).expect("second");
    assert_eq!((second.after(), second.through()), (10, 12));
    let error = blocks.reserve(12).expect_err("none left");
    assert!(error.to_string().contains("no id block"), "{error}");
}

fn applied_in(term: u64, index: u64, event: AppliedEvent) -> Applied {
    Applied {
        at: LogId {
            leader_id: CommittedLeaderId::new(term),
            index,
        },
        event,
    }
}

/// Rows are positioned by index alone, so an index that repeats or goes
/// back stops the miner even under a later term; a later index under a
/// later term follows.
#[test]
fn an_index_that_does_not_advance_stops_the_miner_whatever_the_term() {
    let config = || AppliedEvent::MinerConfig(in_force(BASE));
    let flusher = || AppliedEvent::Flusher(1.into());
    let follows = |term, index| {
        let mut miner = GroupMiner::new();
        miner.apply(applied_in(2, 5, config())).expect("first");
        miner.apply(applied_in(term, index, flusher()))
    };
    for (term, index) in [(2, 5), (3, 5), (3, 4), (2, 4)] {
        let outcome = follows(term, index);
        assert!(
            matches!(outcome, Err(GroupMinerError::OutOfOrder { .. })),
            "({term}, {index}): {outcome:?}"
        );
    }
    assert_eq!(follows(3, 6), Ok(Mined::default()));
}

fn change_of(event: &AuditEvent) -> Option<(u64, &TemplateChange)> {
    match &event.payload {
        AuditPayload::Template {
            template_id,
            change,
            ..
        } => Some((*template_id, change)),
        _ => None,
    }
}

/// A new template and a widening each come back with the row that needs
/// them, from the event that mined it.
#[test]
fn template_events_come_with_the_rows_that_need_them() {
    let entries = vec![
        configured(0.7),
        block(0, 100),
        batch("acme", &["a b c d e"]),
        batch("acme", &["a b c d x"]),
    ];
    let mined = mined_each(GroupMiner::new(), events_of(&entries, 8));
    assert!(
        mined[..2]
            .iter()
            .all(|m| m.audit.is_empty() && m.rows.is_empty())
    );
    let (created, widened) = (&mined[2], &mined[3]);
    let id = created.rows[0].record.template_id;
    let changes = |m: &Mined| -> Vec<(u64, bool)> {
        m.audit
            .iter()
            .filter_map(change_of)
            .map(|(id, change)| (id, matches!(change, TemplateChange::Widened { .. })))
            .collect()
    };
    assert_eq!(changes(created), [(id, false)], "created");
    assert!(matches!(
        change_of(&created.audit[0]),
        Some((_, TemplateChange::Created { .. }))
    ));
    assert_eq!(widened.rows[0].record.template_id, id);
    assert_eq!(changes(widened), [(id, true)], "widened");
}

/// Two replicas emit the same audit events in the same order, apart from
/// the wall-clock timestamps; an injected clock stamps them all.
#[test]
#[ignore = "superseded: audit events carry the batch's time, the miner has no clock \
            (RFC 0065 §6.5, #1244); see two_replicas_stamp_every_audit_event_with_its_batchs_time"]
fn two_replicas_emit_the_same_audit_events_apart_from_timestamps() {
    let mut entries = vec![configured(0.7), block(0, 10_000)];
    entries.extend(corpus().chunks(5).map(|lines| batch("acme", lines)));
    let audit = |miner: GroupMiner| -> Vec<AuditEvent> {
        mined_each(miner, events_of(&entries, 8))
            .into_iter()
            .flat_map(|mined| mined.audit)
            .collect()
    };
    let untimed = |events: Vec<AuditEvent>| -> Vec<AuditEvent> {
        events
            .into_iter()
            .map(|event| AuditEvent {
                timestamp: SystemTime::UNIX_EPOCH,
                ..event
            })
            .collect()
    };
    let replica = audit(GroupMiner::new());
    assert!(replica.iter().filter_map(change_of).count() >= 2);
    assert_eq!(untimed(replica.clone()), untimed(audit(GroupMiner::new())));
}

fn time(nanos: u64) -> BatchTime {
    BatchTime::from_nanos(nanos).expect("in range")
}

fn stamps(mined: &Mined) -> Vec<SystemTime> {
    mined.audit.iter().map(|event| event.timestamp).collect()
}

/// Two replicas mining the same log, at different moments on their own
/// clocks, stamp every audit event alike: with the time the leader wrote
/// into the batch it mined, never the replica's clock (RFC 0065 §6.5).
#[test]
fn two_replicas_stamp_every_audit_event_with_its_batchs_time() {
    let base = 1_000_000_000_000_000_000;
    let mut entries = vec![configured(0.7), block(0, 10_000)];
    let mut times = vec![None, None];
    for (n, lines) in (1..).zip(corpus().chunks(5)) {
        let at = time(base + n);
        entries.push(batch_at(at, "acme", lines));
        times.push(Some(at.system_time()));
    }
    let replica = mined_each(GroupMiner::new(), events_of(&entries, 64));
    std::thread::sleep(Duration::from_millis(2));
    let other = mined_each(GroupMiner::new(), events_of(&entries, 1));
    assert_eq!(replica, other);

    let audited = replica.iter().filter(|m| !m.audit.is_empty()).count();
    assert!(audited >= 2, "{audited} batches emitted audit events");
    for (mined, at) in replica.iter().zip(&times) {
        let expected: Vec<SystemTime> = at
            .iter()
            .flat_map(|at| mined.audit.iter().map(|_| *at))
            .collect();
        assert_eq!(stamps(mined), expected);
    }
}

/// Batches that wait for the first configuration are each stamped with
/// their own time when it mines them.
#[test]
fn batches_mined_under_the_first_configuration_keep_their_own_times() {
    let (early, later, last) = (time(10), time(20), time(30));
    let entries = vec![
        batch_at(early, "acme", &["a b c d e"]),
        block(0, 100),
        batch_at(later, "acme", &["v w x y z"]),
        configured(0.7),
        batch_at(last, "acme", &["a b c d q"]),
    ];
    let mined = mined_each(GroupMiner::new(), events_of(&entries, 8));
    let at = |t: BatchTime| t.system_time();
    assert!(mined[..3].iter().all(|m| m.audit.is_empty()));
    assert_eq!(stamps(&mined[3]), [at(early), at(later)]);
    assert!(!mined[4].audit.is_empty(), "widened");
    assert!(stamps(&mined[4]).iter().all(|&t| t == at(last)));
}
