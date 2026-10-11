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
//! - **Decode → encode → decode.** The events a file decodes to go back
//!   through the writer's `audit_events_to_batch`, to Parquet, and read back
//!   equal: a reader that accepts rows the writer cannot reproduce would
//!   fold something no producer wrote.
//!
//! A target of its own, not a branch of `artifact_decode`: the Parquet
//! footer, page and codec decoders are a far larger and slower coverage
//! space than the JSON artifacts, and in one binary they would take most of
//! the corpus and the executions.

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
    match AuditReader::open_bytes(file).and_then(AuditReader::read_all) {
        Ok(events) => Some(events),
        Err(e @ (AuditReaderError::Io { .. } | AuditReaderError::PartitionMismatch { .. })) => {
            panic!("RFC0015.14: reading audit bytes reported {e}")
        }
        Err(_) => None,
    }
}
