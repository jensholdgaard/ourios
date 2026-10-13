//! RFC 0060 — background template-map refresh, its configuration and
//! the served role's task.
//! See `docs/rfcs/0060-background-template-map-refresh.md` §5.
//!
//! Stubs are `#[ignore]`d so the default run stays green while the
//! RFC is red; each names the slice that discharges it.

/// Scenario RFC0060.5 — Disabled means today's behaviour.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.5 stub — implemented in stage 2 of RFC 0060 (configuration and the served role)"]
fn rfc0060_5_disabled_means_todays_behaviour() {
    todo!(
        "RFC0060.5 — with `querier.template_map.refresh.enabled: false` the served \
         querier idles past `interval` with no acquisition, no `refresh template_map` \
         span and no refresh sample"
    );
}

/// Scenario RFC0060.6 — Configuration is validated.
/// See `docs/rfcs/0060-background-template-map-refresh.md` §5.
#[test]
#[ignore = "RFC0060.6 stub — implemented in stage 2 of RFC 0060 (configuration and the served role)"]
fn rfc0060_6_configuration_is_validated() {
    todo!(
        "RFC0060.6 — enabled with `interval: 1s` or a non-duration fails startup \
         naming the key; disabled, the same values resolve"
    );
}
