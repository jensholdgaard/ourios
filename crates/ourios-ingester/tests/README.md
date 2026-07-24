# Integration-test layout (RFC 0028 slice 1)

`it/` is the consolidated harness — one compiled binary for the crate's
integration tests (RFC 0028 §3.1; the per-binary link cost dominated
`cargo test` wall time). New integration tests go in `it/<name>.rs` plus a
`mod` line in `it/main.rs`, unless they belong on the exempt list below.

## Harness-exempt binaries (RFC0028.2)

Each of these installs a **process-global** OTel provider (a meter via
`ourios_telemetry::init_in_memory` or instruments resolving through the
global meter; or, for the tracer, a global `SdkTracerProvider` + subscriber).
OpenTelemetry cannot restore a replaced global, so two installers in one
process race each other (see the note in `perf_metrics.rs`); they stay
one-per-binary:

- `perf_metrics.rs` — ingest + sink instruments through the global meter.
- `audit_sink_metrics.rs` — audit-sink instruments through the global meter.
- `rfc0018_otlp_compliance.rs` — its `.6` telemetry arm installs the
  global in-memory provider.
- `rfc0025_quarantine.rs` — its RFC0025.5 telemetry arm installs the
  global in-memory provider.
- `rfc0026_telemetry.rs` — the RFC0026.7 rejection-telemetry arm installs
  the global in-memory provider.
- `rfc0038_3_spawn_boundary.rs` — installs the global in-memory **tracer**;
  a global (not scoped) tracer is required to capture the `ingest logs` /
  `sweep partitions` spans across the receiver's `tokio::spawn` and the
  compactor's `spawn_blocking` (RFC0038.3).

`fixtures/` holds the crash-fixture **`[[bin]]` targets** (SIGKILL'd by
harness tests via `CARGO_BIN_EXE_*`), not test binaries.
