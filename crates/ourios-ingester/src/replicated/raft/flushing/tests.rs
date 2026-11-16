use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ourios_core::record::MinedRecord;
use ourios_parquet::Reader;

use tokio::sync::watch;

use super::super::kept::Bounds;
use super::super::kept::tests::{entries, mined_log};
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
}

/// A tail replica of `GROUP` over a local store, its mined log ready to
/// step through, and `formed.json` written when `formed`.
fn fixture(formed: bool, bounds: Bounds) -> Fixture {
    let tmp = tempfile::TempDir::new().expect("temp");
    let root = tmp.path().to_path_buf();
    let store = Store::local(&root).expect("store");
    if formed {
        record_formation(&store);
    }
    let source = Source {
        store: store.clone(),
        voters: VOTERS.map(str::to_owned).to_vec(),
        flush: FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: Duration::MAX,
            ceiling_bytes: usize::MAX,
        },
        promoted: PromotedAttributes::default(),
        audit_ceiling: usize::MAX,
    };
    Fixture {
        _tmp: tmp,
        root,
        store,
        flushing: Flushing::new(me(), GROUP, source),
        kept: Kept::new(bounds),
        log: mined_log(&entries(8)),
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
            self.flushing.step(named, mined, &self.kept);
        }
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
