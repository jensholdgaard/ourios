//! Pair specs, gate math, latency channel, the template-map probe,
//! measurement splitting, and the machine-readable results record.

use crate::*;

/// Build the L4 `PairSpec` from a picked [`FrequencyPair`]. The `regexp`
/// argument is backtick-delimited (a `LogQL`/Go raw string literal): the
/// pattern already carries Go RE2 escapes (`\s+`, `\S+`, and any
/// `regex_escape`d metacharacter) from [`param_capture_regex`], and a
/// double-quoted `LogQL` string literal would try to interpret those same
/// backslashes as *its own* escape sequences — `\s` is not a valid one,
/// so Loki's parser rejects the query with "invalid char escape" before
/// the pattern ever reaches the regex engine. Returns `None` (loudly
/// logged) if `capture_regex` itself contains a backtick — the fixed
/// corpus token would prematurely close the raw string, and `regex_escape`
/// does not escape backticks (they are not an RE2 metacharacter).
pub(crate) fn l4_pair_spec(
    pair: &FrequencyPair,
    min_effective_time_unix_nano: u64,
    max_effective_time_unix_nano: u64,
    now: u64,
    window: u64,
    margins: &ourios_bench::ComparativeMargins,
) -> Option<PairSpec> {
    if pair.capture_regex.contains('`') {
        eprintln!(
            "L4 pair template_id={} param({}): rejected — capture_regex contains a \
             backtick, which would break the LogQL raw-string delimiter: {:?}",
            pair.template_id, pair.param, pair.capture_regex,
        );
        return None;
    }
    // Loki's `query_range` evaluates a fixed step-grid starting exactly
    // at `start` (`start, start+step, start+2*step, ..., end`), each
    // instant's `[bucket_width]` range-vector lookback covering the
    // PRECEDING window. Ourios's own buckets (RFC 0002's `bucket(width)`)
    // are epoch-aligned (`floor(ts/width)*width`), not aligned to
    // whatever `min_effective_time_unix_nano` happens to be. Runs
    // #6-#10 all converged L4 to a stable ~93-97% regardless of poll
    // deadline (ruling out a timing race) even after the entries-limit
    // fix (run #8) closed part of the gap — consistent with a
    // structural, not transient, shortfall: unless `start`/`end` are
    // themselves bucket-boundary multiples, `(end - start) % width` is
    // generically nonzero, leaving a fractional sliver at the tail of
    // the range that the step-grid never evaluates a window over at
    // all, independent of how long Loki is given to ingest. Snapping
    // `start` down and `end` up to the nearest bucket boundary costs
    // nothing (no data exists outside `[min, max]` to inflate the
    // count) and guarantees full coverage.
    //
    // Reviewed concern: doesn't the step-grid's first evaluated instant
    // (`t = start`) waste a step on `parse_loki_matrix`'s `bucket_start =
    // t - width` decoding, losing the first real bucket? No — at
    // `t = start`, `count_over_time`'s lookback window `(start - width,
    // start]` covers only time strictly before `start`, where no corpus
    // data exists by construction (that's the whole point of the
    // bucket-aligned `start`). That instant decodes to an empty,
    // zero-count bucket that's simply never present in Loki's response
    // (Prometheus range-vector queries omit empty series/samples), so
    // it's silently and correctly dropped. The first REAL bucket,
    // `[start, start + width)`, is covered by the *next* evaluated
    // instant, `t = start + width`, whose window `(start, start +
    // width]` decodes to `bucket_start = start` — exactly right.
    // Every subsequent instant follows the same pattern, so all `N` real
    // buckets in `[start, end)` are covered by the `N` evaluated
    // instants after the wasted first one, not lost to it.
    let bucket_width_ns = bucket_width_seconds(&pair.bucket_width)
        .checked_mul(1_000_000_000)
        .expect("bucket width fits u64 nanoseconds");
    let start = (min_effective_time_unix_nano / bucket_width_ns) * bucket_width_ns;
    let end = max_effective_time_unix_nano
        .checked_add(1)
        .expect("corpus max timestamp overflows the Loki window end")
        .div_ceil(bucket_width_ns)
        .checked_mul(bucket_width_ns)
        .expect("bucket-aligned window end overflows u64 nanoseconds");
    Some(PairSpec {
        label: format!(
            "frequency aggregation, L4 family: template_id={} param({}) bucket({})",
            pair.template_id, pair.param, pair.bucket_width,
        ),
        margin: margins.m_l4,
        class: PairClass::L4,
        dsl: format!(
            "template_id == {} | count by param({}), bucket({})",
            pair.template_id, pair.param, pair.bucket_width,
        ),
        logql: format!(
            "sum by (value) (count_over_time({{service_name=~\".+\"}} |= \"{}\" \
             | regexp `{}` [{}]))",
            pair.needle, pair.capture_regex, pair.bucket_width,
        ),
        start,
        end,
        expected_rows: pair.groups.values().sum(),
        now,
        window,
    })
}

/// Timed repetitions per pair per system for the §3.6 latency channel.
/// Every repetition runs AFTER the pair's correctness measurement, so
/// all of them — including the first — may be cache-warm on both sides:
/// the reported figure is a **warm p50** (median, never min, so one
/// especially-warm rep cannot masquerade as the typical cost).
pub(crate) const LATENCY_REPS: usize = 7;

/// Median wall time of a rep set: odd lengths take the middle sample,
/// even lengths the mean of the two middles. `None` on an empty set.
pub(crate) fn median_duration(samples: &[Duration]) -> Option<Duration> {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => Some(sorted[mid]),
        _ => Some((sorted[mid - 1] + sorted[mid]) / 2),
    }
}

/// The RFC0031.7 latency floor over the measured p50s, through the
/// shared checked gate math ([`ourios_bench::bytes_within_floor`] —
/// its arithmetic is unit-agnostic) on **nanosecond** integers:
/// millisecond units would truncate a sub-millisecond in-process Ourios
/// p50 to 0 and trip the gate's zero-measurement guard.
pub(crate) fn latency_floor_gate(
    ourios: Duration,
    loki: Duration,
    factor: u64,
) -> ourios_bench::BytesGateOutcome {
    // A p50 past u64::MAX nanoseconds (~584 years) is a broken clock;
    // saturating keeps the gate's own guards in charge of failing it.
    let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    ourios_bench::bytes_within_floor(nanos(ourios), nanos(loki), factor)
}

/// The Ourios half of the §3.6 latency channel: [`LATENCY_REPS`] timed
/// end-to-end repetitions of the pair's own query through
/// [`ourios_bench::ourios_query_answer`] — the honest per-query cost
/// (count scan + row materialization + template-map acquisition, fresh
/// querier and runtime per rep; once the RFC 0033 write-through has
/// published, the acquisition is one warm artifact GET — the production
/// steady state), median wall time. Ourios is timed
/// **in-process** (no HTTP layer) while Loki is timed over localhost
/// HTTP: negligible against multi-second scans, not against
/// sub-millisecond answers — the report carries that caveat, and
/// latency stays corroborating, not sole-gating (RFC 0031 §2.5). A
/// failed repetition logs loudly and yields `None` ("latency:
/// unmeasured") rather than failing the run.
pub(crate) fn ourios_latency_p50(
    bucket_root: &std::path::Path,
    tenant: &TenantId,
    spec: &PairSpec,
) -> Option<Duration> {
    let mut samples = Vec::with_capacity(LATENCY_REPS);
    for _ in 0..LATENCY_REPS {
        let start = std::time::Instant::now();
        if let Err(e) =
            ourios_bench::ourios_query_answer(bucket_root, tenant, &spec.dsl, spec.now, spec.window)
        {
            eprintln!(
                "LATENCY UNMEASURED (ourios) [{}]: a timed repetition failed: {e:?}",
                spec.label,
            );
            return None;
        }
        samples.push(start.elapsed());
    }
    median_duration(&samples)
}

/// Which direction a pair's bytes gate asks its question in (RFC 0031
/// §2 dispositions): the L1–L4 classes must WIN by the margin, the
/// L6/L7 family must merely stay WITHIN the floor factor.
#[derive(Clone, Copy)]
pub(crate) enum GateKind {
    MustWin,
    Floor,
}

impl GateKind {
    pub(crate) fn evaluate(
        self,
        ourios: u64,
        loki: u64,
        calibration: u64,
    ) -> ourios_bench::BytesGateOutcome {
        match self {
            Self::MustWin => ourios_bench::bytes_must_win(ourios, loki, calibration),
            Self::Floor => ourios_bench::bytes_within_floor(ourios, loki, calibration),
        }
    }
}

/// The RFC 0031 taxonomy class a pair belongs to: which §7 value
/// applies, on which channel, and whether it is FROZEN (asserted by the
/// dispatch run) or deferred/diagnostic (reported only) — per the §7
/// partial freeze (2026-07-13, calibrated against `benchmarks.md`
/// §9.13) and the `M_L2` unfreeze (2026-07-14, §9.15).
#[derive(Clone, Copy, Debug)]
pub(crate) enum PairClass {
    /// L1 template lookup — must-win, `M_L1 = 10` frozen on the
    /// storage channel.
    L1,
    /// L2 attribute predicate — must-win, `M_L2 = 10` frozen on the
    /// processed channel (primary) plus a frozen 1.1× storage-side
    /// floor (the 2026-07-14 unfreeze: RFC 0033's artifact moved the
    /// storage channel past parity).
    L2,
    /// L3 trace correlation — must-win, `M_L3 = 10` frozen on the
    /// storage channel.
    L3,
    /// L4 frequency aggregation — must-win (RFC 0031 §3.4), but `M_L4`
    /// stays §7-DEFERRED until first measured: both bytes channels are
    /// PRINTED, never asserted (`frozen_gate_failures` skips this class).
    L4,
    /// L6-family window browse — `F_L6 = 3` frozen on the LATENCY
    /// channel (RFC0031.7 as written); the bytes channel is a
    /// published diagnostic, not a gate.
    WindowFloor,
    /// Selective-resource window — diagnostic only; nothing gates.
    Diagnostic,
}

impl PairClass {
    /// The bytes-gate direction the class's channels report under.
    pub(crate) fn gate(self) -> GateKind {
        match self {
            Self::L1 | Self::L2 | Self::L3 | Self::L4 => GateKind::MustWin,
            Self::WindowFloor | Self::Diagnostic => GateKind::Floor,
        }
    }

    /// The class's stable name in the results artifact
    /// ([`comparative_results_json`]) — part of that JSON's schema, so
    /// renaming a variant must not silently rename the recorded series.
    /// [`ClassFilter`] accepts the same names, so the artifact and the
    /// dispatch input speak one vocabulary.
    pub(crate) fn artifact_name(self) -> &'static str {
        match self {
            Self::L1 => "L1",
            Self::L2 => "L2",
            Self::L3 => "L3",
            Self::L4 => "L4",
            Self::WindowFloor => "window-floor",
            Self::Diagnostic => "diagnostic",
        }
    }

    /// Every class, in artifact-name order — the parse table for
    /// [`ClassFilter`] and the exhaustiveness anchor a new variant
    /// must be added to (the compiler can't check array completeness,
    /// but `class_filter_knows_every_class` does).
    pub(crate) const ALL: [Self; 6] = [
        Self::L1,
        Self::L2,
        Self::L3,
        Self::L4,
        Self::WindowFloor,
        Self::Diagnostic,
    ];
}

/// Which taxonomy classes a dispatch measures (issue #538 item 4): the
/// `OURIOS_COMPARATIVE_CLASSES` env var, a comma-separated list of
/// [`PairClass::artifact_name`]s (`"L1,L4"`), lets a targeted re-run —
/// say, re-verifying one flaky pair — skip every other class's
/// measurement polls and latency reps instead of paying the full ~2 h.
/// Unset or `"all"` measures everything (every prior dispatch's
/// behavior, unchanged). An unknown name panics loudly: a typo that
/// silently measured nothing would read as a mysteriously-thin run.
///
/// The filter selects which pairs are MEASURED; it never touches what a
/// measured pair asserts (equivalence and the frozen gates apply to
/// every pair that runs). A filtered run's results artifact records the
/// filter so the trend series can distinguish full runs from partial
/// ones.
#[derive(Clone, Debug)]
pub(crate) struct ClassFilter(Vec<PairClass>);

impl ClassFilter {
    /// Parse `raw` (the env var's value). See the type doc for the
    /// grammar; `parse_class_filter_*` tests pin it.
    pub(crate) fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("all") {
            return Self(PairClass::ALL.to_vec());
        }
        let mut classes: Vec<PairClass> = Vec::new();
        for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let class = PairClass::ALL
                .into_iter()
                .find(|c| c.artifact_name() == token)
                .unwrap_or_else(|| {
                    panic!(
                        "OURIOS_COMPARATIVE_CLASSES: unknown class {token:?} (valid: {}, \
                         or \"all\")",
                        PairClass::ALL.map(PairClass::artifact_name).join(", "),
                    )
                });
            // First-seen dedup: `L1,L1` is one class, and a list naming
            // every class (with or without repeats) is a FULL run —
            // `artifact_value` keys off the count, so duplicates must
            // not make a full run read as partial.
            if !classes
                .iter()
                .any(|c| c.artifact_name() == class.artifact_name())
            {
                classes.push(class);
            }
        }
        assert!(
            !classes.is_empty(),
            "OURIOS_COMPARATIVE_CLASSES parsed to an empty set — a run that measures \
             nothing is never what a dispatcher meant",
        );
        Self(classes)
    }

    /// The `OURIOS_COMPARATIVE_CLASSES` env var, or the full set when
    /// unset.
    pub(crate) fn from_env() -> Self {
        std::env::var("OURIOS_COMPARATIVE_CLASSES")
            .map_or_else(|_| Self(PairClass::ALL.to_vec()), |raw| Self::parse(&raw))
    }

    pub(crate) fn includes(&self, class: PairClass) -> bool {
        self.0
            .iter()
            .any(|c| c.artifact_name() == class.artifact_name())
    }

    /// The filter as it should appear in the results artifact: `None`
    /// (serialized `null`) for a full run, the selected names for a
    /// partial one — so the trend series can tell them apart.
    pub(crate) fn artifact_value(&self) -> Option<Vec<&'static str>> {
        if self.0.len() == PairClass::ALL.len() {
            None
        } else {
            Some(self.0.iter().map(|c| c.artifact_name()).collect())
        }
    }
}

/// One measured query of the indicative run: the equivalent DSL/`LogQL`
/// question, the window both systems answer it over, and the row count
/// both must return exactly.
#[derive(Clone, Debug)]
pub(crate) struct PairSpec {
    pub(crate) label: String,
    /// The pair's §7 calibration value (`m_l1`/`m_l2`/`m_l3` for the
    /// must-win classes; `f_l6` for the window slices — the latency
    /// floor factor, since the §7 freeze reclassified their bytes
    /// channel to a diagnostic).
    pub(crate) margin: u64,
    /// The taxonomy class: which channel gates, which is reported, and
    /// whether the §7 value is frozen or deferred.
    pub(crate) class: PairClass,
    pub(crate) dsl: String,
    pub(crate) logql: String,
    /// Loki `query_range` window (nanoseconds, `[start, end)` by the
    /// clean-edge construction).
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) expected_rows: u64,
    /// The Ourios querier's window parameters (`time_window_filter` is
    /// `ts ≥ now − window ∧ ts < now`). For the time-window slices these
    /// map exactly to `[start, end)` (`now = end`, `window = end −
    /// start`); the severity pair instead uses the full-corpus
    /// effective-time window — there both sides' windows are supersets
    /// of every matching row and the severity predicate does the
    /// selecting.
    pub(crate) now: u64,
    pub(crate) window: u64,
}

/// The run's pairs — a selectivity curve plus the L-class points, not a
/// single number. Run #6 measured ONE extreme-selectivity pair (1 row)
/// and found storage-side advantage 5.95×: Ourios's fixed per-query
/// reads dominate when the answer is a single row. The curve tests the
/// amortization prediction — Ourios's fixed cost should wash out as the
/// result grows while Loki's scan grows with it. The severity pair is
/// the L2-family point; the two time-window slices (~100 and ~2000 rows
/// on the same service) are broad scans, so their gates report under
/// the L6 floor factor. The trace pair is the L3 must-win; the template
/// pair is the L1 must-win — the taxonomy's flagship class.
/// Everything the pickers produced, bundled for [`build_pair_specs`].
pub(crate) struct Picks<'a> {
    pub(crate) pair: &'a SelectivePair,
    pub(crate) clean_ts: &'a [u64],
    pub(crate) poison_ts: &'a [u64],
    pub(crate) trace: Option<&'a (String, u64)>,
    pub(crate) template: Option<&'a TemplatePair>,
    pub(crate) rare_window: Option<&'a (String, u64, u64, u64)>,
}

pub(crate) fn build_pair_specs(
    picks: &Picks<'_>,
    class_filter: &ClassFilter,
    corpus_now: u64,
    corpus_window: u64,
) -> Vec<PairSpec> {
    let (pair, clean_ts, poison_ts) = (picks.pair, picks.clean_ts, picks.poison_ts);
    let margins = ourios_bench::ComparativeMargins::default();
    // The filter applies DURING construction, not as a post-hoc retain:
    // an excluded class must impose no preconditions — the L6 window
    // loop below hard-panics when no clean window exists, and a
    // `classes=L1` dispatch must not die on a precondition of a class
    // it never asked to measure.
    let mut specs = Vec::new();
    if class_filter.includes(PairClass::L2) {
        specs.push(PairSpec {
            label: format!(
                "severity, L2 family: service={} severity>={} (text={:?})",
                pair.service, pair.threshold, pair.text
            ),
            margin: margins.m_l2,
            class: PairClass::L2,
            dsl: format!(
                "service == \"{}\" and severity >= {} | limit 5000",
                pair.service, pair.threshold
            ),
            logql: format!(
                "{{service_name=\"{}\"}} | severity_text=\"{}\"",
                pair.service, pair.text
            ),
            start: pair.min_ts,
            end: pair
                .max_ts
                .checked_add(1)
                .expect("corpus max timestamp overflows the Loki window end"),
            expected_rows: pair.rows,
            now: corpus_now,
            window: corpus_window,
        });
    }
    if class_filter.includes(PairClass::WindowFloor) {
        for k in [100usize, 2000] {
            let Some((start, end)) = pick_window_pair(clean_ts, poison_ts, k) else {
                panic!(
                    "no clean {k}-row window on service {} ({} clean rows, {} poison)",
                    pair.service,
                    clean_ts.len(),
                    poison_ts.len(),
                );
            };
            specs.push(PairSpec {
                label: format!(
                    "time-window slice, L6 family: service={} k={k}",
                    pair.service
                ),
                margin: margins.f_l6,
                class: PairClass::WindowFloor,
                dsl: format!("service == \"{}\" | limit 5000", pair.service),
                logql: format!("{{service_name=\"{}\"}}", pair.service),
                start,
                end,
                expected_rows: k as u64,
                now: end,
                window: end - start,
            });
        }
    }
    specs.extend(class_pair_specs(
        picks,
        class_filter,
        &margins,
        corpus_now,
        corpus_window,
    ));
    specs
}

/// The L-class (L3/L1) must-win pairs plus the selective-resource
/// diagnostic — split from [`build_pair_specs`] so each half stays
/// readable.
pub(crate) fn class_pair_specs(
    picks: &Picks<'_>,
    class_filter: &ClassFilter,
    margins: &ourios_bench::ComparativeMargins,
    corpus_now: u64,
    corpus_window: u64,
) -> Vec<PairSpec> {
    let (pair, trace, template, rare_window) =
        (picks.pair, picks.trace, picks.template, picks.rare_window);
    let mut specs = Vec::new();
    // L3 must-win: an exact trace lookup over the full corpus window.
    // Loki cannot pre-narrow a trace to a stream (`.+` selector +
    // structured-metadata filter = a scan across every service); Ourios
    // compiles it to a trace_id column equality. Skipping when no
    // eligible trace exists is LOUD (stderr + a 3-section report), never
    // silent.
    if class_filter.includes(PairClass::L3) {
        match trace {
            Some((hex, rows)) => specs.push(PairSpec {
                label: format!("trace correlation, L3 family: trace_id={hex}"),
                margin: margins.m_l3,
                class: PairClass::L3,
                dsl: format!("trace_id == \"{hex}\" | limit 5000"),
                logql: format!("{{service_name=~\".+\"}} | trace_id=\"{hex}\""),
                start: pair.min_ts,
                end: pair
                    .max_ts
                    .checked_add(1)
                    .expect("corpus max timestamp overflows the Loki window end"),
                expected_rows: *rows,
                now: corpus_now,
                window: corpus_window,
            }),
            None => eprintln!(
                "L3 PAIR SKIPPED: no eligible trace (16-byte id, no zero-ts/empty-service rows, \
             2..=100 rows) in the corpus"
            ),
        }
    } else {
        eprintln!("L3 PAIR SKIPPED by OURIOS_COMPARATIVE_CLASSES");
    }
    // L1 must-win — the taxonomy's flagship: DSL `template_id == N` rides
    // the writer's existing bloom filter on template_id; Loki has no
    // template concept, so its honest equivalent is a line filter over
    // every stream. The picker proved the two select IDENTICAL row sets
    // (needle-count == template row count + rendered-row containment).
    if class_filter.includes(PairClass::L1) {
        match template {
            Some(t) => specs.push(PairSpec {
                label: format!(
                    "template lookup, L1 family: template_id={} needle={:?}",
                    t.template_id, t.needle
                ),
                margin: margins.m_l1,
                class: PairClass::L1,
                dsl: format!("template_id == {} | limit 5000", t.template_id),
                logql: format!("{{service_name=~\".+\"}} |= \"{}\"", t.needle),
                start: pair.min_ts,
                end: pair
                    .max_ts
                    .checked_add(1)
                    .expect("corpus max timestamp overflows the Loki window end"),
                expected_rows: t.rows,
                now: corpus_now,
                window: corpus_window,
            }),
            None => eprintln!(
                "L1 PAIR SKIPPED: no template with a validated constant needle (≥ 10 safe \
             chars, 2..=4000 rows, needle-count == template row count, no \
             zero-ts/empty-service match) in the corpus"
            ),
        }
    } else {
        eprintln!("L1 PAIR SKIPPED by OURIOS_COMPARATIVE_CLASSES");
    }
    // Selective-resource DIAGNOSTIC (not an L-class gate): the same
    // window-browse shape as the L6 pairs but scoped to the corpus's
    // LOWEST-volume service, where the promoted service.name bloom can
    // actually skip row groups — the L6 pairs use the highest-volume
    // service, the bloom's worst case, so they bound the unscoped-browse
    // cost while this bounds the enriched one.
    if class_filter.includes(PairClass::Diagnostic) {
        match rare_window {
            Some((service, start, end, rows)) => specs.push(PairSpec {
                label: format!("selective-resource window, diagnostic: service={service} k={rows}"),
                margin: margins.f_l6,
                class: PairClass::Diagnostic,
                dsl: format!("service == \"{service}\" | limit 5000"),
                logql: format!("{{service_name=\"{service}\"}}"),
                start: *start,
                end: *end,
                expected_rows: *rows,
                now: *end,
                window: *end - *start,
            }),
            None => eprintln!(
                "SELECTIVE-RESOURCE PAIR SKIPPED: no low-volume service with a clean \
             2..=100-row window in the corpus"
            ),
        }
    } else {
        eprintln!("SELECTIVE-RESOURCE (diagnostic) PAIR SKIPPED by OURIOS_COMPARATIVE_CLASSES");
    }
    specs
}

/// One pair's Ourios measurement: the correctness answer plus the §3.6
/// latency channel (`None` = unmeasured; latency never fails the run).
#[derive(Clone)]
pub(crate) struct OuriosMeasured {
    pub(crate) answer: ourios_bench::OuriosAnswer,
    pub(crate) latency_p50: Option<Duration>,
    /// The RFC 0033 template-map acquisition + publish-outcome label
    /// behind `answer.registry_bytes` — cold audit fold vs warm
    /// artifact GET, with the publish outcome the §3.2 amendment
    /// requires printed explicitly ([`TemplateMapProbe`]).
    pub(crate) template_map: String,
}

/// One pair's artifact observation, taken right after its query (the
/// state changes across pairs — the first cold miss publishes for the
/// rest): a warm hit's acquisition equals the published artifact's byte
/// size exactly (the only registry-path GET is the artifact); anything
/// else is the cold audit fold. The absent arm's publish outcome —
/// `abstained` vs `error`, run #20's ambiguity — is resolved once,
/// after every pair's timed measurement
/// ([`reproduce_publish_decision`]). `lost_race` cannot occur here: the
/// harness is the store's only writer, so it is never printed.
pub(crate) enum TemplateMapProbe {
    Warm {
        artifact_bytes: u64,
    },
    ColdPublished {
        registry_bytes: u64,
        artifact_bytes: u64,
    },
    ColdAbsent {
        registry_bytes: u64,
    },
    ColdUnreadable {
        registry_bytes: u64,
        detail: String,
    },
}

impl TemplateMapProbe {
    pub(crate) fn observe(artifact: &std::path::Path, registry_bytes: u64) -> Self {
        match std::fs::metadata(artifact) {
            Ok(meta) if meta.len() == registry_bytes => Self::Warm {
                artifact_bytes: registry_bytes,
            },
            Ok(meta) => Self::ColdPublished {
                registry_bytes,
                artifact_bytes: meta.len(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Self::ColdAbsent { registry_bytes }
            }
            Err(e) => Self::ColdUnreadable {
                registry_bytes,
                detail: e.to_string(),
            },
        }
    }

    /// The report label; `absent_outcome` is the once-computed
    /// [`reproduce_publish_decision`] string (present iff any pair
    /// observed absence).
    pub(crate) fn label(&self, absent_outcome: Option<&str>) -> String {
        match self {
            Self::Warm { artifact_bytes } => {
                format!("warm (one artifact GET, {artifact_bytes} B compressed)")
            }
            Self::ColdPublished {
                registry_bytes,
                artifact_bytes,
            } => format!(
                "cold (audit fold, {registry_bytes} B; published — artifact \
                 {artifact_bytes} B compressed)"
            ),
            Self::ColdAbsent { registry_bytes } => format!(
                "cold (audit fold, {registry_bytes} B; {})",
                absent_outcome.expect("an absent probe resolves its publish outcome"),
            ),
            Self::ColdUnreadable {
                registry_bytes,
                detail,
            } => format!("cold (audit fold, {registry_bytes} B; artifact stat failed: {detail})"),
        }
    }
}

/// Resolve run #20's abstained-vs-error ambiguity for a store left with
/// no artifact (RFC 0033 §3.2 amendment: the harness MUST print each
/// pair's publish outcome): reproduce the §3.5 publish decision — one
/// fold + serialize + compress, the exact bytes the write-through would
/// have published — against the folded audit bytes. Off the measured
/// path by construction: called once, after every pair's timed
/// measurement, and only labels.
pub(crate) fn reproduce_publish_decision(bucket: &std::path::Path, tenant: &TenantId) -> String {
    let (map, fold_bytes) = match ourios_querier::derive_template_map(
        ourios_querier::StoreRef::Local(bucket),
        tenant,
    ) {
        Ok(derived) => derived,
        Err(e) => return format!("publish outcome unresolvable — refold failed: {e}"),
    };
    match map.to_artifact_bytes() {
        Ok(bytes) if (bytes.len() as u64) < fold_bytes => format!(
            "publish error — would-be artifact {} B compressed < folded audit \
             {fold_bytes} B, yet nothing on the store",
            bytes.len(),
        ),
        Ok(bytes) => format!(
            "abstained — would-be artifact {} B compressed >= folded audit {fold_bytes} B",
            bytes.len(),
        ),
        Err(e) => format!("publish error — serialization failed: {e}"),
    }
}

/// The RFC0033.6 corpus acquisition gate (§5.6 as amended 2026-07-14,
/// the run #21 record): when any measured pair ran warm, the warm
/// artifact GET must cost at most half the cold audit fold —
/// `warm × 2 ≤ fold`, integer-exact. The fold figure comes from a cold
/// pair's measurement when the run has one; the healthy steady state is
/// all-warm (run #21), so the caller refolds once via
/// `derive_template_map` — off the timed path, after every pair's reps,
/// exactly like the publish-outcome resolution. No warm pair means the
/// gate has nothing to decide (run #20's all-cold shape is RFC 0033's
/// own red, not this gate's); a failed refold makes it non-evaluable,
/// reported LOUDLY rather than failed, mirroring the RFC0031.7
/// unmeasured-latency stance. Returns the failure in the
/// `frozen_gate_failures` message shape so the caller asserts them
/// together, AFTER the report has printed.
pub(crate) fn template_map_acquisition_failure(
    warm_artifact: Option<u64>,
    cold_fold: Option<u64>,
    refold: impl FnOnce() -> Result<u64, String>,
) -> Option<String> {
    let warm = warm_artifact?;
    let fold = match cold_fold.map_or_else(refold, Ok) {
        Ok(fold) => fold,
        Err(e) => {
            eprintln!("RFC0033.6 not evaluable this run (refold failed: {e})");
            return None;
        }
    };
    if warm == 0 || fold == 0 {
        // The lgates honesty rule: a zero measurement must not decide a
        // gate — least of all fake a pass.
        return Some(format!(
            "[template map] RFC0033.6 corpus acquisition gate invalid: zero byte-count \
             (warm={warm}, fold={fold})"
        ));
    }
    let pass = warm.checked_mul(2).is_some_and(|double| double <= fold);
    (!pass).then(|| {
        format!(
            "[template map] RFC0033.6 corpus acquisition gate (warm ≤ fold/2) failed: \
             warm artifact GET {warm} B vs cold audit fold {fold} B"
        )
    })
}

/// The L4 slot of [`comparative_results_json`], grouped: the pair spec
/// (if a candidate cleared the picker), its Loki measurement, and the
/// Ourios-side bytes from the picked [`FrequencyPair`] — all `None`able
/// independently, since L4 is measured outside the line-pair machinery.
#[derive(Clone, Copy)]
pub(crate) struct L4ArtifactInput<'a> {
    pub(crate) spec: Option<&'a PairSpec>,
    pub(crate) loki: Option<&'a Result<L4Measured, String>>,
    pub(crate) ourios_bytes: Option<u64>,
}

#[allow(clippy::type_complexity)] // one-shot plumbing tuple for the runner
/// The dispatch's machine-readable per-pair record (issue #538 item 3):
/// each pair's `expected_rows` vs the rows Loki actually returned, plus
/// both bytes channels — so run-over-run completeness is a queryable
/// series instead of a log line, and a slide toward
/// [`L4_COMPLETENESS_MARGIN`] (or an L3-style routing flicker) shows up
/// as a trend across artifacts rather than as a surprise failure.
///
/// Built from the RAW measurement results, before `split_measurements`,
/// equivalence, or any gate asserts — a failing run records its numbers
/// too, and failed runs are exactly the points a trend needs. A pair
/// whose Loki side failed to measure carries its failure string in
/// place of numbers. `schema` names this shape; additive changes only
/// (bump the suffix on anything breaking).
pub(crate) fn comparative_results_json(
    specs: &[PairSpec],
    ourios: &[OuriosMeasured],
    loki: &[Result<Measured, String>],
    l4: L4ArtifactInput<'_>,
    total_records: u64,
    class_filter: &ClassFilter,
) -> serde_json::Value {
    let L4ArtifactInput {
        spec: l4_spec,
        loki: l4_loki,
        ourios_bytes: l4_ourios_bytes,
    } = l4;
    #[allow(clippy::cast_precision_loss)]
    // Row counts are far below f64's 2^53 exact-integer range.
    let completeness = |returned: u64, expected: u64| {
        if expected == 0 {
            1.0
        } else {
            returned as f64 / expected as f64
        }
    };
    let latency_ms = |d: &Option<Duration>| d.map(|d| serde_json::json!(d.as_secs_f64() * 1_000.0));
    let pairs: Vec<serde_json::Value> = specs
        .iter()
        .zip(ourios)
        .zip(loki)
        .map(|((spec, ours), result)| match result {
            Ok((lines, processed, fetched, loki_latency)) => serde_json::json!({
                "label": spec.label,
                "class": spec.class.artifact_name(),
                "expected_rows": spec.expected_rows,
                "loki_rows": lines.len(),
                "completeness": completeness(lines.len() as u64, spec.expected_rows),
                "ourios_bytes_read": ours.answer.bytes_read,
                "loki_storage_bytes": fetched.compressed_bytes + fetched.head_chunk_bytes,
                "loki_processed_bytes": processed,
                "ourios_latency_p50_ms": latency_ms(&ours.latency_p50),
                "loki_latency_p50_ms": latency_ms(loki_latency),
            }),
            Err(detail) => serde_json::json!({
                "label": spec.label,
                "class": spec.class.artifact_name(),
                "expected_rows": spec.expected_rows,
                "failure": detail,
            }),
        })
        .collect();
    let l4 = match (l4_spec, l4_loki) {
        (Some(spec), Some(Ok((groups, processed, fetched)))) => {
            let returned: u64 = groups.values().sum();
            serde_json::json!({
                "label": spec.label,
                "class": spec.class.artifact_name(),
                "expected_rows": spec.expected_rows,
                "loki_rows": returned,
                "completeness": completeness(returned, spec.expected_rows),
                "ourios_bytes_read": l4_ourios_bytes,
                "loki_storage_bytes": fetched.compressed_bytes + fetched.head_chunk_bytes,
                "loki_processed_bytes": processed,
            })
        }
        (Some(spec), Some(Err(detail))) => serde_json::json!({
            "label": spec.label,
            "class": spec.class.artifact_name(),
            "expected_rows": spec.expected_rows,
            "failure": detail,
        }),
        _ => serde_json::Value::Null,
    };
    serde_json::json!({
        "schema": "ourios-comparative-results/v1",
        "total_records": total_records,
        "class_filter": class_filter.artifact_value(),
        "pairs": pairs,
        "l4": l4,
    })
}

/// A representative successful L4 measurement (1,149 of 1,197 rows —
/// run #21's real shape) for the results-record tests.
fn sample_l4_measurement() -> L4Measured {
    (
        HashMap::from([
            (
                ourios_bench::AggKey {
                    bucket_start_unix_nanos: 0,
                    group_key: "a".to_string(),
                },
                1_000,
            ),
            (
                ourios_bench::AggKey {
                    bucket_start_unix_nanos: 1,
                    group_key: "b".to_string(),
                },
                149,
            ),
        ]),
        3_000_000,
        LokiFetchedBytes {
            compressed_bytes: 170_000_000,
            head_chunk_bytes: 0,
        },
    )
}

#[test]
fn comparative_results_json_records_every_pair_and_the_failure_shapes() {
    let spec = |label: &str, class: PairClass, expected: u64| PairSpec {
        label: label.to_string(),
        margin: 10,
        class,
        dsl: String::new(),
        logql: String::new(),
        start: 0,
        end: 1,
        expected_rows: expected,
        now: 1,
        window: 1,
    };
    let ours = OuriosMeasured {
        answer: ourios_bench::OuriosAnswer {
            lines: Vec::new(),
            bytes_read: 1_000,
            count_scan_bytes: 0,
            materialize_bytes: 900,
            registry_bytes: 100,
        },
        latency_p50: Some(Duration::from_millis(50)),
        template_map: String::new(),
    };
    let specs = vec![
        spec("good", PairClass::L1, 4),
        spec("flaky", PairClass::L3, 9),
    ];
    let line = ourios_bench::LineKey {
        timestamp_unix_nanos: 1,
        body: b"x".to_vec(),
    };
    let loki: Vec<Result<Measured, String>> = vec![
        Ok((
            vec![line.clone(), line.clone(), line],
            2_000_000,
            LokiFetchedBytes {
                compressed_bytes: 40_000,
                head_chunk_bytes: 2_000,
            },
            None,
        )),
        Err("loki returned 0 of 9 expected rows".to_string()),
    ];
    let l4_spec = spec("freq", PairClass::L4, 1_197);
    let l4_loki: Result<L4Measured, String> = Ok(sample_l4_measurement());

    let json = comparative_results_json(
        &specs,
        &[ours.clone(), ours],
        &loki,
        L4ArtifactInput {
            spec: Some(&l4_spec),
            loki: Some(&l4_loki),
            ourios_bytes: Some(47_995_205),
        },
        4_900_000,
        &ClassFilter::parse("all"),
    );

    assert_eq!(json["schema"], "ourios-comparative-results/v1");
    assert_eq!(json["total_records"], 4_900_000);
    let good = &json["pairs"][0];
    assert_eq!(good["class"], "L1");
    assert_eq!(good["loki_rows"], 3);
    assert!((good["completeness"].as_f64().unwrap() - 0.75).abs() < 1e-12);
    assert_eq!(good["loki_storage_bytes"], 42_000);
    assert_eq!(good["ourios_latency_p50_ms"], 50.0);
    assert!(good["loki_latency_p50_ms"].is_null());
    let flaky = &json["pairs"][1];
    assert_eq!(flaky["class"], "L3");
    assert_eq!(flaky["expected_rows"], 9);
    assert!(
        flaky["failure"]
            .as_str()
            .unwrap()
            .contains("0 of 9 expected"),
        "a failed pair must carry its failure string: {flaky}",
    );
    assert!(flaky.get("loki_rows").is_none());
    let l4 = &json["l4"];
    assert_eq!(l4["loki_rows"], 1_149);
    assert!((l4["completeness"].as_f64().unwrap() - 1_149.0 / 1_197.0).abs() < 1e-12);
    assert_eq!(l4["ourios_bytes_read"], 47_995_205);
}

#[test]
fn comparative_results_json_with_no_l4_candidate_records_null_not_absence() {
    let json = comparative_results_json(
        &[],
        &[],
        &[],
        L4ArtifactInput {
            spec: None,
            loki: None,
            ourios_bytes: None,
        },
        1,
        &ClassFilter::parse("all"),
    );
    assert_eq!(json["schema"], "ourios-comparative-results/v1");
    assert!(json["l4"].is_null());
    assert_eq!(json["pairs"].as_array().unwrap().len(), 0);
}

pub(crate) fn split_measurements(
    specs: &[PairSpec],
    ourios: &[OuriosMeasured],
    loki: Vec<Result<Measured, String>>,
) -> (
    Vec<PairSpec>,
    Vec<OuriosMeasured>,
    Vec<Measured>,
    Vec<String>,
) {
    assert!(
        specs.len() == ourios.len() && specs.len() == loki.len(),
        "harness bug: {} specs vs {} ourios vs {} loki measurements — zip \
         would silently drop the tail",
        specs.len(),
        ourios.len(),
        loki.len(),
    );
    let (mut ok_specs, mut ok_ourios, mut ok_loki, mut failures) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ((spec, ours), result) in specs.iter().zip(ourios).zip(loki) {
        match result {
            Ok(measured) => {
                ok_specs.push(spec.clone());
                ok_ourios.push(ours.clone());
                ok_loki.push(measured);
            }
            Err(detail) => failures.push(detail),
        }
    }
    (ok_specs, ok_ourios, ok_loki, failures)
}

/// The §7-frozen gate checks (partial freeze 2026-07-13; `M_L2` unfrozen
/// and frozen 2026-07-14): L1/L3 must-win on the storage channel at
/// their frozen margins; L2 must-win on the processed channel (primary)
/// AND hold the frozen 1.1× storage-side floor; the L6-family window
/// pairs hold the RFC0031.7 latency floor **when latency was measured**
/// — latency is `Option` by design, and an unmeasured p50 makes the
/// frozen gate non-evaluable, which is reported LOUDLY rather than
/// failed (a corroborating-channel hiccup must not fake a floor
/// breach). The window pairs' bytes channel and the selective-resource
/// pair are published diagnostics — never checked here. L4 is a
/// must-win class (§3.4) but `M_L4` stays §7-DEFERRED, so it is excluded
/// here too — its bytes are `print_pair_bytes_gates`' job, reported not
/// asserted, until the maintainer freezes a margin against a first
/// measurement. Returns the failures instead of panicking so the caller
/// can assert them all at once, AFTER the report has printed.
pub(crate) fn frozen_gate_failures(
    specs: &[PairSpec],
    ourios: &[OuriosMeasured],
    loki: &[Measured],
) -> Vec<String> {
    let mut failures = Vec::new();
    for ((spec, ours), (_, loki_processed, loki_fetched, loki_latency)) in
        specs.iter().zip(ourios).zip(loki)
    {
        let loki_storage = loki_fetched.compressed_bytes + loki_fetched.head_chunk_bytes;
        match spec.class {
            PairClass::L1 | PairClass::L3 => {
                let outcome =
                    ourios_bench::bytes_must_win(ours.answer.bytes_read, loki_storage, spec.margin);
                if !outcome.passed() {
                    failures.push(format!(
                        "[{}] frozen storage-channel must-win (margin {}) failed: {outcome:?}",
                        spec.label, spec.margin,
                    ));
                }
            }
            PairClass::L2 => {
                let outcome = ourios_bench::bytes_must_win(
                    ours.answer.bytes_read,
                    *loki_processed,
                    spec.margin,
                );
                if !outcome.passed() {
                    failures.push(format!(
                        "[{}] frozen processed-channel must-win (margin {}) failed: {outcome:?}",
                        spec.label, spec.margin,
                    ));
                }
                let tenths = ourios_bench::ComparativeMargins::default().m_l2_storage_floor_tenths;
                let outcome = ourios_bench::bytes_must_win_tenths(
                    ours.answer.bytes_read,
                    loki_storage,
                    tenths,
                );
                if !outcome.passed() {
                    failures.push(format!(
                        "[{}] frozen storage-channel floor (must-win at {tenths}/10) \
                         failed: {outcome:?}",
                        spec.label,
                    ));
                }
            }
            PairClass::WindowFloor => match (ours.latency_p50, *loki_latency) {
                (Some(ours_p50), Some(loki_p50)) => {
                    let outcome = latency_floor_gate(ours_p50, loki_p50, spec.margin);
                    if !outcome.passed() {
                        failures.push(format!(
                            "[{}] RFC0031.7 latency floor (frozen factor {}) failed: {outcome:?}",
                            spec.label, spec.margin,
                        ));
                    }
                }
                _ => eprintln!(
                    "RFC0031.7 not evaluable this run for [{}] (latency unmeasured)",
                    spec.label,
                ),
            },
            // M_L4 is §7-deferred — reported by `print_pair_bytes_gates`,
            // never asserted here.
            PairClass::L4 | PairClass::Diagnostic => {}
        }
    }
    failures
}

#[test]
fn l4_pair_spec_uses_a_backtick_delimited_regexp_argument() {
    // The bug this guards: a double-quoted LogQL string literal
    // interprets `capture_regex`'s own Go RE2 escapes (`\s+`) as ITS
    // OWN escape sequences, and `\s` is not a valid one — Loki's
    // parser rejects the query with "invalid char escape" before the
    // pattern reaches the regex engine (the exact failure the first L4
    // dispatch hit). Backticks pass the pattern through literally.
    let pair = synthetic_frequency_pair(r"peer\s+(?P<value>\S+)");
    let margins = ourios_bench::ComparativeMargins::default();
    let spec = l4_pair_spec(
        &pair,
        0,
        1_000_000_000,
        2_000_000_000,
        2_000_000_000,
        &margins,
    )
    .expect("a plain capture_regex carries no backtick");
    assert!(
        spec.logql.contains(r"regexp `peer\s+(?P<value>\S+)`"),
        "the regexp argument must be backtick-delimited: {}",
        spec.logql,
    );
}

#[test]
fn l4_pair_spec_rejects_a_capture_regex_containing_a_backtick() {
    // A backtick in the pattern (from an unescaped fixed-token
    // neighbour — `regex_escape` does not escape backticks, since they
    // are not an RE2 metacharacter) would prematurely close the
    // backtick-delimited raw string. Reject loudly rather than emit a
    // malformed query.
    let pair = synthetic_frequency_pair("peer`s\\s+(?P<value>\\S+)");
    let margins = ourios_bench::ComparativeMargins::default();
    assert!(
        l4_pair_spec(
            &pair,
            0,
            1_000_000_000,
            2_000_000_000,
            2_000_000_000,
            &margins
        )
        .is_none(),
        "a capture_regex containing a backtick must be rejected, not emitted",
    );
}

#[test]
fn median_duration_takes_the_middle_sample() {
    let ms = Duration::from_millis;

    // The K = 7 shape: the fourth-smallest, regardless of arrival order.
    let reps = [ms(70), ms(10), ms(50), ms(30), ms(20), ms(60), ms(40)];
    assert_eq!(median_duration(&reps), Some(ms(40)));

    assert_eq!(median_duration(&[ms(9), ms(1), ms(5)]), Some(ms(5)));
    assert_eq!(median_duration(&[ms(7)]), Some(ms(7)));
    assert_eq!(
        median_duration(&[ms(4), ms(1), ms(3), ms(2)]),
        Some(ms(2) + Duration::from_micros(500)),
        "even lengths take the mean of the two middles"
    );
    assert_eq!(median_duration(&[]), None);
}

#[test]
fn latency_floor_gate_decides_in_nanoseconds() {
    // The RFC0031.7 rule at F_L6 = 3: pass iff ourios_p50 ≤ 3 × loki_p50.
    let ms = Duration::from_millis;
    assert!(latency_floor_gate(ms(300), ms(100), 3).passed());
    assert!(!latency_floor_gate(ms(301), ms(100), 3).passed());

    // Sub-millisecond p50s stay decidable — the reason the gate runs on
    // nanosecond integers, not milliseconds.
    let us = Duration::from_micros;
    assert!(latency_floor_gate(us(300), us(100), 3).passed());
    assert!(!latency_floor_gate(us(301), us(100), 3).passed());

    // The shared gate's honesty guards carry over: a zero measurement is
    // Invalid, never a pass.
    assert!(!latency_floor_gate(Duration::ZERO, ms(100), 3).passed());
    assert!(matches!(
        latency_floor_gate(Duration::ZERO, ms(100), 3),
        ourios_bench::BytesGateOutcome::Invalid { .. }
    ));
}

#[test]
fn template_map_acquisition_gate_decides_warm_against_the_fold() {
    let no_refold = || -> Result<u64, String> { panic!("a measured cold fold must be used") };

    // No warm pair: nothing to decide (run #20's all-cold shape is
    // RFC 0033's own red, not this gate's) — and no refold is spent.
    assert_eq!(
        template_map_acquisition_failure(None, Some(513_862), no_refold),
        None,
    );

    // Warm with a cold pair's measured fold: pass at exactly warm × 2 ==
    // fold, fail one byte above — the ≤ 1/2 rule of the amended §5.6.
    assert_eq!(
        template_map_acquisition_failure(Some(250_000), Some(500_000), no_refold),
        None,
    );
    let over = template_map_acquisition_failure(Some(250_001), Some(500_000), no_refold);
    assert!(
        over.as_deref().is_some_and(|f| f.contains("RFC0033.6")),
        "{over:?}",
    );

    // The all-warm steady state (run #21) refolds once: the §9.15
    // record (187,904 B warm vs the 513,862 B fold, ≈ 1/2.73) passes.
    assert_eq!(
        template_map_acquisition_failure(Some(187_904), None, || Ok(513_862)),
        None,
    );
    assert!(template_map_acquisition_failure(Some(300_000), None, || Ok(513_862)).is_some());

    // A failed refold is non-evaluable — loud, never a spurious fail
    // (the RFC0031.7 unmeasured-latency stance).
    assert_eq!(
        template_map_acquisition_failure(Some(187_904), None, || Err("boom".to_string())),
        None,
    );

    // The lgates honesty rule: a zero measurement must not decide the
    // gate — least of all a 0-byte "warm" GET faking a pass.
    assert!(template_map_acquisition_failure(Some(0), Some(513_862), no_refold).is_some());
    assert!(template_map_acquisition_failure(Some(187_904), Some(0), no_refold).is_some());
}

#[test]
fn class_filter_knows_every_class() {
    // The exhaustiveness anchor: PairClass::ALL must contain every
    // variant exactly once (the compiler can't check the array, so a
    // new variant missing from it would silently become unfilterable).
    let names: std::collections::HashSet<_> =
        PairClass::ALL.iter().map(|c| c.artifact_name()).collect();
    assert_eq!(names.len(), PairClass::ALL.len(), "duplicate in ALL");
    // One arm per variant: adding a variant breaks this match, pointing
    // here to extend ALL alongside.
    for class in PairClass::ALL {
        match class {
            PairClass::L1
            | PairClass::L2
            | PairClass::L3
            | PairClass::L4
            | PairClass::WindowFloor
            | PairClass::Diagnostic => {}
        }
    }
}

#[test]
fn class_filter_parses_all_and_subsets() {
    for full in ["all", "ALL", "", "  all  "] {
        let f = ClassFilter::parse(full);
        assert!(
            PairClass::ALL.into_iter().all(|c| f.includes(c)),
            "{full:?} must include every class",
        );
        assert!(f.artifact_value().is_none(), "{full:?} is a full run");
    }
    let f = ClassFilter::parse("L1, L4");
    assert!(f.includes(PairClass::L1));
    assert!(f.includes(PairClass::L4));
    assert!(!f.includes(PairClass::L2));
    assert!(!f.includes(PairClass::WindowFloor));
    assert_eq!(f.artifact_value(), Some(vec!["L1", "L4"]));
    let windows = ClassFilter::parse("window-floor,diagnostic");
    assert!(windows.includes(PairClass::WindowFloor));
    assert!(!windows.includes(PairClass::L4));
}

#[test]
#[should_panic(expected = "unknown class \"L9\"")]
fn class_filter_rejects_an_unknown_class_loudly() {
    let _ = ClassFilter::parse("L1,L9");
}

#[test]
fn class_filter_excluded_classes_impose_no_preconditions() {
    // The review finding this pins: a `classes=L2` dispatch must not die
    // on the L6 window loop's no-clean-window panic — an empty timestamp
    // set would panic that loop if the filter applied after construction
    // instead of during it.
    let pair = SelectivePair {
        service: "svc".to_string(),
        threshold: 17,
        text: "ERROR".to_string(),
        rows: 3,
        total_records: 10,
        min_ts: 1,
        max_ts: 9,
    };
    let picks = Picks {
        pair: &pair,
        clean_ts: &[], // would panic the window loop if it ran
        poison_ts: &[],
        trace: None,
        template: None,
        rare_window: None,
    };
    let specs = build_pair_specs(&picks, &ClassFilter::parse("L2"), 10, 10);
    assert_eq!(specs.len(), 1, "exactly the requested L2 spec: {specs:?}");
    assert!(matches!(specs[0].class, PairClass::L2));

    // And the inverse: excluding L2 with windows requested still hits
    // the window precondition — the filter skips classes, it does not
    // soften a REQUESTED class's requirements.
    let result = std::panic::catch_unwind(|| {
        build_pair_specs(&picks, &ClassFilter::parse("window-floor"), 10, 10)
    });
    assert!(
        result.is_err(),
        "a requested window class must still enforce its clean-window precondition",
    );
}
