//! C2 — Template-count convergence.
//!
//! Per RFC 0006 §3.4.3 the gate is: "template count grows
//! sub-linearly and plateaus within 2× of its steady-state
//! value by 1 M lines", operationalised as **`count(1M) ≥
//! SS / 2`** where SS is the template count at end of corpus.
//! Since template count is monotonic non-decreasing (the
//! miner never unmerges), `count(1M) ≥ SS/2` means the curve
//! can't have more than doubled between 1 M lines and the
//! end — i.e. it's within 2× of steady state.
//!
//! The template count at any point is the number of distinct
//! non-[`NO_TEMPLATE`] `template_id`s seen so far. The
//! cluster's id allocator is **monotonic** — it hands out
//! `1, 2, 3, …` in creation order (RFC 0001 §6.1:
//! "per-tenant monotonic"; the bench is single-tenant), so a
//! template's id first appears on the line that created it and
//! is strictly greater than every id seen before. That lets
//! C2 count distinct templates in **O(1) memory**: track the
//! max id seen and bump a counter only when an id exceeds it.
//! This matters because a *non-converging* corpus — the very
//! thing C2 is built to flag — can mint millions of templates;
//! a `HashSet` of ids would balloon (and could OOM) on exactly
//! those inputs, whereas the max-plus-counter stays flat. C2
//! is otherwise a pure stream accumulator over the harness
//! callback, like C1.
//!
//! Pinned definitions (§3.4.3):
//!
//! - **Sample cadence** `N = max(1, ceil(total_lines / 1024))`
//!   — bounds the curve to ≤ 1024 samples. The count is
//!   recorded after line indices `N-1, 2N-1, …` (1-based:
//!   every N-th line), with the final line always sampled.
//!   Sample count is `ceil(total_lines / N)`.
//! - **Steady-state (SS)**: the count at the last sample.
//! - **Count at 1 M lines**: the count at the sample whose
//!   1-based line number is closest to `1_000_000`, floor
//!   tie-break. Defined only on corpora ≥ 1 M lines.
//! - **Convergence ratio** = `count_1m / SS`, in `[0, 1]` when
//!   defined — `0` when no template has been minted as of the
//!   sample nearest the 1 M mark (`count_1m == 0`, `SS > 0`);
//!   undefined (`None`) for a ≥ 1 M service that mints zero
//!   templates at all (see the gate).
//! - **Pass** (per service, RFC 0006 §3.4.3 as amended for #444):
//!   the gate is evaluated **per `service.name`**, since C2 is
//!   defined over a single stable service. A corpus passes iff
//!   every service with ≥ 1 M lines has `ratio ≥ 0.5` (a zero-
//!   template ≥ 1 M service passes trivially — flat count); it
//!   abstains (`pass = None`) only when no service reaches 1 M
//!   lines. The whole-corpus [`crate::C2Result::convergence_ratio`]
//!   is retained as a diagnostic — on a multi-service corpus it
//!   conflates a noisy broker with clean application services
//!   (`docs/benchmarks.md` §9.12). A single-service (or plain-text
//!   `<unknown>`) corpus collapses to that one service's verdict.

use std::collections::BTreeMap;

use ourios_core::otlp::any_value::Value as AnyValueKind;
use ourios_core::record::MinedRecord;
use ourios_miner::cluster::NO_TEMPLATE;
use ourios_parquet::promoted::SERVICE_NAME_KEY;

use crate::{C2Result, ConvergenceSample, PerServiceC2};

/// Distinct-`service.name` cap for the per-service decomposition. Real
/// OTLP corpora carry tens of services; the cap is a cardinality guard
/// (mirroring §3.2's ethos) so a pathological corpus with millions of
/// distinct service names can't balloon the `by_service` map. Overflow
/// folds into a single `<other>` bucket.
const MAX_SERVICES: usize = 1024;

/// The `<other>` overflow bucket name (see [`MAX_SERVICES`]).
const OTHER_SERVICES: &str = "<other>";

/// The `service.name` used when a record carries no such resource
/// attribute (never expected on OTLP corpora; possible on the
/// plain-text form, where the whole decomposition is one bucket).
const UNKNOWN_SERVICE: &str = "<unknown>";

/// Per-service tally for the C2 decomposition. Template creation is a
/// global monotonic event attributed to the creating line's service,
/// so this is O(services) memory — no per-service id set (the module's
/// whole memory-safety argument would otherwise break on exactly the
/// non-converging corpora C2 exists to flag).
#[derive(Default)]
struct PerService {
    lines: u64,
    created: u64,
    created_at_1m: Option<u64>,
}

/// Curve-size cap: the cadence is chosen so a corpus of any
/// size yields at most this many samples (§3.4.3).
const SAMPLE_BUDGET: u64 = 1024;

/// The "1 M lines" mark the convergence ratio is measured at.
const ONE_MILLION: u64 = 1_000_000;

/// Streaming accumulator for the §3.4.3 C2 measurement. Fed one
/// emitted record per ingested line by the harness loop;
/// [`Self::finalize`] computes the [`C2Result`].
pub(crate) struct C2Accumulator {
    total_lines: u64,
    cadence: u64,
    /// Distinct templates seen so far. Bumped whenever an id
    /// exceeds `max_template_id` (a newly-created template,
    /// given monotonic allocation).
    template_count: u64,
    /// Largest `template_id` observed. A larger id means a
    /// freshly-created template; `≤` means a reuse already
    /// counted.
    max_template_id: u64,
    curve: Vec<ConvergenceSample>,
    processed: u64,
    /// Per-`service.name` decomposition (diagnostic; see [`PerServiceC2`]).
    by_service: BTreeMap<String, PerService>,
    /// [`MAX_SERVICES`] hit — extra services folded into `<other>`.
    services_truncated: bool,
}

impl C2Accumulator {
    /// Create an accumulator for a corpus of `total_lines`
    /// lines. The cadence is fixed up front from the line
    /// count per §3.4.3.
    pub(crate) fn new(total_lines: u64) -> Self {
        let cadence = total_lines.div_ceil(SAMPLE_BUDGET).max(1);
        Self {
            total_lines,
            cadence,
            template_count: 0,
            max_template_id: 0,
            curve: Vec::new(),
            processed: 0,
            by_service: BTreeMap::new(),
            services_truncated: false,
        }
    }

    /// Observe one emitted record: fold its `template_id` into the
    /// whole-corpus curve and attribute any template creation to the
    /// record's `service.name` for the per-service decomposition.
    pub(crate) fn record(&mut self, emitted: &MinedRecord) {
        let created = self.observe(emitted.template_id);
        self.attribute(service_name(emitted), created);
    }

    /// Core of [`Self::record`], split out so the colocated
    /// tests can drive the sampling + convergence math at
    /// scale (millions of synthetic ids) without constructing
    /// `MinedRecord`s or running the miner. Returns whether this id
    /// created a new template (a monotonic-max advance).
    fn observe(&mut self, template_id: u64) -> bool {
        // A non-`NO_TEMPLATE` id larger than any seen before is
        // a freshly-created template (monotonic allocation);
        // a smaller-or-equal id is a reuse already counted.
        let created = template_id != NO_TEMPLATE && template_id > self.max_template_id;
        if created {
            self.max_template_id = template_id;
            self.template_count += 1;
        }
        self.processed += 1;

        // Sample after every N-th line (1-based `processed`
        // divisible by the cadence) and always on the final
        // line. The guard avoids a duplicate final sample when
        // the last line happens to fall on a cadence boundary.
        let on_cadence = self.processed.is_multiple_of(self.cadence);
        let is_last = self.processed == self.total_lines;
        if (on_cadence || is_last) && self.curve.last().map(|s| s.lines) != Some(self.processed) {
            self.curve.push(ConvergenceSample {
                lines: self.processed,
                template_count: self.template_count,
            });
        }
        created
    }

    /// Attribute a line (and any template creation) to its service.
    /// The 1 M-line snapshot is taken at exactly the millionth line of
    /// *that* service — the **gate** basis (RFC 0006 §3.4.3 as amended
    /// for #444), and strictly more precise than the whole-corpus rule's
    /// nearest-sample, which is now only the diagnostic ratio.
    fn attribute(&mut self, service: &str, created: bool) {
        // Cardinality guard: a known service (or a new one below the
        // cap) keeps its name; once the cap is hit, unseen services fold
        // into one `<other>` bucket rather than grow unboundedly. The
        // one presence lookup here is reused below, so the hot path is
        // two map ops (this + `get_mut`), never three.
        let present = self.by_service.contains_key(service);
        let key = if present || self.by_service.len() < MAX_SERVICES {
            service
        } else {
            self.services_truncated = true;
            OTHER_SERVICES
        };
        // Insert (allocating the owned key) only on a first sighting;
        // `entry(key.to_string())` would copy on every line, a per-record
        // allocation across a multi-million-line corpus. A known service
        // is already answered by `present`; only the `<other>` sentinel
        // needs its own check, since the cap path can arrive with the
        // bucket already created.
        let needs_insert = if key == service {
            !present
        } else {
            !self.by_service.contains_key(key)
        };
        if needs_insert {
            self.by_service
                .insert(key.to_string(), PerService::default());
        }
        let entry = self
            .by_service
            .get_mut(key)
            .expect("bucket present after the insert above");
        entry.lines += 1;
        if created {
            entry.created += 1;
        }
        if entry.lines == ONE_MILLION {
            entry.created_at_1m = Some(entry.created);
        }
    }

    /// Compute the §3.4.3 [`C2Result`] from the accumulated
    /// curve.
    ///
    /// The `u64 → f64` casts for the ratio lose precision only
    /// above `2^52` distinct templates, which no real corpus
    /// approaches.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn finalize(self) -> C2Result {
        let template_count_at_end = self.curve.last().map_or(0, |s| s.template_count);
        let corpus_at_least_1m = self.total_lines >= ONE_MILLION;

        // Whole-corpus convergence — a **diagnostic** now, not the gate
        // (the gate is per-service below, RFC 0006 §3.4.3 as amended
        // for #444): on a multi-service corpus a whole-corpus ratio
        // conflates a noisy broker with clean application services
        // (v8 §9.12). Undefined (both `None`) below 1 M lines *and* when
        // the corpus mints zero templates (SS = 0, ratio 0/0): the count
        // and the ratio stay a matched pair — both `Some` or both `None`,
        // never mixed — which the report layer relies on (report.rs).
        let (template_count_at_1m_lines, convergence_ratio) =
            if corpus_at_least_1m && template_count_at_end > 0 {
                // Sample whose 1-based line number is closest to
                // 1 M; on a tie the earlier (smaller `lines`)
                // sample wins — the `(distance, lines)` key makes
                // that the strict minimum.
                let count_1m = self
                    .curve
                    .iter()
                    .min_by_key(|s| (s.lines.abs_diff(ONE_MILLION), s.lines))
                    .map(|s| s.template_count);
                let ratio = count_1m.map(|c| (c as f64) / (template_count_at_end as f64));
                (count_1m, ratio)
            } else {
                (None, None)
            };

        // Per-service decomposition, largest service first. Each
        // service's gate follows §3.4.3 on its own line count; template
        // creation is attributed to the minting service, so
        // `templates_created` sums to `template_count_at_end`.
        let mut by_service: Vec<PerServiceC2> = self
            .by_service
            .into_iter()
            .map(|(service_name, s)| {
                let (at_1m, ratio, pass) = if s.lines >= ONE_MILLION {
                    if s.created > 0 {
                        let c = s.created_at_1m.unwrap_or(s.created);
                        let ratio = (c as f64) / (s.created as f64);
                        (Some(c), Some(ratio), Some(ratio >= 0.5))
                    } else {
                        // >= 1 M lines but zero templates minted (every line
                        // NO_TEMPLATE): the count is flat at zero, the strongest
                        // possible convergence — C2's falsifier is *linear*
                        // growth, so this passes trivially with an undefined
                        // ratio. Gated (`Some`), never abstaining, so
                        // `gate_pass`'s `None` keeps meaning "no service reached
                        // 1 M lines" (an all-NO_TEMPLATE service is a body-
                        // retention / parse-failure concern, caught by §3.1's
                        // counters — not a convergence failure).
                        (Some(0), None, Some(true))
                    }
                } else {
                    (None, None, None)
                };
                PerServiceC2 {
                    service_name,
                    lines: s.lines,
                    templates_created: s.created,
                    templates_created_at_1m_lines: at_1m,
                    convergence_ratio: ratio,
                    pass,
                }
            })
            .collect();
        by_service.sort_by(|a, b| {
            b.lines
                .cmp(&a.lines)
                .then(a.service_name.cmp(&b.service_name))
        });

        let pass = gate_pass(&by_service);

        C2Result {
            sample_cadence: self.cadence,
            total_lines: self.total_lines,
            template_count_at_1m_lines,
            template_count_at_end,
            convergence_ratio,
            convergence_curve: self.curve,
            pass,
            corpus_at_least_1m,
            by_service,
            services_truncated: self.services_truncated,
        }
    }
}

/// The per-service C2 **gate** (RFC 0006 §3.4.3 as amended for #444):
/// a corpus passes iff every service with ≥ 1 M lines passes its own
/// ratio ≥ 0.5. `finalize` sets `pass = Some(_)` for *every* ≥ 1 M
/// service (a zero-template service passes trivially — flat count), so
/// the fold below considers exactly the gated services (`s.pass.is_some()`)
/// and `None` means "no service reached 1 M lines", never a
/// silently-dropped ≥ 1 M service. A single-service
/// corpus — including the plain-text `<unknown>` bucket — is gated on
/// that one service's ratio, measured at its **exact** millionth line
/// (`created_at_1m`). That reproduces the pre-#444 whole-corpus verdict
/// for every historical converged corpus (their ratio sits far from the
/// 0.5 boundary); it is not bit-identical to the whole-corpus
/// `convergence_ratio`, which is sampled at the nearest curve point and
/// is only a diagnostic. Only multi-service OTLP corpora change verdict.
fn gate_pass(by_service: &[PerServiceC2]) -> Option<bool> {
    let mut verdict = None;
    for s in by_service {
        match s.pass {
            // Any gated service that fails is decisive — short-circuit.
            Some(false) => return Some(false),
            Some(true) => verdict = Some(true),
            None => {}
        }
    }
    verdict
}

/// The record's `service.name` resource attribute, or a sentinel when
/// absent. Borrowed — the caller copies into the map key only on a
/// first sighting.
fn service_name(emitted: &MinedRecord) -> &str {
    emitted
        .resource_attributes
        .iter()
        .find(|kv| kv.key == SERVICE_NAME_KEY)
        .and_then(|kv| match kv.value.as_ref()?.value.as_ref()? {
            AnyValueKind::StringValue(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or(UNKNOWN_SERVICE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive `observe` over `total_lines` lines whose
    /// `template_id`s cycle through `1..=distinct` (a bounded,
    /// stable alphabet), then finalize. Pure counter work — no
    /// miner, no disk — so even a 1 M-line run is milliseconds.
    fn run_stable(total_lines: u64, distinct: u64) -> C2Result {
        let mut acc = C2Accumulator::new(total_lines);
        for i in 0..total_lines {
            // ids in 1..=distinct (0 would be NO_TEMPLATE).
            acc.observe((i % distinct) + 1);
        }
        acc.finalize()
    }

    /// Cadence bounds the curve to ≤ 1024 samples, and the
    /// curve length is exactly `ceil(total_lines / cadence)`
    /// (the RFC0006.3 assertion).
    #[test]
    fn cadence_bounds_curve_length() {
        for total in [1_u64, 100, 1024, 1025, 50_000, 1_000_000] {
            let r = run_stable(total, 4);
            assert!(r.sample_cadence >= 1);
            assert!(
                r.convergence_curve.len() as u64 <= SAMPLE_BUDGET,
                "curve exceeds 1024 samples for total={total}",
            );
            assert_eq!(
                r.convergence_curve.len() as u64,
                total.div_ceil(r.sample_cadence),
                "curve length must equal ceil(total / cadence) for total={total}",
            );
            // The final sample always covers the last line.
            assert_eq!(
                r.convergence_curve.last().unwrap().lines,
                total,
                "final sample is the last line for total={total}",
            );
        }
    }

    /// A ≥ 1 M-line corpus with a bounded alphabet plateaus
    /// immediately, so `count_1m == SS` → whole-corpus diagnostic
    /// ratio 1.0. Exercises the ≥ 1 M ratio math at scale without
    /// the miner; the per-service *gate* abstains here (no
    /// `service.name`), as the body notes.
    #[test]
    fn stable_curve_ratio_is_one() {
        // `run_stable` drives `observe` (no `service.name`), so it
        // exercises the whole-corpus ratio *diagnostic*; the per-service
        // gate needs record input and is covered by `gate_pass_*` +
        // the partition test.
        let r = run_stable(1_000_000, 8);
        assert!(r.corpus_at_least_1m);
        assert_eq!(r.template_count_at_end, 8);
        assert_eq!(r.template_count_at_1m_lines, Some(8));
        assert_eq!(r.convergence_ratio, Some(1.0));
        // No service data → the per-service gate abstains.
        assert_eq!(r.pass, None);
    }

    /// A corpus below 1 M lines has no 1 M count, no ratio; the gate
    /// abstains.
    #[test]
    fn short_corpus_abstains() {
        let r = run_stable(10_000, 5);
        assert!(!r.corpus_at_least_1m);
        assert_eq!(r.template_count_at_1m_lines, None);
        assert_eq!(r.convergence_ratio, None);
        assert_eq!(r.pass, None);
        // The curve is still produced (a diagnostic), and SS is
        // the bounded alphabet size.
        assert_eq!(r.template_count_at_end, 5);
    }

    /// The per-service gate fold: pass iff every ≥ 1 M service passes;
    /// abstain when none are gated; a `<1 M` service (pass = None) does
    /// not veto a passing sibling.
    #[test]
    fn gate_pass_folds_over_gated_services() {
        let svc = |name: &str, pass: Option<bool>| PerServiceC2 {
            service_name: name.to_string(),
            lines: 0,
            templates_created: 0,
            templates_created_at_1m_lines: None,
            convergence_ratio: None,
            pass,
        };
        // No gated service → abstain.
        assert_eq!(gate_pass(&[svc("a", None), svc("b", None)]), None);
        // All gated services pass → pass.
        assert_eq!(
            gate_pass(&[svc("a", Some(true)), svc("b", None), svc("c", Some(true))]),
            Some(true)
        );
        // One gated service fails → fail (even with passing siblings).
        assert_eq!(
            gate_pass(&[svc("a", Some(true)), svc("b", Some(false))]),
            Some(false)
        );
    }

    /// A corpus whose template count is still climbing steeply
    /// at 1 M lines (no plateau) fails the gate: `count_1m` is
    /// far under half the end count.
    #[test]
    fn non_converging_curve_fails_the_gate() {
        // Phase 1 (first 1 M lines): a bounded alphabet of 10
        // templates, so the count at 1 M is ~10. Phase 2
        // (next 1 M lines): a brand-new template every line, so
        // the end count is ~1 M. count_1m / SS ≈ 10 / 1_000_010
        // ≪ 0.5 → the gate fails unambiguously. (Distinct
        // phase-2 ids start at 2 M so they never collide with
        // the 1..=10 alphabet.)
        let total = 2_000_000u64;
        let mut acc = C2Accumulator::new(total);
        for i in 0..total {
            let id = if i < 1_000_000 {
                (i % 10) + 1
            } else {
                2_000_000 + i
            };
            acc.observe(id);
        }
        let r = acc.finalize();
        assert!(r.corpus_at_least_1m);
        let ratio = r.convergence_ratio.expect("ratio on ≥1M corpus");
        assert!(
            ratio < 0.5,
            "templates still climbing at 1 M → whole-corpus ratio {ratio} must be < 0.5",
        );
        // `observe` has no service data, so the per-service gate abstains
        // here — the fold's fail path is covered by `gate_pass_*`.
        assert_eq!(r.pass, None);
    }

    /// A `MinedRecord` carrying just the two fields the per-service
    /// decomposition reads: `template_id` and a `service.name` resource
    /// attribute.
    fn rec(template_id: u64, service: &str) -> MinedRecord {
        use ourios_core::otlp::{AnyValue, KeyValue};
        use ourios_core::record::BodyKind;
        use ourios_core::tenant::TenantId;
        MinedRecord {
            tenant_id: TenantId::new("bench-tenant"),
            template_id,
            template_version: 0,
            severity_number: 9,
            severity_text: None,
            scope_name: None,
            scope_version: None,
            scope_attributes: Vec::new(),
            resource_schema_url: None,
            scope_schema_url: None,
            time_unix_nano: 0,
            observed_time_unix_nano: None,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            resource_attributes: vec![KeyValue {
                key: SERVICE_NAME_KEY.to_string(),
                value: Some(AnyValue {
                    value: Some(AnyValueKind::StringValue(service.to_string())),
                }),
                ..Default::default()
            }],
            trace_id: None,
            span_id: None,
            flags: 0,
            event_name: None,
            body_kind: BodyKind::String,
            params: Vec::new(),
            separators: vec![String::new(), String::new()],
            body: None,
            confidence: 1.0,
            lossy_flag: false,
        }
    }

    /// The per-service decomposition attributes each template creation
    /// to the service of the *creating* line, so per-service creations
    /// partition the whole-corpus end count exactly. Two services share
    /// the id space (ids interleave), which the max-id attribution must
    /// handle without a per-service id set.
    #[test]
    fn per_service_creations_partition_the_whole() {
        // "svc-a" mints ids 1,2; "svc-b" mints ids 3,4,5. Interleaved,
        // with reuse — but first-appearances stay monotonic (1,2,3,4,5),
        // the miner's id-allocation invariant the max-id attribution
        // relies on (RFC 0001 §6.1: ids are handed out in creation
        // order). A script that minted id 2 *after* id 3 would be
        // physically impossible from the miner and would (correctly)
        // not register as a creation.
        let script = [
            (1, "svc-a"), // a creates 1
            (2, "svc-a"), // a creates 2
            (3, "svc-b"), // b creates 3
            (1, "svc-a"), // a reuse
            (4, "svc-b"), // b creates 4
            (5, "svc-b"), // b creates 5
            (2, "svc-a"), // a reuse
            (3, "svc-b"), // b reuse
        ];
        let mut acc = C2Accumulator::new(script.len() as u64);
        for (id, svc) in script {
            acc.record(&rec(id, svc));
        }
        let r = acc.finalize();
        assert_eq!(r.template_count_at_end, 5, "5 distinct templates overall");
        assert_eq!(r.by_service.len(), 2);
        let a = r
            .by_service
            .iter()
            .find(|s| s.service_name == "svc-a")
            .unwrap();
        let b = r
            .by_service
            .iter()
            .find(|s| s.service_name == "svc-b")
            .unwrap();
        assert_eq!(a.templates_created, 2, "svc-a minted ids 1,2");
        assert_eq!(b.templates_created, 3, "svc-b minted ids 3,4,5");
        assert_eq!(
            a.templates_created + b.templates_created,
            r.template_count_at_end,
            "per-service creations partition the whole",
        );
        // Both services are < 1 M lines → each abstains.
        assert_eq!(a.pass, None);
        assert_eq!(b.pass, None);
        // Sorted largest-first: svc-a and svc-b both have 4 lines, tie
        // broken by name → svc-a first.
        assert_eq!(r.by_service[0].service_name, "svc-a");
    }

    /// A record with no `service.name` attribute lands in the
    /// `<unknown>` bucket rather than being dropped.
    #[test]
    fn missing_service_name_falls_back_to_unknown() {
        let mut acc = C2Accumulator::new(2);
        acc.record(&rec(1, "svc")); // has service.name
        let mut bare = rec(2, "svc");
        bare.resource_attributes.clear();
        acc.record(&bare);
        let r = acc.finalize();
        assert!(
            r.by_service
                .iter()
                .any(|s| s.service_name == UNKNOWN_SERVICE)
        );
        assert_eq!(
            r.by_service.iter().map(|s| s.lines).sum::<u64>(),
            2,
            "every line is attributed to some bucket",
        );
    }

    /// A service that clears the 1 M-line floor but mints zero templates
    /// (every line `NO_TEMPLATE`) is **gated** and passes trivially — its
    /// count is flat at zero, the opposite of the linear growth C2 flags.
    /// Regression for the fold (#451): such a service must not collapse to
    /// `pass = None` and get silently dropped, which would leave `None`
    /// meaning both "below 1 M lines" and "≥ 1 M but degenerate".
    #[test]
    fn zero_template_service_over_1m_passes_trivially() {
        let quiet = rec(NO_TEMPLATE, "quiet-svc");
        let mut acc = C2Accumulator::new(ONE_MILLION);
        for _ in 0..ONE_MILLION {
            acc.record(&quiet);
        }
        let r = acc.finalize();
        let svc = r
            .by_service
            .iter()
            .find(|s| s.service_name == "quiet-svc")
            .expect("quiet-svc bucket");
        assert_eq!(svc.lines, ONE_MILLION);
        assert_eq!(svc.templates_created, 0, "no template ever minted");
        assert_eq!(svc.convergence_ratio, None, "0/0 ratio is undefined");
        assert_eq!(svc.pass, Some(true), "flat count → trivial convergence");
        // Gated as a PASS, not folded away as an abstention.
        assert_eq!(r.pass, Some(true));
        assert_eq!(r.template_count_at_end, 0);
        // Whole-corpus diagnostic stays a matched pair (SS = 0 → both
        // `None`, never the mixed `(Some(0), None)` the report layer
        // rejects) even though the corpus cleared 1 M lines.
        assert!(r.corpus_at_least_1m);
        assert_eq!(r.template_count_at_1m_lines, None);
        assert_eq!(r.convergence_ratio, None);
    }
}
