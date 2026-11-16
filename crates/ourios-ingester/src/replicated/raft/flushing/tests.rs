use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ourios_core::record::MinedRecord;
use ourios_parquet::Reader;

use tokio::sync::watch;

use super::super::kept::Bounds;
use super::super::kept::tests::{entries, mined_log};
use super::super::log_ids::gated::{Outage, with_outage};
use super::super::mining::tests::{HeldLog, logged};
use super::super::mining::tests::{batch_at, block, configured, corpus, time};
use super::super::types::{Entry, GroupEntry, LogId};
use super::*;

const GROUP: u32 = 3;
const LOG: u32 = 41;
const VOTERS: [&str; 3] = ["n0", "n1", "n2"];

fn me() -> NodeId {
    NodeId::from(7)
}

fn other() -> NodeId {
    NodeId::from(8)
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    store: Store,
    flushing: Flushing,
    kept: Kept,
    log: Vec<(u64, Mined)>,
    /// Takes down the store the flusher publishes to.
    outage: Outage,
}

/// A tail replica of `GROUP` over a local store, its mined log ready to
/// step through, and `formed.json` written when `formed`.
fn fixture(formed: bool, bounds: Bounds) -> Fixture {
    fixture_with(formed, Setup::bounded(bounds))
}

/// As [`fixture`], its record sink's size trigger at `target_bytes`.
fn fixture_sliced(formed: bool, bounds: Bounds, target_bytes: usize) -> Fixture {
    fixture_with(
        formed,
        Setup {
            target_bytes,
            ..Setup::bounded(bounds)
        },
    )
}

/// How a [`fixture_with`] replica is set up.
struct Setup {
    bounds: Bounds,
    target_bytes: usize,
    /// The group's log from index 1.
    entries: Vec<GroupEntry>,
}

impl Setup {
    fn bounded(bounds: Bounds) -> Self {
        Self {
            bounds,
            target_bytes: usize::MAX,
            entries: entries(8),
        }
    }
}

fn fixture_with(formed: bool, setup: Setup) -> Fixture {
    let Setup {
        bounds,
        target_bytes,
        entries,
    } = setup;
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path().to_path_buf();
    let store = Store::local(&root).expect("store");
    if formed {
        record_formation(&store);
    }
    let (publishing, outage) = with_outage(store.clone());
    let source = Source {
        store: publishing,
        voters: VOTERS.map(str::to_owned).to_vec(),
        flush: FlushConfig {
            target_bytes,
            max_buffer_age: Duration::MAX,
            ceiling_bytes: usize::MAX,
        },
        promoted: PromotedAttributes::default(),
        audit_ceiling: usize::MAX,
    };
    let mut held = HeldLog(
        logged(&entries)
            .into_iter()
            .map(|e| (e.log_id.index, e))
            .collect(),
    );
    // A group's log starts at 0, with the first membership.
    let first = Entry {
        log_id: LogId {
            index: 0,
            ..held.0[&1].log_id
        },
        payload: openraft::EntryPayload::Blank,
    };
    held.0.insert(0, first);
    Fixture {
        _tmp: tmp,
        root,
        store,
        flushing: Flushing::new(me(), GROUP, source, Arc::new(held)),
        kept: Kept::new(bounds),
        log: mined_log(&entries),
        outage,
    }
}

fn record_formation(store: &Store) {
    let members: BTreeMap<String, NodeId> = (0..)
        .zip(VOTERS)
        .map(|(n, voter)| (voter.to_owned(), NodeId::from(100 + n)))
        .collect();
    let formation = Formation::new(GROUP, LOG, members, &VOTERS).expect("formation");
    let bytes = formation.encode().expect("encode");
    store
        .put_blocking(&formed_key(GROUP), bytes)
        .expect("record");
}

impl Fixture {
    /// Keep and step entries `range` of the log (by position in it), the
    /// first of them carrying `assigned`.
    fn step(&mut self, range: std::ops::Range<usize>, assigned: Option<NodeId>) {
        for (n, (index, mined)) in self.log[range].iter().enumerate() {
            self.kept.keep(*index, mined.clone());
            let named = if n == 0 { assigned } else { None };
            self.flushing.step(named, mined, &mut self.kept);
        }
    }

    /// As [`Self::step`], for a replica that dropped the output of every
    /// entry through `gap`.
    fn step_dropping(&mut self, range: std::ops::Range<usize>, assigned: Option<NodeId>, gap: u64) {
        for (n, (index, mined)) in self.log[range].iter().enumerate() {
            if *index <= gap {
                self.kept.gap_at(*index);
            } else {
                self.kept.keep(*index, mined.clone());
            }
            let named = if n == 0 { assigned } else { None };
            self.flushing.step(named, mined, &mut self.kept);
        }
    }

    /// The records the current replay holds buffered, unpublished.
    fn replay_buffered(&self) -> usize {
        self.flushing
            .publication
            .as_ref()
            .map_or(0, |publication| publication.buffered_records())
    }

    /// Flush the current publication, if any, and read every row on the
    /// store, in position order.
    fn published(&self) -> Vec<MinedRecord> {
        if let Some(publication) = self.flushing.current().borrow().clone() {
            assert!(publication.flush_all(), "the store took it");
        }
        let mut rows: Vec<_> = parquet_files(&self.root.join("data"))
            .iter()
            .flat_map(|file| {
                Reader::open_file(file)
                    .expect("open")
                    .read_all()
                    .expect("read")
            })
            .collect();
        rows.sort_by_key(|row| row.ingest_position);
        rows
    }

    /// The rows of log entries `range`, as published under `LOG`.
    fn rows_of(&self, range: std::ops::Range<usize>) -> Vec<MinedRecord> {
        self.log[range]
            .iter()
            .flat_map(|(_, mined)| &mined.rows)
            .map(|row| MinedRecord {
                ingest_position: Some(ourios_core::record::IngestPosition {
                    group: LOG,
                    seq: row.position.seq,
                    index: row.position.index,
                }),
                ..row.record.clone()
            })
            .collect()
    }
}

fn parquet_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                out.push(path);
            }
        }
    }
    out
}

/// RFC 0065 §6.7: a tail replica that applies an assignment naming
/// itself publishes everything it kept, then each later entry's rows,
/// every row under the log id `formed.json` records.
#[test]
fn a_named_replica_publishes_what_it_kept_then_each_step() {
    let mut f = fixture(true, Bounds::default());
    f.step(0..5, None);
    assert!(f.flushing.current().borrow().is_none());
    f.step(5..10, Some(me()));
    assert_eq!(f.published(), f.rows_of(0..10));
}

/// An assignment naming another replica stops publication at that entry:
/// what was already published stays, nothing later is, and the rows still
/// buffered are dropped with the publication, being kept for the new
/// flusher.
#[test]
fn an_assignment_naming_another_replica_stops_publication() {
    let mut f = fixture(true, Bounds::default());
    f.step(0..6, Some(me()));
    let before = f.published();
    assert_eq!(before, f.rows_of(0..6));
    f.step(6..8, None);
    f.step(8..10, Some(other()));
    assert!(f.flushing.current().borrow().is_none());
    assert_eq!(f.published(), before, "nothing from the reassignment on");
    assert_eq!(
        f.kept.rows().count(),
        f.rows_of(0..10).len(),
        "all still kept"
    );
}

/// Without a readable `formed.json` a named replica publishes nothing,
/// and once the record is there it publishes everything it kept.
#[test]
fn a_named_replica_waits_for_its_log_id() {
    let mut f = fixture(false, Bounds::default());
    f.step(0..6, Some(me()));
    assert!(f.flushing.current().borrow().is_none());
    assert!(parquet_files(&f.root.join("data")).is_empty());
    record_formation(&f.store);
    f.step(6..10, None);
    assert_eq!(f.published(), f.rows_of(0..10));
}

/// A replica with a gap publishes nothing; once a checkpoint at or above
/// the gap applies, it publishes everything it still keeps.
#[test]
#[ignore = "superseded: RFC 0065 §6.7, a named flusher with a gap and no checkpoint \
            replays and then publishes; see a_flusher_with_a_gap_replays_it_then_publishes_as_if_it_dropped_nothing"]
fn a_replica_with_a_gap_publishes_nothing_until_it_clears() {
    let mut f = fixture(
        true,
        Bounds {
            rows: 1,
            ..Bounds::default()
        },
    );
    f.step(0..6, Some(me()));
    let gap = f.kept.gap().expect("every row dropped");
    assert!(f.published().is_empty(), "a gap holds publication back");

    f.kept = Kept::default();
    f.kept.checkpoint(gap);
    let after = f
        .log
        .iter()
        .position(|(index, _)| *index > gap)
        .expect("later entries");
    f.step(after..10, None);
    assert_eq!(f.published(), f.rows_of(after..10));
}

/// A handle taken from `current()` before a reassignment cannot publish
/// after it: the publication's buffers are discarded, releasing their
/// accounting, and it refuses to flush. Those rows stay kept for the new
/// flusher.
#[test]
fn a_retained_handle_cannot_publish_after_a_reassignment() {
    let mut f = fixture(true, Bounds::default());
    f.step(0..6, Some(me()));
    let retained = f.flushing.current().borrow().clone().expect("live");
    f.step(6..8, Some(other()));
    assert_eq!(retained.buffered_records(), 0, "the buffers were discarded");
    assert!(!retained.flush_all(), "a held publication refuses to flush");
    assert!(
        parquet_files(&f.root.join("data")).is_empty(),
        "nothing published"
    );
}

/// A replica named again publishes through the same publication, so a
/// takeover cycle builds no second set of sinks and their metrics.
#[test]
fn a_replica_named_again_reuses_its_publication() {
    let mut f = fixture(true, Bounds::default());
    f.step(0..4, Some(me()));
    let first = f.flushing.current().borrow().clone().expect("live");
    f.step(4..6, Some(other()));
    f.step(6..10, Some(me()));
    let again = f.flushing.current().borrow().clone().expect("live");
    assert!(Arc::ptr_eq(&first, &again), "one publication for the group");
    assert_eq!(f.published(), f.rows_of(0..10));
}

/// A named replica whose `formed.json` it cannot use is held for that
/// reason, reported once; once the record is usable the hold clears and
/// it publishes everything it kept.
#[test]
fn a_named_replica_is_held_by_an_unusable_record_until_it_is_usable() {
    let mut f = fixture(false, Bounds::default());
    f.store
        .put_blocking(&formed_key(GROUP), b"not json".to_vec())
        .expect("write");
    f.step(0..4, Some(me()));
    assert_eq!(f.flushing.held(), Some("malformed"));
    record_formation(&f.store);
    f.step(4..10, None);
    assert_eq!(f.flushing.held(), None);
    assert_eq!(f.published(), f.rows_of(0..10));
}

/// The age sweep flushes a publication whose audit buffer reached its
/// ceiling at once, not at its next tick.
#[test]
fn a_sweep_flushes_an_audit_buffer_at_its_ceiling_before_its_tick() {
    let mut f = fixture(true, Bounds::default());
    f.flushing.source.audit_ceiling = 1;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let (stop, stopped) = watch::channel(false);
    let every = Duration::from_secs(30);
    let sweep = runtime.spawn(sweep(f.flushing.current(), every, stopped));
    // The first tick fires at once; publish only after it.
    std::thread::sleep(Duration::from_millis(100));
    f.step(0..6, Some(me()));
    let audit = f.root.join("audit");
    let flushed = (0..250).any(|_| {
        std::thread::sleep(Duration::from_millis(20));
        !parquet_files(&audit).is_empty()
    });
    stop.send_replace(true);
    runtime.block_on(sweep).expect("join");
    assert!(flushed, "no flush before the tick");
}

/// A named replica that cannot read its log id copies nothing it keeps:
/// the backlog is copied once, when its publication can be built.
#[test]
fn a_named_replica_copies_its_backlog_only_once_it_can_publish() {
    let mut f = fixture(false, Bounds::default());
    f.step(0..8, Some(me()));
    assert_eq!(
        f.flushing.backlog_copies, 0,
        "no copy while the log id is unread"
    );
    record_formation(&f.store);
    f.step(8..10, None);
    assert_eq!(f.flushing.backlog_copies, 1);
    assert_eq!(f.published(), f.rows_of(0..10));
}

/// The log index of the entry at `position` in the fixture's log.
fn index_at(f: &Fixture, position: usize) -> u64 {
    f.log[position].0
}

/// RFC 0065 §6.7, RFC0065.23: a named flusher whose drops came before any
/// checkpoint replays the group's log from its first entry through the
/// gap, then publishes what it keeps above it: the same rows as a
/// flusher that dropped nothing.
#[test]
fn a_flusher_with_a_gap_replays_it_then_publishes_as_if_it_dropped_nothing() {
    let mut f = fixture(true, Bounds::default());
    let gap = index_at(&f, 5);
    f.step_dropping(0..6, None, gap);
    f.step(6..10, Some(me()));
    assert_eq!(f.kept.gap(), None, "the replay reached the gap");
    assert_eq!(f.published(), f.rows_of(0..10));
}

/// RFC 0065 §6.7: the replay publishes a slice at a time, each made
/// durable before the next is mined, and publishes nothing it keeps until
/// it reaches the gap.
#[test]
fn a_gap_replay_publishes_one_slice_per_step() {
    let mut f = fixture_sliced(true, Bounds::default(), 1);
    let gap = index_at(&f, 5);
    f.step_dropping(0..6, None, gap);
    f.step(6..7, Some(me()));
    assert_eq!(f.kept.gap(), Some(gap), "one slice does not reach the gap");
    assert!(f.flushing.current().borrow().is_none(), "kept output waits");
    assert_eq!(f.replay_buffered(), 0, "the slice is durable");
    assert!(!parquet_files(&f.root.join("data")).is_empty());
    for position in 7..10 {
        f.step(position..position + 1, None);
        if f.kept.gap().is_some() {
            assert_eq!(f.replay_buffered(), 0, "each slice is durable");
        }
    }
    assert_eq!(f.kept.gap(), None);
    assert_eq!(f.published(), f.rows_of(0..10));
}

/// A checkpoint at or above the gap that applies during the replay ends
/// it: the gap clears and the flusher publishes what it keeps.
#[test]
fn a_checkpoint_at_or_above_the_gap_ends_the_replay() {
    let mut f = fixture_sliced(true, Bounds::default(), 1);
    let gap = index_at(&f, 5);
    f.step_dropping(0..6, None, gap);
    f.step(6..7, Some(me()));
    f.kept.checkpoint(gap);
    f.step(7..10, None);
    assert_eq!(f.kept.gap(), None);
    let published = f.published();
    for row in f.rows_of(6..10) {
        assert!(
            published.contains(&row),
            "kept rows above the gap published"
        );
    }
}

/// A checkpoint below the gap whose cut the replay has already mined past
/// leaves the replay going: it read every entry the purge can remove.
#[test]
fn a_checkpoint_the_replay_has_passed_does_not_stop_it() {
    let mut f = fixture_sliced(true, Bounds::default(), 1);
    let gap = index_at(&f, 5);
    f.step_dropping(0..6, None, gap);
    f.step(6..7, Some(me()));
    f.kept.checkpoint(index_at(&f, 1));
    f.step(7..10, None);
    assert_eq!(f.flushing.held(), None);
    assert_eq!(f.kept.gap(), None);
    assert_eq!(f.published(), f.rows_of(0..10));
}

/// RFC 0065 §6.7: a checkpoint below the gap that the replay has not
/// mined past means restarting from that checkpoint's bundle, which this
/// replica cannot restore yet, so it holds, publishing nothing it keeps.
#[test]
fn a_checkpoint_below_the_gap_ahead_of_the_replay_holds_it() {
    let mut f = fixture_sliced(true, Bounds::default(), 1);
    let gap = index_at(&f, 5);
    f.step_dropping(0..6, None, gap);
    f.step(6..7, Some(me()));
    f.kept.checkpoint(index_at(&f, 4));
    f.step(7..10, None);
    assert_eq!(f.flushing.held(), Some("restore_unavailable"));
    assert!(f.flushing.current().borrow().is_none());
    assert_eq!(f.kept.gap(), Some(gap));
}

/// A gap above an applied checkpoint is rebuilt from that checkpoint's
/// bundle, which this replica cannot restore yet: it holds.
#[test]
fn a_gap_above_a_checkpoint_holds_until_restore() {
    let mut f = fixture(true, Bounds::default());
    f.kept.checkpoint(index_at(&f, 1));
    let gap = index_at(&f, 5);
    f.step_dropping(2..6, None, gap);
    f.step(6..10, Some(me()));
    assert_eq!(f.flushing.held(), Some("restore_unavailable"));
    assert!(parquet_files(&f.root.join("data")).is_empty());
}

/// A replay that cannot read an entry stops, holding the flusher with the
/// replay's `error.type`, and does not start again from the top at each
/// later entry.
#[test]
fn a_replay_that_cannot_read_its_log_holds_the_flusher() {
    let mut f = fixture(true, Bounds::default());
    f.flushing = Flushing::new(
        me(),
        GROUP,
        f.flushing.source.clone(),
        Arc::new(HeldLog(BTreeMap::new())),
    );
    let gap = index_at(&f, 5);
    f.step_dropping(0..6, None, gap);
    f.step(6..10, Some(me()));
    assert_eq!(f.flushing.held(), Some("purged"));
    assert!(parquet_files(&f.root.join("data")).is_empty());
}

/// RFC 0065 §6.7: batches held for a late first configuration are read
/// after the replay has moved past the configuration, so a checkpoint at
/// or above them but below the gap can purge what the replay still needs:
/// it holds for the checkpoint's bundle instead of reading on.
#[test]
fn a_checkpoint_over_batches_the_replay_still_holds_stops_it() {
    let lines = corpus();
    let mut entries = vec![block(0, 10_000)];
    let chunks: Vec<_> = (1..).zip(lines.chunks(5)).take(6).collect();
    for (n, chunk) in &chunks[..3] {
        entries.push(batch_at(time(*n), "acme", chunk));
    }
    entries.push(configured(0.7));
    for (n, chunk) in &chunks[3..] {
        entries.push(batch_at(time(*n), "acme", chunk));
    }
    let mut f = fixture_with(
        true,
        Setup {
            target_bytes: 1,
            entries,
            ..Setup::bounded(Bounds::default())
        },
    );
    // Positions: the block at 1, the configuration at 5 mining 2 to 4.
    let configuration = 5;
    let gap = f.log.last().expect("entries").0;
    let last = f.log.len();
    f.step_dropping(0..last, None, gap);
    f.step(last - 1..last, Some(me()));
    assert_eq!(f.kept.gap(), Some(gap), "one held batch replayed");
    f.kept.checkpoint(configuration);
    f.step(last - 1..last, None);
    assert_eq!(f.flushing.held(), Some("restore_unavailable"));
}

/// A replay abandoned with a slice the store refused, because another
/// replica was named, leaves nothing behind once this one is named again:
/// the fresh replay holds only its own slice, so the refused one is
/// published once, by it. Slices already durable repeat only their
/// positions.
#[test]
fn a_fresh_replay_starts_without_an_abandoned_slice() {
    let mut f = fixture_sliced(true, Bounds::default(), 1);
    let gap = index_at(&f, 5);
    f.step_dropping(0..6, None, gap);
    f.step(6..7, Some(me()));
    f.outage.begin();
    f.step(7..8, None);
    assert!(
        f.replay_buffered() > 0,
        "the refused slice is still buffered"
    );
    f.step(8..9, Some(other()));
    f.outage.end();
    f.step(9..10, Some(me()));
    while f.kept.gap().is_some() {
        f.flushing.step(None, &Mined::default(), &mut f.kept);
    }
    let published = f.published();
    let refused = index_at(&f, 3);
    let of_refused = |row: &&MinedRecord| row.ingest_position.is_some_and(|p| p.seq == refused);
    assert_eq!(
        published.iter().filter(of_refused).count(),
        f.rows_of(3..4).len(),
        "the refused slice is published by the fresh replay alone"
    );
    let mut once = published;
    once.dedup_by_key(|row| row.ingest_position);
    assert_eq!(once, f.rows_of(0..10));
}

/// A reassignment during a replay discards what the replay handed on and
/// the store refused: a handle retained from before the gap, such as the
/// age sweep's, cannot publish it after another replica is named.
#[test]
fn a_retained_handle_cannot_publish_a_refused_replay_slice_after_a_reassignment() {
    let mut f = fixture_sliced(true, Bounds::default(), 1);
    f.step(0..2, Some(me()));
    let retained = f.flushing.current().borrow().clone().expect("live");
    let gap = index_at(&f, 5);
    f.step_dropping(2..6, None, gap);
    f.outage.begin();
    f.step(6..7, None);
    assert!(
        retained.buffered_records() > 0,
        "the refused slice is buffered"
    );
    f.step(7..8, Some(other()));
    f.outage.end();
    assert_eq!(retained.buffered_records(), 0, "the buffers were discarded");
    assert!(!retained.flush_all(), "a held publication refuses to flush");
}
