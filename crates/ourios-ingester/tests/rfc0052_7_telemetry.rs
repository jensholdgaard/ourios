//! RFC0052.7 — The WAL's state is exported.
//! See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the green slice that discharges it.
//!
//! Its own test binary, like `perf_metrics.rs` (RFC0028.2 exempt list
//! in `README.md`): the green implementation installs the **global**
//! in-memory meter provider to read the exported stream, and two
//! global-installing tests in one binary would race. The stubs land
//! here so the file does not move when it goes green.

/// Scenario RFC0052.7 — every registry name is in the exported stream.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.7 stub — implemented in the timer-and-telemetry green slice E (in-memory exporter over the WAL instruments)"]
fn rfc0052_7_every_wal_instrument_is_exported_under_its_registry_name() {
    todo!(
        "RFC0052.7 — a running node, metrics collected: unflushed bytes, \
         on-disk bytes, segment count, all unreclaimed bytes and the age \
         of the oldest unreclaimed frame, the retain floor with its lag \
         and Pinned state, the cadence_failed latch, the \
         rotation-failure state, and the housekeeping backlog (horizon \
         application remaining and unlinks remaining, in segments) are \
         all present under registry names; a run whose checkpoint never \
         advances still reports growing unreclaimed bytes"
    );
}

/// Scenario RFC0052.7 — each transition emits its named event exactly once.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.7 stub — implemented in the timer-and-telemetry green slice E (runtime tracing assertion driving each transition)"]
fn rfc0052_7_each_transition_emits_its_registry_event_once() {
    todo!(
        "RFC0052.7 — driving terminal entry on the append path, floor \
         pinned and lifted, and the latch set: each emits exactly one \
         log event named from the registry with the expected state; \
         entering the terminal rotation state emits exactly one event, \
         and no leave event exists until §7's operator verb does"
    );
}

/// Scenario RFC0052.7 — `weaver registry live-check` sees every new event emitted.
/// See `docs/rfcs/0052-wal-reclamation-and-quiesce-recovery.md` §5.
#[test]
#[ignore = "RFC0052.7 stub — implemented in the timer-and-telemetry green slice E (live-check over the new log events; an event never emitted is never checked)"]
fn rfc0052_7_live_check_covers_every_new_log_event() {
    todo!(
        "RFC0052.7 — a weaver registry live-check pass over the new log \
         events, with every one of them emitted by the test so the \
         live-check validates emission and not only definitions — the \
         gap through which #795's un-named event passed CI"
    );
}
