//! RFC 0031 — comparative evaluation vs Grafana Loki (§5 red stubs).
//!
//! Eleven `#[ignore]`d stubs, one per §5 acceptance scenario
//! (RFC0031.1–.11). Tagged `#[ignore]` so the default `cargo test`
//! stays green while the stubs exist and fail-when-run; each is
//! discharged by its named green slice.
//!
//! Placement note: the comparative harness lives in `ourios-bench`
//! for now (extending the RFC 0006 harness) rather than a new crate,
//! keeping the §7 "new crate vs `bench/` harness" question open — a
//! new crate is a `CLAUDE.md` §7 architectural commitment and is not
//! made here.
//!
//! The primary gate metric throughout is **bytes read from object
//! storage** (RFC 0031 §2.5 / §3.6): the implementation-independent
//! expression of the pruning thesis. Latency is corroborating, not
//! sole-gating. See `docs/rfcs/0031-comparative-evaluation-loki.md`.

/// Scenario RFC0031.1 — result-set equivalence gates every comparison.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.1 stub — implemented in the equivalence-harness green slice"]
fn rfc0031_1_result_set_equivalence() {
    todo!(
        "RFC0031.1 — for a line-returning class the harness keys each \
         system's matches by (timestamp_unix_nanos, body_bytes) and \
         asserts the two MULTISETS are identical (per-key counts equal, \
         so duplicate identical lines are not silently collapsed); for \
         the L4 aggregation class it asserts the (bucket, group_key) -> \
         count maps are identical; a mismatch writes no L-metric for \
         that class, emits the count-delta summary + example keys to \
         stderr, exits non-zero, and no benchmarks.md §9 row is written"
    );
}

/// Scenario RFC0031.2 — L1 selective template lookup wins on bytes read.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.2 stub — implemented in the L-gate + bytes-read green slice"]
fn rfc0031_2_l1_template_lookup_bytes() {
    todo!(
        "RFC0031.2 — on the headline OTel-Demo corpus, a template \
         matching <0.1% of lines: ourios.bytes_read / loki.bytes_read \
         <= 1/M_L1 (Ourios row-group bytes read per the RFC 0016 \
         extension; Loki Summary.totalBytesProcessed). must-win: above \
         the ratio flips l1.pass = false and surfaces a pillar-level \
         finding (benchmarks.md §7). Cold + warm latency recorded as \
         corroborating, non-gating"
    );
}

/// Scenario RFC0031.3 — L2 attribute predicate wins on bytes read.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.3 stub — implemented in the L-gate + bytes-read green slice"]
fn rfc0031_3_l2_attribute_predicate_bytes() {
    todo!(
        "RFC0031.3 — headline corpus, predicate severity >= ERROR AND \
         service.name = X over a bounded window, expressed equivalently \
         in both DSLs (RFC0031.1 holding): ourios.bytes_read / \
         loki.bytes_read <= 1/M_L2, same pillar-level escalation on \
         failure (resource-context pruning via promoted columns, \
         RFC 0022)"
    );
}

/// Scenario RFC0031.4 — L3 trace correlation wins on bytes read (OTLP-native).
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.4 stub — implemented in the L-gate + bytes-read green slice"]
fn rfc0031_4_l3_trace_correlation_bytes() {
    todo!(
        "RFC0031.4 — headline corpus, 'every log line for this \
         trace_id', with trace_id NOT a Loki label (high-cardinality, \
         un-labelable per §3.3): ourios.bytes_read / loki.bytes_read <= \
         1/M_L3 (Ourios bloom-filtered promoted column; Loki \
         label-stream scan). must-win — a query Loki's model cannot \
         serve without a full scan, so a loss is among the strongest \
         signals against the thesis"
    );
}

/// Scenario RFC0031.5 — L4 frequency aggregation wins on bytes read (OTLP-native).
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.5 stub — implemented in the L-gate + bytes-read green slice"]
fn rfc0031_5_l4_frequency_aggregation_bytes() {
    todo!(
        "RFC0031.5 — headline corpus, count of one template over time \
         grouped by an extracted param (Ourios: columnar GROUP BY on \
         template_id + a typed param column; Loki: count_over_time with \
         a LogQL pattern/label_format extraction over scanned chunks), \
         RFC0031.1 grouped-count-map equivalence holding: \
         ourios.bytes_read / loki.bytes_read <= 1/M_L4. must-win — the \
         query the template + typed-params pillar exists to serve"
    );
}

/// Scenario RFC0031.6 — L5 substring needle measured + published, loss permitted.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.6 stub — implemented in the L-gate + reporting green slice"]
fn rfc0031_6_l5_substring_needle_published() {
    todo!(
        "RFC0031.6 — a literal not captured by a template or a promoted \
         column (embedded in a param, nothing prunes it), RFC0031.1 \
         holding: both systems' bytes_read + latency recorded, \
         disposition 'acknowledged'. Run PASSES regardless of winner — \
         an Ourios loss does not fail the run and does not escalate, but \
         MUST appear in the benchmarks.md §9 table (a suppressed L5 loss \
         is a process violation)"
    );
}

/// Scenario RFC0031.7 — L6 broad scan stays within the floor.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.7 stub — implemented in the L-gate green slice"]
fn rfc0031_7_l6_broad_scan_floor() {
    todo!(
        "RFC0031.7 — low-selectivity wide-time-range query, RFC0031.1 \
         holding: ourios.latency_p50 <= F_L6 * loki.latency_p50. \
         Exceeding the floor is a tuning-RFC signal, not a pillar-level \
         escalation"
    );
}

/// Scenario RFC0031.8 — L7 ingest throughput parity within a stated factor.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.8 stub — implemented in the ingest-parity green slice"]
fn rfc0031_8_l7_ingest_throughput_parity() {
    todo!(
        "RFC0031.8 — OTLP replay driver feeding both systems to steady \
         state on the same hardware: ourios.ingest_throughput >= \
         loki.ingest_throughput / F_L7. The WAL-before-ack invariant \
         (CLAUDE.md §3.4) is NOT relaxed to obtain the number — Ourios \
         throughput is measured with durable acks and the config \
         proving it is recorded"
    );
}

/// Scenario RFC0031.9 — storage footprint is a diagnostic, not a gate.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.9 stub — implemented in the reporting green slice"]
fn rfc0031_9_storage_footprint_diagnostic() {
    todo!(
        "RFC0031.9 — both systems' persisted bytes on the shared bucket \
         and their ratio written to benchmarks.md §9 as a DIAGNOSTIC \
         row; no pass/fail derived from it (parity with A1's RFC 0011 \
         demotion)"
    );
}

/// Scenario RFC0031.10 — Loki config committed, competent, machine-checked.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.10 stub — implemented in the config-check green slice"]
fn rfc0031_10_loki_config_machine_checked() {
    todo!(
        "RFC0031.10 — the exact Loki config (index, chunk target size, \
         S3 backend, retention, frozen label set), the OTLP-into-Loki \
         config, and the DSL<->LogQL query pairs are present under \
         bench/comparative/ and the comparison runs with one documented \
         command; a test asserts the label set is drawn from a declared \
         low-cardinality allowlist and that trace_id, span_id, and any \
         per-template id are ABSENT (no catch-all-forcing-full-scan and \
         no high-cardinality label smuggling Ourios's columns into \
         Loki's index); each §9 row links the config commit"
    );
}

/// Scenario RFC0031.11 — losses published and escalation follows §7.
/// See `docs/rfcs/0031-comparative-evaluation-loki.md` §5.
#[test]
#[ignore = "RFC0031.11 stub — implemented in the reporting + escalation green slice"]
fn rfc0031_11_losses_published_and_escalation() {
    todo!(
        "RFC0031.11 — every taxonomy class appears in benchmarks.md §9 \
         (wins AND losses) with disposition, both systems' numbers, the \
         corpus, and the hardware tag; an L1/L2/L3/L4 bytes-read loss on \
         the headline OTel-Demo corpus is a pillar-level finding pausing \
         further implementation pending a CLAUDE.md §2 revisit, whereas \
         a must-win latency-only loss with a bytes-read win is a roadmap \
         item"
    );
}
