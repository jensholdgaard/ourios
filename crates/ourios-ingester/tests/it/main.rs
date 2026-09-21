//! The consolidated ingester integration-test harness (RFC 0028 slice 1).
//!
//! One binary instead of 27: every test binary links the crate's full
//! dependency stack, and the link storm — not compilation — dominated
//! `cargo test` wall time (RFC 0028 §1 / epic #382). Files here moved
//! verbatim from `tests/*.rs`; test names gain only this harness's
//! module-path prefix (RFC0028.1).
//!
//! Eight binaries deliberately remain outside (RFC0028.2): six install a
//! **process-global** `OTel` *meter* provider (`init_in_memory` /
//! global-meter instruments) and two a global *tracer*, and two global
//! installers of either kind in one process race. `tests/README.md` is the
//! list and the authority — the count here was already stale before
//! `cadence_panic_metric.rs` joined it, so prefer that file.

mod ingest_support;
mod otlp_strategy;

mod http_transport_errors;
mod rfc0001_3_5_snapshot_restore;
mod rfc0003_10_dropped_attributes_count;
mod rfc0003_11_transport_errors;
mod rfc0003_12_empty_request_success;
mod rfc0003_13_compression;
mod rfc0003_14_path_config;
mod rfc0003_15_concurrent_wal_before_ack;
mod rfc0003_1_wal_before_ack;
mod rfc0003_2_crash_before_ack;
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
mod rfc0035_2_sweep_crash;
mod rfc0035_5_on_disk_equivalence;
mod rfc0035_f2_miner_panic_salvage;
mod rfc0038_2_ingest_batch_span;
mod rfc0043_6_event_keyed_templating;
mod rfc0043_event_name_derivation;
mod rfc0046_4_wal_frame_carries_tenant;
mod rfc0052_10_no_loss;
mod rfc0052_14_timer_exclusion;
mod rfc0052_15_terminal_only_classification;
mod rfc0052_1_checkpoint_policy;
