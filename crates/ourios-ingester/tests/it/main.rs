//! The consolidated ingester integration-test harness (RFC 0028 slice 1).
//!
//! One binary instead of 27: every test binary links the crate's full
//! dependency stack, and the link storm — not compilation — dominated
//! `cargo test` wall time (RFC 0028 §1 / epic #382). Files here moved
//! verbatim from `tests/*.rs`; test names gain only this harness's
//! module-path prefix (RFC0028.1).
//!
//! Four binaries deliberately remain outside (RFC0028.2): they install
//! the **process-global** `OTel` meter provider (`init_in_memory` /
//! global-meter instruments), and two global installers in one process
//! race — see `tests/README.md`.

mod ingest_support;
mod otlp_strategy;

mod http_transport_errors;
mod invariant_3_7_3_tenant_per_resource_logs;
mod rfc0001_3_5_snapshot_restore;
mod rfc0003_10_dropped_attributes_count;
mod rfc0003_11_transport_errors;
mod rfc0003_12_empty_request_success;
mod rfc0003_13_compression;
mod rfc0003_14_path_config;
mod rfc0003_15_concurrent_wal_before_ack;
mod rfc0003_1_wal_before_ack;
mod rfc0003_2_crash_before_ack;
mod rfc0003_3_tenant_fanout;
mod rfc0003_4_tenant_resolution_failure;
mod rfc0003_5_grpc_http_protobuf_equivalence;
mod rfc0003_6_json_protobuf_equivalence;
mod rfc0003_7_body_structured_verbatim;
mod rfc0003_8_body_string_lraw;
mod rfc0003_9_edge_otlp_fields;
mod rfc0008_10_rotation_cadence;
mod rfc0008_8_batched_fsync;
mod rfc0008_8_ingest_order;
mod rfc0014_5_crash_no_loss;
mod rfc0014_ingest_write_path;
mod rfc0018_retryable;
mod rfc0022_promoted_threading;
mod rfc0023_overflow_roundtrip;
mod rfc0026_auth;
mod rfc0030_tls;
mod rfc0035_1_concurrent_determinism;
mod rfc0035_2_encode_barrier;
mod rfc0035_5_on_disk_equivalence;
