//! RFC0003.14 — Default `/v1/logs` path with configurable override.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.14 — Default `/v1/logs` path with configurable override.
/// See `docs/rfcs/0003-otlp-receiver.md` §5.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.14)"]
#[test]
fn rfc0003_14_default_path_and_configurable_override() {
    unimplemented!(
        "RFC0003.14 — a POST to the default /v1/logs is handled via the §6.2 HTTP \
         path; a POST to any other path returns HTTP 404; and an operator-\
         configured override path (e.g. /otlp/v1/logs) replaces /v1/logs as the \
         accepted path without changing any other receiver behaviour."
    );
}
