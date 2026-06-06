//! RFC0003.12 — Empty `ExportLogsServiceRequest` returns success without WAL write.
//! See `docs/rfcs/0003-otlp-receiver.md` §5.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.12 — Empty `ExportLogsServiceRequest` returns success without WAL write.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.12)"]
#[test]
fn rfc0003_12_empty_request_succeeds_without_persisting() {
    unimplemented!(
        "RFC0003.12 — a zero-LogRecord request (empty resource_logs; empty \
         scope_logs; empty log_records — all three shapes) returns success with \
         partial_success unset (OTLP 'empty is success'), and invokes neither \
         Wal::append/sync nor MinerCluster::ingest (asserted via a counting Wal \
         wrapper)."
    );
}
