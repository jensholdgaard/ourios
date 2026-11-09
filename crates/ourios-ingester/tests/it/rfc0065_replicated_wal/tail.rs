//! The queryable tail: freshness, dedup against Parquet, tenancy,
//! unavailability, isolation, backlog and erasure (RFC0065.11–.14, .18,
//! .19, .21, .22, .28).

/// Scenario RFC0065.11 — a record is queryable within the allowance (B14).
/// Given the flush age at 300 s, when an export is acked and a query
/// starts afterwards, then it returns the records with no flush run.
#[test]
#[ignore = "RFC0065.11 stub — implemented with the tail service"]
fn rfc0065_11_an_acked_record_is_queryable_before_any_flush() {
    todo!(
        "RFC0065.11 — with the flush age at 300 s, a query started after the ack \
         returns the export's records with no flush having run"
    );
}

/// Scenario RFC0065.12 — the tail and Parquet count a record once (B16, B19).
/// Given a test clock over flushes, checkpoints, failovers and compaction,
/// when the same `count` runs at random points, then it equals the records
/// acked before it started (a `proptest` over the event order).
#[test]
#[ignore = "RFC0065.12 stub — implemented with the tail merge"]
fn rfc0065_12_tail_and_parquet_count_a_record_once() {
    todo!(
        "RFC0065.12 — proptest over event order: every `count` returns the number \
         of records acked before it started"
    );
}

/// Scenario RFC0065.13 — the tail keeps tenancy and visibility (B12, B18).
/// Given two tenants in one group and an RFC 0047 masked attribute, then
/// each sees only its own tail rows, masked exactly as in Parquet results.
#[test]
#[ignore = "RFC0065.13 stub — implemented with the tail service"]
fn rfc0065_13_tail_rows_keep_tenancy_and_masking() {
    todo!(
        "RFC0065.13 — each tenant sees only its own tail rows, and the masked \
         attribute is masked exactly as in Parquet results"
    );
}

/// Scenario RFC0065.13 — the tail keeps tenancy and visibility, the caller arm.
/// A `Tail` or `TailScan` call from a certificate not in `tail_clients`, a
/// voter's or tail replica's own included, gets `PERMISSION_DENIED`.
#[test]
#[ignore = "RFC0065.13 stub — implemented with the peer protocol's mTLS gate"]
fn rfc0065_13_tail_calls_outside_tail_clients_are_denied() {
    todo!(
        "RFC0065.13 — `Tail` and `TailScan` from a certificate not in \
         `tail_clients` get PERMISSION_DENIED and no rows"
    );
}

/// Scenario RFC0065.14 — an unreachable tail is reported (B17).
/// Given a group whose tail cannot be read, when a tenant in it is queried,
/// then the response carries the Parquet results and names the tail as
/// unavailable. The `require_complete` arms are served-binary tests.
#[test]
#[ignore = "RFC0065.14 stub — implemented with the tail merge"]
fn rfc0065_14_an_unreachable_tail_is_named_in_the_response() {
    todo!(
        "RFC0065.14 — the response carries the Parquet results and names the tail \
         as unavailable; it never returns fewer records without saying so"
    );
}

/// Scenario RFC0065.18 — tail queries do not slow ingest (B23).
/// Given one tail replica per group at a fixed ingest rate, when tail
/// queries saturate it and then it is paused, then ack p99 stays within
/// 10 % and throughput within 5 % of the same run's query-free baseline.
/// A load run (§8).
#[test]
#[ignore = "RFC0065.18 stub — implemented with the tail load runs"]
fn rfc0065_18_tail_queries_do_not_slow_ingest() {
    todo!(
        "RFC0065.18 — ack latency p99 and ingest throughput stay within the noise \
         band of the run without queries, and no ack waits on the paused tail replica"
    );
}

/// Scenario RFC0065.19 — tail capacity scales on its own (B24).
/// Given one tail replica under saturating load, when a tail peer is added
/// and `tail_replicas` raised to 2, then tail throughput rises while the
/// voters, the ack quorum and ack latency do not change. A load run (§8).
#[test]
#[ignore = "RFC0065.19 stub — implemented with the tail load runs"]
fn rfc0065_19_tail_capacity_scales_on_its_own() {
    todo!(
        "RFC0065.19 — tail query throughput rises with a second tail replica; the \
         voting set, ack quorum and ack latency are unchanged"
    );
}

/// Scenario RFC0065.21 — a long store outage is absorbed and searchable (B25, B19).
/// Given a backlog sized `N`, when the store is unreachable until the
/// backlog reaches half of `N`, then every export is acked, a query over
/// the outage returns every record, selective-query bytes do not grow
/// with the backlog, and the 50 % alert has fired.
#[test]
#[ignore = "RFC0065.21 stub — implemented with the backlog limit"]
fn rfc0065_21_a_long_outage_is_absorbed_and_searchable() {
    todo!(
        "RFC0065.21 — at half of N every export is acked, the whole outage is \
         queryable, a selective query's bytes read do not grow with the backlog, \
         and the 50 % alert has fired"
    );
}

/// Scenario RFC0065.21 — the limit arm (B25, B3).
/// When the backlog reaches the limit, exports get the retryable rejection
/// and nothing already acknowledged is lost.
#[test]
#[ignore = "RFC0065.21 stub — implemented with the backlog limit"]
fn rfc0065_21_at_the_backlog_limit_exports_are_rejected_retryably() {
    todo!(
        "RFC0065.21 — at the limit exports get the retryable rejection and no \
         acknowledged record is lost"
    );
}

/// Scenario RFC0065.22 — a backlog drains while ingest continues (B26).
/// Given RFC0065.21's backlog, when the store returns and ingest continues,
/// then it drains in no longer than it took to build, and a `count` over
/// the window is the same before, during and after.
#[test]
#[ignore = "RFC0065.22 stub — implemented with the backlog limit"]
fn rfc0065_22_a_backlog_drains_while_ingest_continues() {
    todo!(
        "RFC0065.22 — the backlog drains in no longer than it took to build, and \
         a `count` over the window is unchanged before, during and after"
    );
}

/// Scenario RFC0065.28 — the tail never resurrects an erased row, store reachable.
/// Given published rows above the checkpoint and a flusher paused before
/// its published entry commits, when an erasure completes and the flusher
/// resumes, then no erased row is returned: the rewritten file answers.
#[test]
#[ignore = "RFC0065.28 stub — implemented with erasure entries"]
fn rfc0065_28_no_erased_row_returns_with_the_store_reachable() {
    todo!(
        "RFC0065.28 — with the store reachable, the rewritten file answers for the \
         erased conversation and no tail replica serves its rows"
    );
}

/// Scenario RFC0065.28 — the tail never resurrects an erased row, store outage.
/// During an outage the rewritten file is named in `parquet_unavailable`
/// and no tail replica serves the erased rows.
#[test]
#[ignore = "RFC0065.28 stub — implemented with erasure entries"]
fn rfc0065_28_no_erased_row_returns_during_a_store_outage() {
    todo!(
        "RFC0065.28 — during the outage the rewritten file is named in \
         `parquet_unavailable` and no tail replica serves the erased rows"
    );
}
