#![no_main]

//! RFC0015.14 — an audit-event Parquet file read back from object storage
//! (RFC 0005 §3.7; the template registry, the alias map and the drift query
//! all fold these). The input is the whole file, as `Store::get` hands it to
//! `AuditReader::open_bytes`.
//!
//! - **No panic, a typed error.** `open_bytes` and `read_all` return `Ok` or
//!   an `AuditReaderError`, never a panic or an abort, whatever the footer,
//!   pages or column types claim. Bytes involve no file system and no
//!   partition, so `Io` and `PartitionMismatch` are not allowed outcomes.
//!   An upstream `parquet`/`arrow` decoder panic caught by ourios-parquet's
//!   boundary is the expected `DecodePanicked`; one that escapes it crashes.
//! - **Decode → encode → decode.** The events a file decodes to go back
//!   through the writer's `audit_events_to_batch`, to Parquet, and read back
//!   equal: a reader that accepts rows the writer cannot reproduce would
//!   fold something no producer wrote.
//!
//! A target of its own, not a branch of `artifact_decode`: the Parquet
//! footer, page and codec decoders are a far larger and slower coverage
//! space than the JSON artifacts, and in one binary they would take most of
//! the corpus and the executions.

use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use ourios_core::audit::AuditEvent;
use ourios_parquet::{AuditReader, AuditReaderError, audit_events_to_batch};
use parquet::arrow::ArrowWriter;

#[path = "../common/telemetry.rs"]
mod telemetry;

fuzz_target!(init: telemetry::init(), |data: &[u8]| {
    let Some(events) = read(Bytes::copy_from_slice(data)) else {
        return;
    };

    let batch = audit_events_to_batch(&events).unwrap_or_else(|e| {
        panic!("RFC0015.14: the writer cannot encode {} events the reader decoded: {e}", events.len())
    });
    let mut file = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut file, batch.schema(), None)
        .unwrap_or_else(|e| panic!("RFC0015.14: ArrowWriter rejected the audit schema: {e}"));
    writer
        .write(&batch)
        .unwrap_or_else(|e| panic!("RFC0015.14: writing the re-encoded batch failed: {e}"));
    writer
        .close()
        .unwrap_or_else(|e| panic!("RFC0015.14: closing the re-encoded file failed: {e}"));

    match read(Bytes::from(file)) {
        Some(again) => assert!(
            again == events,
            "RFC0015.14: {} decoded events did not survive the writer's encoding",
            events.len()
        ),
        None => panic!("RFC0015.14: the re-encoded audit file does not decode"),
    }
});

/// Every event in `file`, or `None` on an allowed decode error.
fn read(file: Bytes) -> Option<Vec<AuditEvent>> {
    match decode_behind_boundary(file) {
        Ok(events) => Some(events),
        Err(e @ (AuditReaderError::Io { .. } | AuditReaderError::PartitionMismatch { .. })) => {
            panic!("RFC0015.14: reading audit bytes reported {e}")
        }
        Err(_) => None,
    }
}

/// `open_bytes` + `read_all`, with libFuzzer's panic hook swapped out while
/// they run.
fn decode_behind_boundary(file: Bytes) -> Result<Vec<AuditEvent>, AuditReaderError> {
    // libfuzzer-sys aborts from its panic hook, which runs before any
    // unwinding, so ourios-parquet's `catch_unwind` boundary would never
    // see a decoder panic. A recording hook stands in for the one decode
    // (one input at a time, on this thread); a panic the boundary did not
    // turn into `DecodePanicked` lands in the catch below and is re-raised
    // under libFuzzer's restored hook.
    let last_panic = Arc::new(Mutex::new(None));
    let recorder = Arc::clone(&last_panic);
    let fuzzer_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        *recorder.lock().unwrap_or_else(PoisonError::into_inner) = Some(info.to_string());
    }));
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        AuditReader::open_bytes(file).and_then(AuditReader::read_all)
    }));
    panic::set_hook(fuzzer_hook);
    outcome.unwrap_or_else(|_| {
        let escaped = last_panic
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        panic!(
            "RFC0015.14: a panic escaped the ourios-parquet decode boundary: {}",
            escaped.as_deref().unwrap_or("<no panic message recorded>")
        )
    })
}
