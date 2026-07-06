# Integration-test layout (RFC 0028 slice 1)

`it/` is the consolidated harness — one compiled binary for the crate's
integration tests (RFC 0028 §3.1; the per-binary link cost dominated
`cargo test` wall time). New integration tests go in `it/<name>.rs` plus a
`mod` line in `it/main.rs`, unless they belong on the exempt list below.

## Harness-exempt binaries (RFC0028.2)

Each of these installs the **process-global** OTel meter provider
(`ourios_telemetry::init_in_memory`, or instruments resolving through the
global meter). OpenTelemetry cannot restore a replaced global, so two
installers in one process race each other (see the note in
`perf_metrics.rs`); they stay one-per-binary:

- `perf_metrics.rs` — ingest + sink instruments through the global meter.
- `audit_sink_metrics.rs` — audit-sink instruments through the global meter.
- `rfc0018_otlp_compliance.rs` — its `.6` telemetry arm installs the
  global in-memory provider.
- `rfc0025_quarantine.rs` — its RFC0025.5 telemetry arm installs the
  global in-memory provider.

`fixtures/` holds the crash-fixture **`[[bin]]` targets** (SIGKILL'd by
harness tests via `CARGO_BIN_EXE_*`), not test binaries.
