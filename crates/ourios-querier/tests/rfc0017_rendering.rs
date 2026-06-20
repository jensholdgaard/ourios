//! RFC 0017 — read-time template registry & query-row rendering, the
//! body-rendering scenarios (`.3`, `.4`, `.9`).
//!
//! **Status: `red`.** Failing stubs driving the `green` implementation: they
//! encode RFC 0017 §5 scenarios .3 (a clean row renders bit-identically), .4
//! (lossy / parse-failure rows return the retained body), and .9 (a structured
//! `AnyValue` body is returned as structure) and currently `todo!()`. They are
//! `#[ignore]`d so the default `cargo test` (and CI) stays green until the
//! row-rendering path lands; `green` replaces the bodies with the real
//! assertions and removes the `#[ignore]`s.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.3 / §3.4 / §5 / §6.

/// Scenario RFC0017.3 — a stored clean-path (`Faithful`-eligible) row, rendered
/// via the derived registry tokens, equals the originally-ingested line
/// byte-for-byte (CLAUDE.md §3.3 bit-identical invariant), and the row's
/// `Reconstruction` marker is `Faithful`.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.3 — red until clean rows render bit-identically via the registry (green)"]
fn rfc0017_3_clean_row_renders_bit_identically() {
    todo!(
        "RFC0017.3: registry-rendered clean line == originally-ingested line byte-for-byte, marker Faithful"
    )
}

/// Scenario RFC0017.4 — a row flagged lossy or with no template (parse failure),
/// whose `body` was retained, renders the retained `body` verbatim with marker
/// `RetainedVerbatim` — no template walk, never a wrong reconstruction.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.4 — red until lossy / parse-failure rows return the retained body (green)"]
fn rfc0017_4_lossy_rows_return_retained_body() {
    todo!(
        "RFC0017.4: lossy / parse-failure row returns retained body verbatim, marker RetainedVerbatim"
    )
}

/// Scenario RFC0017.9 — a stored row with `body_kind = Structured` (the OTLP
/// `Body` was a map/array, canonical JSON in `body`) is returned as
/// `LogBody::Structured(AnyValue)`, preserving the original map/array shape
/// (not flattened to a byte line) and round-tripping the ingested `AnyValue`.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
#[ignore = "RFC0017.9 — red until structured bodies return as LogBody::Structured (green)"]
fn rfc0017_9_structured_body_returned_as_structure() {
    todo!(
        "RFC0017.9: body_kind=Structured returns LogBody::Structured(AnyValue), round-trips, never flattened"
    )
}
