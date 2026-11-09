//! Reading a group's log entries: unreadable control entries, the timed
//! batch record and refused miner configurations (RFC0065.29, .31).

/// Scenario RFC0065.29 — a log entry a replica cannot read stops it (B11),
/// the log arm. Given a flusher assignment and then an unknown control kind,
/// or an assignment whose body is not 16 bytes, when the replica opens the
/// group's log, then it reads the first UUID and refuses the rest.
/// One `0x04` entry record at `(1, index)`.
fn record(index: u64, body: ourios_wal::EntryBody<'_>) -> Vec<u8> {
    let log_id = ourios_wal::RaftLogId { term: 1, index };
    ourios_wal::ReplicatedRecord::Entry { log_id, body }
        .encode()
        .expect("encode")
}

/// A flusher assignment at `index` naming `replica`, then `edit`ed.
fn assignment(index: u64, replica: [u8; 16], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let control = ourios_wal::Control::FlusherAssignment(replica);
    let mut frame = record(index, ourios_wal::EntryBody::Control(control));
    edit(&mut frame);
    frame
}

/// Open group 0's log from a WAL holding a blank entry, then `frames`.
fn open_holding(
    frames: &[&[u8]],
) -> (
    std::io::Result<ourios_ingester::replicated::raft::log_store::GroupLog>,
    tempfile::TempDir,
) {
    use ourios_ingester::replicated::raft::log_store::{GroupLog, group_dir};
    use ourios_ingester::replicated::raft::vote_log::{self, VoteLog};

    let dir = tempfile::tempdir().expect("tempdir");
    let config = ourios_wal::WalConfig {
        root: group_dir(dir.path(), 0),
        batch_window_ms: 2,
        segment_size_bytes: ourios_wal::MIN_SEGMENT_SIZE_BYTES,
        segment_age_secs: 600,
        housekeeping_secs: 60,
        max_unlinks_per_pass: ourios_wal::DEFAULT_MAX_UNLINKS_PER_PASS,
        rotation_retry_attempts: ourios_wal::DEFAULT_ROTATION_RETRY_ATTEMPTS,
        macos_full_fsync: false,
    };
    let mut wal = ourios_wal::Wal::open(config.clone()).expect("the group WAL");
    let blank = record(0, ourios_wal::EntryBody::Blank);
    for frame in std::iter::once(&blank[..]).chain(frames.iter().copied()) {
        wal.append(ourios_wal::FrameKind::ReplicatedEntry, frame)
            .expect("append");
    }
    wal.sync().expect("sync");
    drop(wal);
    vote_log::create(dir.path()).expect("vote log");
    let votes = VoteLog::open(dir.path()).expect("vote log");
    (GroupLog::open(config, 0, votes), dir)
}

#[tokio::test]
async fn rfc0065_29_an_unreadable_control_entry_stops_the_open() {
    use openraft::EntryPayload;
    use openraft::storage::RaftLogReader as _;
    use ourios_ingester::replicated::raft::types::GroupEntry;
    use ourios_wal::Control;

    let flusher = *uuid::Uuid::from_u128(0x0190_b3c8_1a2b_7c3d_9e4f_5060_7080_a0b0).as_bytes();
    let first = assignment(1, flusher, |_| {});
    let (opened, _dir) = open_holding(&[&first]);
    let mut log = opened.expect("a log holding a flusher assignment opens");
    let entries = log.try_get_log_entries(..).await.expect("read");
    let assigned = entries.iter().find_map(|e| match &e.payload {
        EntryPayload::Normal(GroupEntry::Control(Control::FlusherAssignment(replica))) => {
            Some(*replica)
        }
        _ => None,
    });
    assert_eq!(assigned, Some(flusher), "the first assignment's UUID");

    let unknown_kind = assignment(2, [7; 16], |f| f[17] = Control::HIGHEST_KIND + 1);
    let short_body = assignment(2, [7; 16], |f| f.truncate(f.len() - 1));
    let long_body = assignment(2, [7; 16], |f| f.push(0));
    let kind = format!("control kind 0x{:02x}", Control::HIGHEST_KIND + 1);
    for (unreadable, why) in [
        (&unknown_kind, kind.as_str()),
        (&short_body, "body is 32 byte(s); expected 33"),
        (&long_body, "body is 34 byte(s); expected 33"),
    ] {
        let (refused, _dir) = open_holding(&[&first, unreadable]);
        let Err(error) = refused else {
            panic!("{why}: the entry did not stop the open");
        };
        assert!(error.to_string().contains(why), "{error}");
    }
}

/// Scenario RFC0065.29 — the batch-record arm (B11): a batch under tag
/// `0x02`, or under tag `0x05` with a `time` above `2^63 - 1`, stops the
/// open at that record.
#[test]
#[ignore = "RFC0065.29 stub — the batch-record arm, implemented with the timed batch record"]
fn rfc0065_29_an_untimed_or_out_of_range_batch_refuses_the_log() {
    todo!(
        "RFC0065.29 — a group WAL holding a batch under tag 0x02, or under tag 0x05 \
         with a time above 2^63 - 1, is refused at that record"
    );
}

/// Scenario RFC0065.29 — the batch-record arm (B11): a log holding a timed
/// batch opens with its time, and a batch under tag `0x02`, or under tag
/// `0x05` with a `time` above `2^63 - 1`, stops the open at that record.
#[tokio::test]
async fn rfc0065_29_an_untimed_or_out_of_range_batch_stops_the_open() {
    use openraft::EntryPayload;
    use openraft::storage::RaftLogReader as _;
    use ourios_ingester::replicated::raft::types::GroupEntry;
    use ourios_wal::BatchTime;

    let payload = ourios_wal::TenantBatch::encode("acme", b"export").expect("batch");
    let time = BatchTime::from_nanos(1_700_000_000_000_000_000).expect("in range");
    let timed = |index| {
        record(
            index,
            ourios_wal::EntryBody::Batch {
                time,
                batch: &payload,
            },
        )
    };
    let first = timed(1);
    let (opened, _dir) = open_holding(&[&first]);
    let mut log = opened.expect("a log holding a timed batch opens");
    let entries = log.try_get_log_entries(..).await.expect("read");
    let read = entries.iter().find_map(|e| match &e.payload {
        EntryPayload::Normal(GroupEntry::Batch(batch)) => Some(batch.time),
        _ => None,
    });
    assert_eq!(read, Some(time), "the batch's time");

    let mut untimed = vec![0x02];
    untimed.extend_from_slice(&timed(2)[1..17]);
    untimed.extend_from_slice(&payload);
    let mut late = timed(2);
    late[17..25].copy_from_slice(&(BatchTime::MAX.nanos() + 1).to_le_bytes());
    for (unreadable, why) in [
        (&untimed, "unknown ReplicatedEntry tag 0x02"),
        (&late, "is above 2^63 - 1"),
    ] {
        let (refused, _dir) = open_holding(&[&first, unreadable]);
        let Err(error) = refused else {
            panic!("{why}: the batch did not stop the open");
        };
        assert!(error.to_string().contains(why), "{error}");
    }
}

/// Scenario RFC0065.29 — the undecodable-batch arm (B11): a committed batch
/// whose export does not decode stops the group's miner at that entry on
/// every tail replica alike, while the group keeps acknowledging.
#[test]
#[ignore = "RFC0065.29 stub — the undecodable-batch arm, implemented with mining on tail replicas"]
fn rfc0065_29_an_undecodable_batch_stops_the_groups_miner() {
    todo!(
        "RFC0065.29 — the flusher, another tail replica and one restored before the \
         entry all stop there: none mines, publishes, proposes a cut at or above it or \
         serves a tail row past it, each logs the group, log id and decode error, a \
         query reports the group in `tail_unavailable`, and acks and log replication \
         continue"
    );
}

/// A miner configuration entry at `index` carrying `config`.
fn miner_config(index: u64, config: ourios_wal::MinerConfigV1) -> Vec<u8> {
    let control = ourios_wal::Control::MinerConfig(config);
    record(index, ourios_wal::EntryBody::Control(control))
}

fn default_miner_config() -> ourios_wal::MinerConfigV1 {
    ourios_wal::MinerConfigV1 {
        similarity_threshold: 0.7,
        similarity_floor: 0.4,
        param_byte_limit: 256,
        prefix_depth: 2,
        max_node_children: 100,
        max_templates: 20_000,
        max_line_tokens: 512,
        upstream_templates: ourios_wal::UpstreamTemplates::Ignore,
        upstream_template_byte_limit: 8192,
        upstream_association_limit: 4,
    }
}

/// Scenario RFC0065.31, the refused-configuration arm: a miner
/// configuration body that `MinerConfig` refuses stops the open as
/// RFC0065.29's unreadable entry does, as does one whose body is not 28
/// bytes or whose `upstream_templates` is not 0, 1 or 2.
#[tokio::test]
async fn rfc0065_31_a_miner_configuration_minerconfig_refuses_stops_the_open() {
    let accepted = miner_config(1, default_miner_config());
    let (opened, _dir) = open_holding(&[&accepted]);
    opened.expect("a log holding a valid configuration opens");

    let refused = |edit: fn(&mut ourios_wal::MinerConfigV1)| {
        let mut config = default_miner_config();
        edit(&mut config);
        miner_config(2, config)
    };
    let above_one = refused(|c| c.similarity_threshold = 1.5);
    let floor_above = refused(|c| c.similarity_floor = 0.9);
    let over_ceiling = refused(|c| c.param_byte_limit = 1025);
    let too_deep = refused(|c| c.prefix_depth = 9);
    let no_children = refused(|c| c.max_node_children = 0);
    let mut short_body = accepted.clone();
    short_body.pop();
    let mut long_body = accepted.clone();
    long_body.push(0);
    let mut unknown_mode = accepted.clone();
    unknown_mode[17 + 1 + 21] = 3;
    for (unreadable, why) in [
        (&above_one, "similarity_threshold"),
        (&floor_above, "similarity_floor"),
        (&over_ceiling, "param_byte_limit"),
        (&too_deep, "prefix_depth"),
        (&no_children, "max_node_children"),
        (&short_body, "body is 44 byte(s); expected 45"),
        (&long_body, "body is 46 byte(s); expected 45"),
        (&unknown_mode, "upstream_templates 3"),
    ] {
        let (refused, _dir) = open_holding(&[&accepted, unreadable]);
        let Err(error) = refused else {
            panic!("{why}: the entry did not stop the open");
        };
        assert!(error.to_string().contains(why), "{error}");
    }
}
