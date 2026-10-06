//! RFC0059.7 — The bootstrap reads footers in bounded memory.
//! See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
//!
//! The scan runs once per store over every data and audit file, hundreds
//! of thousands on a long-lived node (#895 was an OOM from reading an
//! audit history whole). It must hold one directory listing and one
//! footer at a time.
//!
//! The assertion is on heap bytes, measured by `dhat`'s testing-mode
//! global allocator. Installing a global allocator is process-global, so
//! this test is its own binary (`tests/README.md`).

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_core::record::{BodyKind, MinedRecord};
use ourios_core::tenant::TenantId;
use ourios_ingester::template_ids::BootstrapScan;
use ourios_parquet::{ParquetAuditSink, PartitionKey, Store, Writer};
use tempfile::TempDir;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const TENANT: &str = "eq-perses";
/// About 20 KiB of distinct text per body and per template.
const TOKENS: usize = 2_500;
/// The widest any partition directory gets, in both histories: the
/// history grows by adding directories, never by widening one.
const WIDTH: u64 = 10;
const HOUR_NS: u64 = 3_600_000_000_000;

/// A UTC hour on the civil calendar.
#[derive(Clone, Copy)]
struct Civil {
    year: u64,
    month: u64,
    day: u64,
    hour: u64,
}

/// Nanoseconds since the epoch at `when` (Howard Hinnant's
/// `days_from_civil`).
fn at(when: Civil) -> u64 {
    let Civil {
        year,
        month,
        day,
        hour,
    } = when;
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 24 + hour) * HOUR_NS
}

/// Data file `seq`'s instant: `WIDTH` hours a day, `WIDTH` days a month.
fn data_at(seq: u64) -> u64 {
    at(Civil {
        year: 2026,
        month: 1 + seq / (WIDTH * WIDTH),
        day: 1 + (seq / WIDTH) % WIDTH,
        hour: seq % WIDTH,
    })
}

/// Audit file `seq`'s instant: `WIDTH` days a month, `WIDTH` months a
/// year.
fn audit_at(seq: u64) -> u64 {
    at(Civil {
        year: 2026 + seq / (WIDTH * WIDTH),
        month: 1 + (seq / WIDTH) % WIDTH,
        day: 1 + seq % WIDTH,
        hour: 0,
    })
}

fn text(seq: u64) -> String {
    (0..TOKENS)
        .map(|i| format!("t{seq}x{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn row(seq: u64) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new(TENANT),
        template_id: seq + 1,
        template_version: 1,
        severity_number: 9,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: data_at(seq),
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind: BodyKind::String,
        params: Vec::new(),
        separators: vec![String::new()],
        body: Some(text(seq)),
        confidence: 1.0,
        lossy_flag: true,
    }
}

fn created(seq: u64) -> AuditEvent {
    AuditEvent {
        tenant_id: TenantId::new(TENANT),
        timestamp: UNIX_EPOCH + Duration::from_nanos(audit_at(seq)),
        payload: AuditPayload::Template {
            template_id: seq + 1,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: text(seq),
            },
        },
    }
}

/// `files` data files and `files` audit files, one row or event each;
/// returns the body and template bytes they hold.
fn write_history(bucket: &Path, files: u64) -> u64 {
    let store = Store::local(bucket).expect("store");
    let mut audit = ParquetAuditSink::new(store.clone());
    let mut bytes = 0;
    for seq in 0..files {
        let record = row(seq);
        let partition = PartitionKey::derive(&record).expect("partition");
        let mut writer = Writer::open_in(&store, partition).expect("writer");
        writer.append_records(&[record]).expect("append");
        writer.close().expect("close");
        audit.emit(created(seq));
        bytes += 2 * text(seq).len() as u64;
    }
    assert_eq!(audit.write_failures(), 0, "audit fixtures must all persist");
    bytes
}

/// One scan over a fresh history: what it read, the history's bytes, and
/// its peak heap.
struct Scanned {
    scan: BootstrapScan,
    bytes: u64,
    peak: u64,
}

/// The scan over a fresh history of `files` data and audit files.
fn scan_over(files: u64) -> Scanned {
    let bucket = TempDir::new().expect("temp dir");
    let bytes = write_history(bucket.path(), files);
    let store = Store::local(bucket.path()).expect("store");
    let _profiler = dhat::Profiler::builder().testing().build();
    let scan = BootstrapScan::run(&store).expect("scan");
    let peak = dhat::HeapStats::get().max_bytes as u64;
    Scanned { scan, bytes, peak }
}

/// Scenario RFC0059.7 — the scan's peak heap does not grow when the
/// history grows by directories, with the widest directory and the
/// largest file held fixed: it is bounded by the largest directory
/// listing plus one file.
/// See `docs/rfcs/0059-durable-template-id-allocation.md` §5.
#[test]
fn rfc0059_7_the_bootstrap_scan_heap_does_not_grow_with_history() {
    let Scanned {
        scan,
        bytes: small_bytes,
        peak: small_peak,
    } = scan_over(40);
    assert_eq!((scan.data_max, scan.audit_max), (Some(40), Some(40)));
    assert_eq!(scan.files_scanned, 80);
    let Scanned {
        scan,
        bytes: large_bytes,
        peak: large_peak,
    } = scan_over(160);
    assert_eq!((scan.data_max, scan.audit_max), (Some(160), Some(160)));
    eprintln!(
        "RFC0059.7 bootstrap scan: 80 files ({small_bytes} B) peak {small_peak} B; \
         320 files ({large_bytes} B) peak {large_peak} B",
    );

    assert!(
        large_peak < large_bytes / 8,
        "the scan held {large_peak} B at peak over {large_bytes} B of history",
    );
    assert!(
        large_peak < small_peak + small_peak / 2,
        "peak heap grew with the history: {small_peak} B over 80 files, {large_peak} B \
         over 320",
    );
}
