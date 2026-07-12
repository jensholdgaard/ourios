//! The consolidated parquet integration-test harness (RFC 0028 slice 2).
//!
//! One binary instead of 17: every test binary links the full arrow +
//! parquet stack, and the link count — not compilation — dominated
//! `cargo test` wall time (RFC 0028 §1 / epic #382). Files here moved
//! verbatim from `tests/*.rs`; test names gain only this harness's
//! module-path prefix (RFC0028.1). No parquet binary needs process
//! isolation, so nothing is exempt (RFC0028.2) — the `#[ignore]`d
//! RFC0005.6 sizing test moves with its file and stays `#[ignore]`d.

mod audit_round_trip;
mod audit_row_vs_path_validation;
mod buffer_and_put;
mod effective_timestamp;
mod no_body_dict;
mod partition_layout;
mod rfc0013_object_store;
mod rfc0018_otlp_compliance;
mod rfc0021_arrow_upgrade;
mod rfc0022_promoted_columns;
mod rfc0024_properties;
mod rfc0025_absent_body;
mod round_trip;
mod row_vs_path_validation;
mod schema_pin;
mod sizing;
mod trace_bloom;
mod zstd_level;
