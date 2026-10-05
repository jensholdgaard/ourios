//! #898 — startup recovery's template-id floor reads the whole audit
//! stream, across every tenant, in heap that does not grow with it.
//!
//! The floor is read exactly when a snapshot was discarded, which on an
//! upgraded node means over its full audit history (#895 was an OOM from
//! decoding that history). The read must hold one audit object at a time
//! and answer each from its footer, never from decoded events.
//!
//! The assertion is on heap bytes, measured by `dhat`'s testing-mode
//! global allocator. Installing a global allocator is process-global
//! state, so this test lives in its own binary (`tests/README.md`).

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use ourios_core::audit::{
    AuditEvent, AuditPayload, AuditSink, TemplateChange, hash_triggering_line,
};
use ourios_core::tenant::TenantId;
use ourios_ingester::issued_ids::highest_issued_template_id;
use ourios_parquet::{ParquetAuditSink, Store};
use tempfile::TempDir;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const TENANTS: [&str; 2] = ["eq-perses", "nocturnal"];
/// Tokens per template: about 20 KiB of template text per event.
const TOKENS: usize = 2_500;

/// A long template, distinct per event so neither the codec nor a shared
/// allocation hides its size.
fn template(seq: u64) -> String {
    (0..TOKENS)
        .map(|i| format!("t{seq}x{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn created(seq: u64) -> AuditEvent {
    let tenant = TENANTS[usize::try_from(seq % 2).expect("index")];
    AuditEvent {
        tenant_id: TenantId::new(tenant),
        timestamp: UNIX_EPOCH + Duration::from_secs(seq),
        payload: AuditPayload::Template {
            template_id: seq + 1,
            triggering_line_hash: hash_triggering_line(b"line"),
            triggering_line_sample: None,
            change: TemplateChange::Created {
                new_template: template(seq),
            },
        },
    }
}

/// `files` audit files, one event each through the production sink;
/// returns the template text they hold.
fn write_history(bucket: &Path, files: u64) -> u64 {
    let mut sink = ParquetAuditSink::new(Store::local(bucket).expect("store"));
    let mut text = 0;
    for seq in 0..files {
        text += template(seq).len() as u64;
        sink.emit(created(seq));
    }
    assert_eq!(sink.write_failures(), 0, "audit fixtures must all persist");
    text
}

/// The floor over a fresh history of `files` audit files: its value, the
/// history's template text, and the read's peak heap.
fn floor_over(files: u64) -> (Option<u64>, u64, u64) {
    let bucket = TempDir::new().expect("temp dir");
    let text = write_history(bucket.path(), files);
    let store = Store::local(bucket.path()).expect("store");
    let _profiler = dhat::Profiler::builder().testing().build();
    let floor = highest_issued_template_id(&store).expect("read the floor");
    let peak = dhat::HeapStats::get().max_bytes as u64;
    (floor, text, peak)
}

#[test]
fn the_floor_reads_the_audit_history_in_bounded_heap() {
    let (floor, small_text, small_peak) = floor_over(40);
    assert_eq!(floor, Some(40));
    let (floor, large_text, large_peak) = floor_over(160);
    assert_eq!(floor, Some(160));
    eprintln!(
        "#898 floor read: 40 files ({small_text} B template text) peak {small_peak} B; \
         160 files ({large_text} B) peak {large_peak} B",
    );

    assert!(
        large_peak < large_text / 8,
        "the floor held {large_peak} B at peak over {large_text} B of template text",
    );
    assert!(
        large_peak < small_peak + small_peak / 2,
        "peak heap grew with the history: {small_peak} B over 40 files, {large_peak} B \
         over 160",
    );
}
