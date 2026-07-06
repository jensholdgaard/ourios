# Integration-test layout (RFC 0028 slice 2)

`it/` is the consolidated harness — one compiled binary for this crate's
integration tests (RFC 0028 §3.1; the per-binary link cost dominated
`cargo test` wall time). New integration tests go in `it/<name>.rs` plus a
`mod` line in `it/main.rs`, unless they belong on the exempt list below.

## Harness-exempt binaries (RFC0028.2)

Each of these installs the **process-global** OTel meter provider
(`ourios_telemetry::init_in_memory`) for one telemetry arm. OpenTelemetry
cannot restore a replaced global, so two installers in one process race
each other; they stay one-per-binary:

- `invariants.rs` — the §6.5 param-overflow metrics arm.
- `hazards.rs` — the per-service overflow-rate alert-threshold arm.
- `rfc0023_bounded_memory.rs` — the template-ceiling/eviction metrics arm.
- `rfc_internal.rs` — the confidence-gauge quantile arm.
