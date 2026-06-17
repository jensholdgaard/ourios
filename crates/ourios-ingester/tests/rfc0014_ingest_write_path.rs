//! RFC 0014 — ingest write path (record sink + flush policy) acceptance
//! scenarios (§5).
//!
//! `.1`–`.4`/`.6` drive the [`ParquetRecordSink`] directly against a
//! `LocalFileSystem`-backed [`Store`] (synthetic `MinedRecord` streams);
//! reading the flushed objects back proves no loss + tenant isolation. `.5`
//! (crash recovery) stays `#[ignore]`d until the sink is wired into the ingest
//! pipeline and the RFC 0008 WAL crash harness is extended (`green`, part 2).
//!
//! See `docs/rfcs/0014-ingest-write-path.md` §5/§6.

use std::path::Path;
use std::time::Duration;

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param, RecordSink};
use ourios_core::tenant::TenantId;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink};
use ourios_parquet::{Reader, Store};

/// A clean-round-trip record for `tenant` at in-hour offset `i`.
fn rec_for(tenant: &str, i: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(tenant),
        template_id: 1,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_string()),
        scope_name: Some("lib.cart".to_string()),
        scope_version: Some("1.0.0".to_string()),
        time_unix_nano: 1_775_127_480_000_000_000 + i * 1_000,
        observed_time_unix_nano: Some(1_775_127_480_000_000_000 + i * 1_000 + 1),
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: format!("{i}"),
        }],
        separators: vec![String::new(), " ".to_string()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    }
}

/// Every flushed `*.parquet` object under `root`, one inner `Vec` per file
/// (so per-file tenant isolation can be asserted).
fn parquet_files(root: &Path) -> Vec<Vec<MinedRecord>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "parquet") {
                let rows = Reader::open_file(&path)
                    .expect("open_file")
                    .read_all()
                    .expect("read_all");
                files.push(rows);
            }
        }
    }
    files
}

fn all_rows(root: &Path) -> Vec<MinedRecord> {
    parquet_files(root).into_iter().flatten().collect()
}

fn sink(dir: &Path, config: FlushConfig) -> ParquetRecordSink {
    ParquetRecordSink::new(Store::local(dir).expect("local store"), config)
}

const HUGE: usize = 1 << 40;
const FOREVER: Duration = Duration::from_secs(86_400);

/// Scenario RFC0014.1 — Size trigger: the emit that crosses the size target
/// flushes the partition to one right-sized Parquet object.
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
fn rfc0014_1_size_trigger() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut s = sink(
        dir.path(),
        FlushConfig {
            target_bytes: 500,
            max_buffer_age: FOREVER,
            ceiling_bytes: HUGE,
        },
    );
    let records: Vec<MinedRecord> = (0..50).map(|i| rec_for("tenant-a", i)).collect();
    for r in &records {
        s.emit(r.clone());
    }
    // The size trigger alone (no age/rotation flush called) has fired.
    assert!(s.flushes() >= 1, "size trigger flushed mid-stream");
    // Drain the buffered tail, then confirm no loss across the flushed objects.
    s.flush_all();
    let mut got = all_rows(dir.path());
    assert_eq!(
        got.len(),
        records.len(),
        "every record published, none lost"
    );
    got.sort_by_key(|r| r.params[0].value.parse::<u64>().unwrap_or_default());
    assert_eq!(got, records, "rows recovered byte-for-byte");
}

/// Scenario RFC0014.2 — Age trigger: a sub-target low-volume partition flushes
/// on the next batch-window tick once its oldest record reaches `max_buffer_age`.
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
fn rfc0014_2_age_trigger() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    // Size never triggers; age is inclusive at zero, so any buffered record is
    // immediately "aged" — `flush_aged` (the tick) is the only thing that flushes.
    let mut s = sink(
        dir.path(),
        FlushConfig {
            target_bytes: HUGE,
            max_buffer_age: Duration::ZERO,
            ceiling_bytes: HUGE,
        },
    );
    for i in 0..5 {
        s.emit(rec_for("tenant-a", i));
    }
    assert_eq!(
        s.flushes(),
        0,
        "size/ceiling did not flush a low-volume partition"
    );
    s.flush_aged();
    assert_eq!(s.flushes(), 1, "the age sweep flushed the partition");
    assert_eq!(all_rows(dir.path()).len(), 5);
}

/// Scenario RFC0014.3 — Rotation force-flush: a WAL segment rotation flushes
/// every partition (including sub-threshold ones); nothing un-flushed predates
/// the sealed segment.
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
fn rfc0014_3_rotation_force_flush() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut s = sink(
        dir.path(),
        FlushConfig {
            target_bytes: HUGE,
            max_buffer_age: FOREVER,
            ceiling_bytes: HUGE,
        },
    );
    // Three tenants → three partitions, all below every other trigger.
    for t in ["tenant-x", "tenant-y", "tenant-z"] {
        for i in 0..3 {
            s.emit(rec_for(t, i));
        }
    }
    assert_eq!(s.flushes(), 0, "nothing flushed before rotation");
    assert_eq!(s.buffered_partitions(), 3);

    s.flush_all(); // the WAL-segment-rotation trigger

    assert_eq!(s.flushes(), 3, "every partition flushed on rotation");
    assert_eq!(
        s.buffered_partitions(),
        0,
        "no buffered record predates the seal"
    );
    assert_eq!(all_rows(dir.path()).len(), 9);
}

/// Scenario RFC0014.4 — Bounded memory: the sink flushes inline so buffered
/// bytes never exceed the hard ceiling; nothing is lost.
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
fn rfc0014_4_bounded_memory() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    // Size/age never trigger; only the ceiling does. 100 records far exceed a
    // 1 KiB ceiling, so the sink must flush inline to stay bounded.
    let ceiling = 1024;
    let mut s = sink(
        dir.path(),
        FlushConfig {
            target_bytes: HUGE,
            max_buffer_age: FOREVER,
            ceiling_bytes: ceiling,
        },
    );
    let n = 100;
    for i in 0..n {
        s.emit(rec_for("tenant-a", i));
        assert!(
            s.buffered_bytes() <= ceiling,
            "ceiling held after each emit: {} <= {ceiling}",
            s.buffered_bytes(),
        );
    }
    assert!(s.flushes() >= 1, "the ceiling forced inline flushes");
    s.flush_all();
    assert_eq!(
        all_rows(dir.path()).len() as u64,
        n,
        "no loss under backpressure"
    );
}

/// Scenario RFC0014.5 — No acknowledged-data loss: a crash with a non-empty
/// buffer loses nothing — WAL replay re-mines every un-flushed acknowledged
/// record (`CLAUDE.md` §3.4).
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
#[ignore = "RFC0014.5 — green part 2: wire the sink into the ingest pipeline + extend the RFC 0008 crash harness"]
fn rfc0014_5_no_acknowledged_data_loss() {
    todo!("RFC0014.5: crash mid-buffer loses no acknowledged data (WAL replay)")
}

/// Scenario RFC0014.6 — Tenant isolation: a flush produces an object holding
/// only one tenant's rows; no buffer or flush crosses tenants (`CLAUDE.md` §3.7).
/// See `docs/rfcs/0014-ingest-write-path.md` §5.
#[test]
fn rfc0014_6_tenant_isolation() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let mut s = sink(
        dir.path(),
        FlushConfig {
            target_bytes: HUGE,
            max_buffer_age: FOREVER,
            ceiling_bytes: HUGE,
        },
    );
    for i in 0..10 {
        s.emit(rec_for("tenant-x", i));
        s.emit(rec_for("tenant-y", i));
    }
    s.flush_all();

    let files = parquet_files(dir.path());
    assert!(!files.is_empty(), "objects were published");
    for file in &files {
        let tenants: std::collections::BTreeSet<&str> =
            file.iter().map(|r| r.tenant_id.as_str()).collect();
        assert_eq!(
            tenants.len(),
            1,
            "each object holds exactly one tenant: {tenants:?}"
        );
    }
    let x = files
        .iter()
        .flatten()
        .filter(|r| r.tenant_id.as_str() == "tenant-x")
        .count();
    let y = files
        .iter()
        .flatten()
        .filter(|r| r.tenant_id.as_str() == "tenant-y")
        .count();
    assert_eq!((x, y), (10, 10), "both tenants' rows present, unmixed");
}
