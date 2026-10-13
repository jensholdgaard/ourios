//! RFC 0060 — background template-map refresh, the querier's task.
//! See `docs/rfcs/0060-background-template-map-refresh.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0060.1 — A start pass absorbs the bootstrap.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.1 stub — implemented in stage 1 of RFC 0060 (the querier refresh task)"]
fn rfc0060_1_a_start_pass_absorbs_the_bootstrap() {
    todo!(
        "RFC0060.1 — a published `no_fold_state` fold runs with `acquire.trigger = refresh` in the first pass, and a later query's lookup is a `query` hit with no fold; the pass is one root INTERNAL `refresh template_map` span over each tenant's `load template_map` (RFC0060.8)"
    );
}

/// Scenario RFC0060.2 — A query during a background fold joins it.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.2 stub — implemented in stage 1 of RFC 0060 (the querier refresh task)"]
fn rfc0060_2_a_query_during_a_background_fold_joins_it() {
    todo!(
        "RFC0060.2 — with the task holding a gated fold, a same-frontier query records `joined` with `acquire.trigger = refresh`, receives the task's map, and exactly one fold runs"
    );
}

/// Scenario RFC0060.3 — One background acquisition at a time.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.3 stub — implemented in stage 1 of RFC 0060 (the querier refresh task)"]
fn rfc0060_3_one_background_acquisition_at_a_time() {
    todo!("RFC0060.3 — three gated tenants are acquired one at a time, in listing order");
}

/// Scenario RFC0060.4 — Failure is contained.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.4 stub — implemented in stage 1 of RFC 0060 (the querier refresh task)"]
fn rfc0060_4_failure_is_contained() {
    todo!(
        "RFC0060.4 — a failed tenant marks only its `load template_map` span; the next tenant is still acquired; a failed tenant listing ends the pass with `error.type` on the pass span and its `refresh.duration` sample"
    );
}

/// Scenario RFC0060.7 — Shutdown is prompt.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.7 stub — implemented in stage 1 of RFC 0060 (the querier refresh task)"]
fn rfc0060_7_shutdown_between_tenants_or_idle_is_prompt() {
    todo!(
        "RFC0060.7 — between tenants or while idle, shutdown exits the task without another acquisition"
    );
}

/// Scenario RFC0060.7 — Shutdown is prompt (in-flight arm).
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.7 stub — implemented in stage 1 of RFC 0060 (the querier refresh task)"]
fn rfc0060_7_shutdown_leaves_a_blocked_flight_to_finish() {
    todo!(
        "RFC0060.7 — with an acquisition gated on the blocking pool, shutdown exits without waiting, and the released flight still completes and publishes"
    );
}

/// Scenario RFC0060.10 — A tenant that persists nothing is backed off.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.10 stub — implemented in stage 1 of RFC 0060 (the querier refresh task)"]
fn rfc0060_10_a_tenant_that_persists_nothing_is_backed_off() {
    todo!(
        "RFC0060.10 — an always-abstaining tenant is acquired on passes 1, 3, 7, then at most one pass in 12; a hit or publish resets it; a successful listing prunes vanished tenants, a failed one keeps them"
    );
}

/// Scenario RFC0060.11 — Passes are spaced from their end, never overlap.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.11 stub — implemented in stage 1 of RFC 0060 (the querier refresh task)"]
fn rfc0060_11_passes_are_spaced_from_their_end() {
    todo!(
        "RFC0060.11 — under paused time a pass gated past `interval` is followed by the next pass `interval` after it finished, with no overlap and no catch-up pass"
    );
}
