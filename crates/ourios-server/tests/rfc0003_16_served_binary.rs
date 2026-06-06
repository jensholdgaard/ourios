//! RFC0003.16 — Served binary: both transports bind, a client export
//! round-trips, graceful shutdown.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the served
//! `ourios-server` receiver role lands (the §9 process-model
//! resolution). Per `docs/verification.md` §3 this is the ignored-stub
//! first loop; the implementation flips it to a real-socket integration
//! test (spawn the binary on `127.0.0.1:0`, export over real gRPC + HTTP
//! clients, signal shutdown, then `Wal::replay` after the handle frees).

/// Scenario RFC0003.16 — Served binary: both transports bind, a client
/// export round-trips, graceful shutdown.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[ignore = "RFC 0003 red gate — served ourios-server receiver role pending (RFC0003.16)"]
#[test]
fn rfc0003_16_served_binary_binds_round_trips_and_shuts_down() {
    unimplemented!(
        "RFC0003.16 — boot the ourios-server receiver role bound on \
         127.0.0.1:0 (gRPC + HTTP, ports read back from the process), \
         export a non-empty batch over each transport with a real tonic \
         client and a real HTTP client, assert transport success only \
         after the batch is durable, then signal shutdown and wait for a \
         clean exit (releasing the single Wal handle); a subsequent \
         Wal::replay recovers each OtlpBatch frame with no acked batch \
         lost. Also: a non-POST to /v1/logs returns 405."
    );
}
