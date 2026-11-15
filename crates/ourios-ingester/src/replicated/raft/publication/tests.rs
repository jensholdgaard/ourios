use std::path::{Path, PathBuf};
use std::time::Duration;

use ourios_parquet::Reader;
use ourios_parquet::store::Store;

use super::super::kept::tests::{entries, mined_log};
use super::*;
use crate::audit_sink::{BufferingAuditSink, SharedParquetAuditSink};
use crate::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};

/// The log id the tests publish under.
const LOG: u32 = 7;

struct Stores {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    audit: PathBuf,
    publication: Publication,
}

/// A publication over local data and audit stores whose triggers never
/// fire on their own.
fn stores() -> Stores {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (data, audit) = (tmp.path().join("data"), tmp.path().join("audit"));
    std::fs::create_dir_all(&data).expect("data root");
    std::fs::create_dir_all(&audit).expect("audit root");
    let held = FlushConfig {
        target_bytes: usize::MAX,
        max_buffer_age: Duration::MAX,
        ceiling_bytes: usize::MAX,
    };
    let records = SharedParquetSink::new(ParquetRecordSink::new(
        Store::local(&data).expect("data store"),
        held,
    ));
    let events = SharedParquetAuditSink::new(BufferingAuditSink::new(
        Store::local(&audit).expect("audit store"),
        usize::MAX,
    ));
    Stores {
        _tmp: tmp,
        data,
        audit,
        publication: Publication::new(LOG, PublishCoordinator::new(records, events)),
    }
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
    std::fs::remove_dir_all(&stores.audit).expect("remove audit root");
    std::fs::write(&stores.audit, b"not a directory").expect("sabotage");

    assert!(!stores.publication.flush_all(), "the audit write failed");
    assert!(
        parquet_files(&stores.data).is_empty(),
        "no rows ahead of their events"
    );
}
