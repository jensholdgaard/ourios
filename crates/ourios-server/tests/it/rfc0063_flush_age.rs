//! RFC 0063 — configurable receiver flush age.
//! See `docs/rfcs/0063-configurable-flush-age.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0063.1 — the age is configurable and validated.
/// See `docs/rfcs/0063-configurable-flush-age.md` §5.
#[test]
#[ignore = "RFC0063.1 stub — implemented with the config field and Helm wiring"]
fn rfc0063_1_the_age_is_configurable_and_validated() {
    todo!(
        "RFC0063.1 — unset and empty resolve to 300, `10` to 10; `0`, `301` and \
         `ten` fail startup naming `receiver.max_buffer_age_secs` and `1..=300`; \
         the chart renders 10 with OURIOS_RECEIVER_MAX_BUFFER_AGE_SECS=10 in \
         receiver.extraEnv and the default without it"
    );
}

/// Scenario RFC0063.2 — the sink drains on the configured age.
/// See `docs/rfcs/0063-configurable-flush-age.md` §5.
#[test]
#[ignore = "RFC0063.2 stub — implemented with the sink wiring"]
fn rfc0063_2_the_sink_drains_on_the_configured_age() {
    todo!(
        "RFC0063.2 — a sink from `max_buffer_age_secs = 1` drains nothing at once \
         and the partition after 1.2 s of real time"
    );
}

/// Scenario RFC0063.3 — the sweep follows the age, the barrier does not.
/// See `docs/rfcs/0063-configurable-flush-age.md` §5.
#[test]
#[ignore = "RFC0063.3 stub — implemented with the sink wiring"]
fn rfc0063_3_the_sweep_follows_the_age_the_barrier_does_not() {
    todo!(
        "RFC0063.3 — ages 300 and 10 give sweeps of 30 s and 10 s and a 300 s \
         barrier in both"
    );
}

/// Scenario RFC0063.4 — the compactor's erasure settle is unchanged.
/// See `docs/rfcs/0063-configurable-flush-age.md` §5.
#[test]
#[ignore = "RFC0063.4 stub — implemented with the sink wiring"]
fn rfc0063_4_the_erasure_settle_is_unchanged() {
    todo!("RFC0063.4 — erasure_settle_nanos is 2 × 300 s + grace for any valid age");
}

/// Scenario RFC0063.5 — a lowered age makes records queryable sooner.
/// See `docs/rfcs/0063-configurable-flush-age.md` §5.
#[test]
#[ignore = "RFC0063.5 stub — implemented with the served-role test"]
fn rfc0063_5_a_lowered_age_makes_records_queryable_sooner() {
    todo!(
        "RFC0063.5 — a served binary with OURIOS_RECEIVER_MAX_BUFFER_AGE_SECS=2 \
         on the local store returns a few acked records within 15 s"
    );
}
