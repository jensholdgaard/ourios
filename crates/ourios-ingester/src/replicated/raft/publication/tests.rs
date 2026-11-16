use std::path::{Path, PathBuf};
use std::time::Duration;

use ourios_parquet::Reader;
use ourios_parquet::store::Store;

use super::super::kept::tests::{entries, mined_log};
use super::held::first_put_held;
use super::*;
use crate::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use crate::record_sink::{FlushConfig, ParquetRecordSink};

/// The log id the tests publish under.
const LOG: u32 = 7;

struct Stores {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    audit: PathBuf,
    publication: Publication,
}

/// A publication over local data and audit stores whose record sink
/// flushes on its own under `config`.
fn stores_with(config: FlushConfig) -> Stores {
    stores_over(config, |store| store)
}

/// [`stores_with`], its data store wrapped by `wrap`.
fn stores_over(config: FlushConfig, wrap: impl FnOnce(Store) -> Store) -> Stores {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (data, audit) = (tmp.path().join("data"), tmp.path().join("audit"));
    std::fs::create_dir_all(&data).expect("data root");
    std::fs::create_dir_all(&audit).expect("audit root");
    let records = ParquetRecordSink::new(wrap(Store::local(&data).expect("data store")), config);
    let events = SharedParquetAuditSink::new(BufferingAuditSink::new(
        Store::local(&audit).expect("audit store"),
        usize::MAX,
    ));
    Stores {
        _tmp: tmp,
        data,
        audit,
        publication: Publication::new(LOG, records, events),
    }
}

/// Triggers that never fire on their own.
const HELD_BACK: FlushConfig = FlushConfig {
    target_bytes: usize::MAX,
    max_buffer_age: Duration::MAX,
    ceiling_bytes: usize::MAX,
};

/// [`stores_with`] [`HELD_BACK`].
fn stores() -> Stores {
    stores_with(HELD_BACK)
}

/// Every record in the files under `root`.
fn published(root: &Path) -> Vec<MinedRecord> {
    parquet_files(root)
        .iter()
        .flat_map(|file| {
            Reader::open_file(file)
                .expect("open")
                .read_all()
                .expect("read")
        })
        .collect()
}

/// Make the audit store refuse every write.
fn fail_audit(stores: &Stores) {
    std::fs::remove_dir_all(&stores.audit).expect("remove audit root");
    std::fs::write(&stores.audit, b"not a directory").expect("sabotage");
}

fn parquet_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
        {
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

fn kept_of(log: &[(u64, super::super::mining::Mined)]) -> Kept {
    let mut kept = Kept::default();
    for (index, mined) in log {
        kept.keep(*index, mined.clone());
    }
    kept
}

/// RFC 0065 §6.6: the flusher publishes every row it kept, each carrying
/// its position in the group's log as `ingest_group`, `ingest_seq` and
/// `ingest_index`, and the audit events with them.
#[test]
fn a_flusher_publishes_its_kept_rows_with_their_positions_in_the_log() {
    let stores = stores();
    let log = mined_log(&entries(10));
    let kept = kept_of(&log);
    stores.publication.publish_kept(&kept);
    assert!(
        stores.publication.flush_all(),
        "a healthy store takes it all"
    );

    let mut published: Vec<MinedRecord> = parquet_files(&stores.data)
        .iter()
        .flat_map(|file| {
            Reader::open_file(file)
                .expect("open")
                .read_all()
                .expect("read")
        })
        .collect();
    published.sort_by_key(|record| record.ingest_position);
    let expected: Vec<MinedRecord> = kept
        .rows()
        .map(|row| MinedRecord {
            ingest_position: Some(IngestPosition {
                group: LOG,
                seq: row.position.seq,
                index: row.position.index,
            }),
            ..row.record.clone()
        })
        .collect();
    assert_eq!(published.len(), 50);
    assert_eq!(published, expected);
    assert!(
        !parquet_files(&stores.audit).is_empty(),
        "the audit events too"
    );
}

/// Audit-before-record: with the audit store failing, no file of rows is
/// published, and both stay buffered for the next attempt.
#[test]
fn no_row_is_published_before_its_audit_events() {
    let stores = stores();
    let kept = kept_of(&mined_log(&entries(4)));
    stores.publication.publish_kept(&kept);
    fail_audit(&stores);

    assert!(!stores.publication.flush_all(), "the audit write failed");
    assert!(
        parquet_files(&stores.data).is_empty(),
        "no rows ahead of their events"
    );
}

/// The record sink's own size trigger is held back by the audit barrier:
/// with the audit store failing, publishing rows past the size target
/// writes no file of them.
#[test]
fn an_inline_flush_waits_for_the_audit_events() {
    let stores = stores_with(FlushConfig {
        target_bytes: 1,
        max_buffer_age: Duration::MAX,
        ceiling_bytes: 1,
    });
    fail_audit(&stores);
    stores
        .publication
        .publish_kept(&kept_of(&mined_log(&entries(4))));
    assert!(
        parquet_files(&stores.data).is_empty(),
        "no rows ahead of their events"
    );
}

/// A drain waits for a publish in progress, so it never takes a step's
/// audit events without the rows handed over with them: a drain started
/// once the first row is taken has not finished 200 ms later.
#[test]
fn a_drain_waits_for_the_publish_in_progress() {
    let stores = stores();
    let kept = kept_of(&mined_log(&entries(4)));
    let publication = &stores.publication;
    std::thread::scope(|scope| {
        let mut drain = None;
        let mut finished_early = false;
        let rows = kept.rows().inspect(|_| {
            if drain.is_none() {
                let started = scope.spawn(move || publication.flush_aged());
                std::thread::sleep(Duration::from_millis(200));
                finished_early = started.is_finished();
                drain = Some(started);
            }
        });
        publication.publish(kept.audit(), rows);
        assert!(!finished_early, "a drain finished mid-publish");
        let drain = drain.expect("the drain ran");
        assert!(drain.join().expect("join"), "the drain then published");
    });
}

/// On a healthy store, crossing the size target during one publish writes
/// files before it returns: the audit barrier flushes the events first
/// rather than holding the rows back.
#[test]
fn a_publish_past_the_size_target_writes_files_before_it_returns() {
    let stores = stores_with(FlushConfig {
        target_bytes: 1,
        max_buffer_age: Duration::MAX,
        ceiling_bytes: 1,
    });
    let kept = kept_of(&mined_log(&entries(4)));
    stores.publication.publish_kept(&kept);
    assert_eq!(
        published(&stores.data).len(),
        kept.rows().count(),
        "every row published"
    );
}

/// A full flush waits for an age flush still writing: one whose data write
/// fails after the full flush began has its rows back in the buffers,
/// and the full flush publishes them before it reports success.
#[test]
fn a_full_flush_waits_for_an_earlier_flush_still_writing() {
    let mut held = None;
    let aged_now = FlushConfig {
        max_buffer_age: Duration::ZERO,
        ..HELD_BACK
    };
    let stores = stores_over(aged_now, |store| {
        let (store, gate) = first_put_held(store);
        held = Some(gate);
        store
    });
    let held = held.expect("held");
    let kept = kept_of(&mined_log(&entries(4)));
    stores.publication.publish_kept(&kept);
    let publication = &stores.publication;
    std::thread::scope(|scope| {
        let aged = scope.spawn(|| publication.flush_aged());
        held.reached.recv().expect("the age flush is writing");
        let full = scope.spawn(|| publication.flush_all());
        std::thread::sleep(Duration::from_millis(200));
        let waited = !full.is_finished();
        held.release.send(()).expect("released");
        assert!(waited, "the full flush did not wait");
        assert!(!aged.join().expect("join"), "the held write failed");
        assert!(full.join().expect("join"), "the full flush published");
    });
    assert_eq!(published(&stores.data).len(), kept.rows().count());
}
