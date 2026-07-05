//! Dev-only OTLP-envelope generators for Ourios property suites.
//!
//! RFC 0024 §3.2's two generation modes over
//! [`ourios_core::otlp::OtlpLogRecord`]:
//!
//! - [`strategies::calibrated`] — field distributions weighted by a
//!   [`manifest::CalibrationManifest`] measured from a real corpus
//!   release, so generated records sit in the realistic centre.
//! - [`strategies::adversarial`] — uniform-ish over the envelope's
//!   legal extremes, bounded only by documented product limits.
//!
//! Generation happens past wire decode (the RFC 0003 §6.6 in-memory
//! shape); the RFC 0003 equivalence suites already cover the wire
//! boundary itself.
//!
//! This crate is test infrastructure per RFC 0024 §3.2: it is never
//! published, and nothing in the workspace's production graph may
//! depend on it — consumers take it as a dev-dependency, which is
//! how `proptest` stays out of every production crate's graph.

pub mod manifest;
pub mod strategies;

/// Per-suite proptest case count: `base` (the suite's tuned CI
/// default) unless `PROPTEST_CASES` is set, which wins. proptest's
/// own env handling feeds `ProptestConfig::default()`, but an
/// explicit `cases:` field overrides it — so suites that tune their
/// base count would silently ignore the deep run's elevated setting
/// without this hook (`.github/workflows/rfc0024-deep-run.yml`).
#[must_use]
pub fn proptest_cases(base: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(base)
}
