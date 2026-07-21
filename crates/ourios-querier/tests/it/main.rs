//! The consolidated querier integration-test harness (RFC 0028 slice 2).
//!
//! One binary instead of 19: every test binary links the full `DataFusion`
//! stack, and the link count — not compilation — dominated `cargo test`
//! wall time (RFC 0028 §1 / epic #382). Files here moved verbatim from
//! `tests/*.rs`; test names gain only this harness's module-path prefix
//! (RFC0028.1). One binary is exempt under the RFC0028.2 rule
//! (process isolation): `tests/rfc0033_7_observability.rs` installs a
//! process-global in-memory `MeterProvider` (the RFC 0016 metrics-test
//! shape).

mod common;

mod acceptance;
mod boundary;
mod drift;
mod execution;
mod forward_compat;
mod manifest;
mod rfc0001_query_semantics;
mod rfc0001_time_preserved;
mod rfc0002_dsl;
mod rfc0005_13;
mod rfc0005_14_alias_derivation;
mod rfc0017_query_rows;
mod rfc0017_registry;
mod rfc0017_rendering;
mod rfc0018_otlp_compliance;
mod rfc0018_severity;
mod rfc0022_attr_columns;
mod rfc0024_properties;
mod rfc0025_rendering;
mod rfc0031_single_pass;
mod rfc0033_cached_template_map;
mod rfc0036_window_materialization;
