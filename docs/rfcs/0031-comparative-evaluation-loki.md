---
rfc: 0031
title: Comparative evaluation against Grafana Loki
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-11
supersedes: —
superseded-by: —
---

# RFC 0031 — Comparative evaluation against Grafana Loki

## 1. Summary

Pins the methodology for the one measurement the project has never
made: Ourios against the incumbent it defines itself against.
`CLAUDE.md` §1 states the existence test — *"Not a Loki/Mimir/
ClickHouse clone. If the answer is 'just use $X,' we should not be
building this"* — and to date every thesis-gate in
`docs/benchmarks.md` is self-referential (Ourios versus its own full
scan, versus `zstdcat | grep`). This RFC adds **Grafana Loki** as a
second reference system and fixes the comparative methodology: the
same OTLP stream ingested into both, the same logical queries run
against both on the same hardware, and a fixed set of comparative
gates (the **`L`-gates**) written into `docs/benchmarks.md`. The
headline corpus is a **real OpenTelemetry-Demo OTLP capture** — Ourios
is an OTLP-native backend, so the honest test is real OTLP logs, the
workload we claim to do best, not a favourable plain-text corpus. The
query taxonomy is anchored to OpenTelemetry's **own** stated log
correlation/analysis model (§2.3): the four must-win classes exercise
the four ways Ourios turns OTLP structure into pruning — template id,
resource/attribute columns, high-cardinality trace context, and typed
template parameters for frequency aggregation. The load-bearing metric
is **bytes read from object storage per query** — the implementation-
independent expression of the pruning thesis — with wall-clock latency
reported as practical corroboration. Result-set equivalence
(multiset-exact), a committed and competent (non-strawman) Loki
configuration, and mandatory publication of losses are acceptance
criteria, not afterthoughts. This RFC amends `docs/benchmarks.md` §1
(reference systems) and §7 (thesis-gate escalation); it does not touch
any `CLAUDE.md` §3 invariant or the Parquet schema.

## 2. Motivation

### 2.1 The thesis has only been tested against a strawman

`docs/benchmarks.md` §1 names exactly one reference system:
`zstdcat <file.zst> | grep <pattern>`. The B1 gate is "≥ 10× faster
than `zstdcat | grep`"; B2 is "scales with result size, not corpus
size." Both are real and both pass (§9.4, §9.8) — but both measure
the *mechanism*, not the *choice*. Parquet footer statistics do prune
row groups; the template count does converge. What no number in the
repository shows is that this beats the system a prospective user
would otherwise reach for. Loki also beats `zstdcat | grep`. The
question `CLAUDE.md` §1 raises — is there a reason to run Ourios
instead of Loki — is the project's existential question, and it is
unmeasured.

### 2.2 Why Loki, specifically

Of the three systems `CLAUDE.md` §1 names, Loki is the sharpest
comparison because it shares the *premise* and differs in the
*mechanism*. Both Ourios and Loki reject the full inverted index of
Elasticsearch/Quickwit; both store compressed log blocks on object
storage and lean on cheap storage plus selective reads. Where they
diverge is exactly the Ourios thesis:

- **Loki** indexes a small set of operator-chosen **labels** and,
  within the matching label streams, brute-force scans (greps)
  compressed chunks.
- **Ourios** mines a **template id** per line at ingest and leans on
  Parquet's per-row-group min/max statistics, bloom filters, and page
  indexes to skip chunks the query cannot match — *automatically*,
  without the operator choosing labels, and at a granularity finer
  than a label stream.

The comparison therefore tests the precise claim in `CLAUDE.md` §2
pillar #1–#2: that automatic template mining + Parquet pruning skips
more data than label-index + chunk scan on the selective queries that
dominate real log investigation. ClickHouse (general-purpose columnar)
and Quickwit (full-text index) are different enough in philosophy that
comparing to them answers a different question; they are noted in §4
and deferred.

### 2.3 OTLP is where the gap is widest, and OTel names the axes

The comparison runs on real OTLP logs (§3.3) because that is the
workload Ourios exists for, and because OTLP structure is exactly where
the two mechanisms diverge hardest. Critically, the query taxonomy is
not invented here: the OpenTelemetry Logs specification's [Log
Correlation](https://opentelemetry.io/docs/specs/otel/logs/#log-correlation)
section names the dimensions along which logs are navigated, filtered,
queried and analysed — *"these correlations can be the foundation of
powerful navigational, filtering, querying and analytical
capabilities"* — and they are precisely the axes Ourios prunes on:

- **Time of execution** — every query is time-bounded; Parquet
  row-group time statistics prune it.
- **Execution (trace) context** — `trace_id` / `span_id` on the
  LogRecord. The spec calls this out as what *"would make logs
  significantly more valuable in distributed systems"*: it directly
  correlates logs with traces and correlates logs *across* the
  components that served one request.
- **Resource context** — `service.name`, `k8s.*`, and other resource
  attributes identifying the telemetry's origin.

An OTLP log record arrives with `severity_number`, these resource
attributes, log attributes, and the trace context. Ourios promotes
this structure into **queryable, statistics-bearing columns
automatically** (template id at ingest per RFC 0001; severity,
service, and configured attributes as Parquet columns per RFC 0022),
so per-row-group min/max and bloom filters prune on OTLP fields
*without the operator declaring anything*. Loki's model is the
inverse: an operator must hand-pick a small set of **low-cardinality**
labels, and everything else is brute-force chunk scan. Four
consequences follow, and they are the four must-win query classes
(§3.4):

- Where template mining fires, Ourios prunes on **template id** (L1).
- Where it does not (the OTel-Demo capture is heavily `NO_TEMPLATE` on
  some services — RFC 0023), Ourios still prunes on the **promoted
  resource/attribute columns** (severity, `service.name` — L2). The
  pruning thesis on native OTLP is therefore *template mining +
  attribute promotion*, a stronger and more honest framing than a
  synthetic well-templated corpus would show.
- `trace_id` is **high-cardinality by construction**, so it cannot be
  a Loki label without exploding Loki's index. Loki must brute-force
  scan to answer "show me every log line for this trace"; Ourios
  promotes `trace_id` to a bloom-filtered column and prunes to the
  handful of row groups that contain it (L3).
- Template mining yields **typed parameters**, so "how often does
  template *X* fire over time, grouped by extracted field *Y*" is a
  columnar `GROUP BY` for Ourios. This is a first-class OTLP operator
  workflow — the canonical [OTLP-log query
  set](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/exporter/clickhouseexporter/README.md)
  opens with a severity-count time series, and an OTel-native vendor
  doing Drain-style mining demonstrates *"a log-frequency alert
  filtering on the pattern and grouping by the product-id field …
  without any metrics, without an extra metric, without a regular
  expression"* ([OTel Night, Berlin
  2025](https://github.com/open-telemetry/sig-end-user/blob/main/video-transcripts/transcripts/20250507T184836Z-otel-night-berlin-v2025-05-leveraging-ai-for-opentelemetry-data.md)).
  Loki, holding unstructured chunks and no typed params, must scan and
  regex-and-count. This is the workload the template + params pillar
  exists to serve (L4).

### 2.4 Why measurement now

The engineering is substantially complete (RFCs 0001–0030 green or
beyond; both data paths shipped and gated). The marginal green RFC no
longer changes whether the project should exist; the comparative
number does. `docs/benchmarks.md` §7 already states the discipline:
*"The worst failure mode for a greenfield project is shipping
something whose central claim quietly fails on real data and then
papering over it with more implementation."* An unmeasured
existential comparison is that failure mode latent. This RFC converts
it into a gate — one that can be **lost**, and whose loss is a
pillar-level signal, not a tuning knob.

### 2.5 Why bytes-read is the primary metric, not latency

Ourios is a young implementation on top of DataFusion; Loki is a
mature engine with years of query-path optimisation. A naive
latency-only comparison confounds two very different claims —
*"Parquet pruning reads less data"* (an architectural claim, the
thesis) and *"our query engine is faster today"* (an implementation-
maturity claim, not the thesis). We isolate the thesis by making the
primary gate **bytes read from object storage per query**: the direct,
engine-independent measure of how much data each architecture must
touch to answer a query. Pruning is, definitionally, reading less.
Wall-clock latency (p50/p99) is reported alongside as the number an
operator actually feels, but a *latency* loss paired with a decisive
*bytes-read* win is interpreted as "sound architecture, young
implementation" — a roadmap signal — whereas a *bytes-read* loss on a
selective query is a thesis failure. This asymmetry is the honesty
core of the RFC and is fixed in §5, not left to interpretation after
the fact.

## 3. Proposed design

### 3.1 Shape

A new comparative bench workstream, layered on the RFC 0006 harness
and the RFC 0007 querier, that:

1. Ingests one fixed corpus into **both** systems over their native
   OTLP path (§3.3).
2. Runs a fixed **query taxonomy** (§3.4) against both, expressed once
   in the Ourios DSL and once in LogQL, asserting **result-set
   equivalence** (§3.5) before any timing is trusted.
3. Records the **`L`-gate** metrics (§3.6) into `docs/benchmarks.md`
   §9 in the same diff-reviewable shape RFC 0006 established.
4. Ships the **exact Loki configuration** and orchestration in-repo so
   the comparison is third-party-reproducible (§3.7).

The Ourios-side numbers come from the existing querier and its OTel
query metrics (RFC 0016: `scanned` / `pruned` row-group counts, which
this RFC extends to bytes — §3.6). The Loki-side numbers come from
Loki's own query-statistics API (`Summary.totalBytesProcessed`,
`execTime`), which Loki returns per query — no instrumentation of
Loki's internals is required or permitted (that would be a fairness
hazard).

### 3.2 Infrastructure

Both systems run as containers under GitHub Actions (containerd; no
local Docker dependency), against the **same object-store backend** — a
single MinIO/localstack S3 endpoint — so the storage substrate is
byte-for-byte identical and cannot bias the bytes-read metric. Loki
runs in single-binary mode with the `tsdb` index and S3 chunk storage;
Ourios runs its normal ingester + querier against the same bucket. Per
the established norm (`benchmarks.md` §1, and the project's
bench-on-`ci-runner`-first discipline), the first comparative run is
**indicative on `ci-runner`**; the authoritative run is on the
`baseline-8vcpu-32gib` tag and is gated on maintainer opt-in. Neither
system is co-scheduled with the other during a timed query (they share
a bucket, not a CPU): ingest both, then quiesce, then query each in
isolation with the other stopped, to remove noisy-neighbour effects
from the latency numbers.

### 3.3 Corpus and ingest parity

The **headline** corpus is the **OTel-Demo v8 capture** — the canonical
real-OTel corpus per the project's corpus policy, native OTLP with the
full attribute/`trace_id` structure §2.3 turns on. This is the number
the project stands behind. It is worth stating plainly that the
OTel-Demo logs are comparatively *well-structured* (shipped over OTLP
as JSON with rich attributes); real-world Kubernetes logs are typically
messier raw-string bodies with only basic attributes (OTel Night 2025,
ibid.). So OTel-Demo is the honest OTLP headline but not a worst case —
the harder real case (mostly `NO_TEMPLATE`, sparse attributes) is
exactly where attribute promotion and the L4 aggregation carry the
thesis, and §7 keeps "a messier captured corpus" as a follow-up. **LogHub
HDFS_v1** (already wired, bench-time-fetched, ~1.47 GiB) is retained as
a **secondary, well-templated sanity floor** — it reproduces the best
case for template mining and anchors against the Drain-paper corpora —
but it is explicitly not the headline, and it is non-native (plain text
replayed as text-body OTLP), so it exercises template pruning without
the OTLP-attribute story.

Both corpora are fed to both systems as the *same* OTLP log stream: a
single replay driver emits OTLP/gRPC to Ourios's receiver and, in
parallel, to Loki over OTLP (native OTLP endpoint preferred, or an OTel
Collector `loki` exporter — §7), so neither system gets a preprocessing
advantage and both derive their structure from the identical OTLP
records. Label selection for Loki is part of the committed config
(§3.7) and must be a *competent operator's* choice (`service.name`,
`severity`, a small set of low-cardinality resource attributes), **not**
a single catch-all label that would force a full scan (an unfair
strawman in Ourios's favour), **nor** a high-cardinality label
(`trace_id`, or one label per template) that smuggles Ourios's promoted
columns into Loki's index and would blow it up in a real deployment (an
unfair strawman in Loki's favour, and not how anyone operates Loki).
The label set is frozen in the config and machine-checked as the §5
RFC0031.10 gate.

### 3.4 Query taxonomy

Seven query classes, each with a must-win / acknowledged-loss / floor /
parity disposition fixed up front so the result cannot be reframed after
it is known. The four must-win classes map one-to-one onto OTel's log
analysis axes (§2.3) and the four ways Ourios turns OTLP structure into
pruning:

| Class | Query | OTel axis / Ourios pruning mechanism | Disposition |
|-------|-------|--------------------------------------|-------------|
| **L1** | Template-exact lookup: all lines of one rare template over the full corpus | body pattern → template id (RFC 0001) | **must-win** (thesis) |
| **L2** | Attribute predicate: `severity ≥ ERROR AND service.name = X` over a bounded window | resource context → promoted columns + Parquet stats (RFC 0022) | **must-win** (thesis) |
| **L3** | Trace correlation: every log line for one `trace_id` | execution context → high-cardinality bloom column | **must-win** (thesis, OTLP-native) |
| **L4** | Frequency aggregation: count of a template over time, grouped by an extracted param | typed template params → columnar `GROUP BY` | **must-win** (thesis, OTLP-native) |
| **L5** | Substring needle: an arbitrary literal *not* captured by a template or a promoted column (embedded in a param) | none (brute scan for both) | **acknowledged** — loss permitted, published |
| **L6** | Broad scan: all lines in a wide time range, low predicate selectivity | little prunes | **floor** — bounded, not must-win |
| **L7** | Ingest throughput: sustained OTLP lines/s to steady state | — | **parity** — within a stated factor |

L1–L4 are where the pruning thesis lives and must win on bytes-read
(§3.6). L3 and L4 are the two Loki *structurally* cannot serve
efficiently: L3 because `trace_id` cannot be a label, L4 because Loki
holds no typed params and must scan-then-regex-and-count where Ourios
does a columnar aggregation. L5 is the honest inclusion: neither
template mining nor attribute promotion helps a substring the miner
folded into a parameter, and Loki's brute-force chunk grep may match or
beat Ourios there — we publish it. L6 tests the floor (when little can
be pruned, Ourios must not be *catastrophically* worse — bounded, not
required to win). L7 checks that thesis-side query wins are not bought
with an unacceptable ingest regression.

### 3.5 Result-set equivalence (the integrity gate)

A latency or bytes comparison between two queries that return different
answers is meaningless. For every query in the taxonomy, the harness
compares the two systems' answers **exactly** before any metric for that
query is recorded:

- For the line-returning classes (L1–L3, L5, L6) it extracts each
  system's matching lines keyed by `(timestamp_unix_nanos, body_bytes)`
  and compares as a **multiset** — the *count* of each key must match,
  not merely the set, so a system returning three identical duplicate
  lines where the other returns two is a mismatch, not a silent pass.
- For the aggregation class (L4) the grouped result itself is the answer:
  the `(bucket, group_key) → count` map must be identical between
  systems.

A mismatch fails the run (non-zero exit, no metric written for that
class) — it means the two queries are not asking the same question and
the comparison is invalid. This is RFC0031.1 and it gates every other
`L`-scenario.

### 3.6 Metrics and the bytes-read extension

Per query, per system, the harness records:

- **`bytes_read`** — bytes fetched from object storage to answer the
  query. Ourios: extended from the RFC 0016 `scanned`/`pruned`
  row-group counts to the **bytes** of the row groups actually read
  (footer + read row-group byte length), emitted on the existing OTel
  query-metrics path. Loki: `totalBytesProcessed` from the query
  `Summary`. **Primary gate metric.** Because it counts bytes fetched
  from the shared object store, it is by construction insensitive to
  CPU speed and engine maturity; to keep it insensitive to local page
  cache as well, each measured query runs against a **freshly started**
  server with OS page cache dropped, so a warm local cache cannot mask
  an architecture that would fetch more from storage.
- **`latency_p50` / `latency_p99`** — wall-clock over N repetitions,
  reported for both a **cold** reading (fresh process, dropped cache —
  the same state the bytes-read gate is measured in) and a **warm**
  reading (repeated in-process), stated separately. Corroborating, not
  sole-gating (§2.5).
- **`storage_footprint`** — total bytes each system persists for the
  corpus on the shared bucket. Recorded **diagnostic** (like A1, per
  RFC 0011 — a byte codec captures redundancy the thesis does not
  claim to beat); not gating.
- **`ingest_throughput`** — steady-state OTLP lines/s (L7 only).
- **`peak_rss`** — high-water memory of each system's query path,
  diagnostic.

### 3.7 Reproducibility and anti-strawman commitment

The entire comparison — Loki config (index, chunk, retention, S3,
label selection), the OTLP-into-Loki config (native endpoint or an OTel
Collector `loki` exporter — §7), the query pairs (DSL ↔ LogQL), and the
orchestration — is committed under `bench/comparative/` and runnable by
a third party with one command. The Loki configuration must be a
**good-faith competent** deployment: tuned chunk target size,
appropriate index period, the label set from §3.3. The config carries a
header comment inviting challenge, and the `L`-gate results in
`benchmarks.md` §9 link the exact config commit. Crucially the label
set is **machine-checked**, not merely eyeballed (RFC0031.10): a test
asserts the committed labels are drawn from a declared low-cardinality
allowlist and that the disallowed keys (`trace_id`, `span_id`, and any
per-template id) are absent, so a strawman config cannot slip in
unnoticed. A benchmark whose loser's configuration cannot be inspected
and re-run is not evidence; this section is what makes the number
defensible rather than a claim.

### 3.8 `benchmarks.md` amendments

- **§1** gains Loki as a second reference system, described as above.
- **§7** gains the `L`-gate escalation: an L1, L2, L3, or L4
  **bytes-read** loss on the headline OTel-Demo corpus is a
  **pillar-level** finding (revisit `CLAUDE.md` §2 before further
  implementation), exactly as two failing thesis-gates are today. A
  must-win *latency* loss with a bytes-read win is a roadmap item, not
  an escalation. L5 (substring) loss is expected and never escalates.
  L6 beyond its floor, or an L7 regression past its factor, is a tuning
  RFC.

## 4. Alternatives considered

**Compare against ClickHouse instead of Loki.** ClickHouse is the
closest system *architecturally* (columnar, statistics-based skipping),
so a ClickHouse comparison would test "did we build a worse
ClickHouse" rather than "should you use Ourios over the log-native
incumbent." It is the more flattering comparison to defer and the more
dangerous one to skip; it belongs in a follow-up RFC once the Loki
number exists, because losing to ClickHouse-on-logs is a distinct and
also-existential finding. Deferred, not dismissed.

**Compare against Quickwit / Elasticsearch.** These carry a full-text
inverted index — the exact structure `CLAUDE.md` §2 claims to collapse.
They will win outright on arbitrary substring search (L5-like queries)
and pay for it in storage and ingest. That trade is already understood
and is not the question Ourios's thesis stakes itself on; benchmarking
it measures a different product. Out of scope (`benchmarks.md` §8
already excludes SIEM-style full-text latency).

**Keep `zstdcat | grep` as the only reference.** This is the status
quo and it is insufficient for the reason in §2.1: it validates the
mechanism, not the choice. Retained as a floor, not removed.

**Latency as the primary gate.** Rejected in §2.5: it confounds the
architectural thesis with implementation maturity and would let a
young-engine latency loss read as a thesis failure (or, worse, tempt us
to chase engine micro-optimisation to rescue a number that the
architecture already wins on bytes). Bytes-read is the honest primary.

**No result-set equivalence check — just run "the same query" in each
DSL.** Rejected: LogQL and the Ourios DSL have different matching
semantics (label streams vs template ids vs substrings), and "looks
equivalent" is exactly how comparative benchmarks lie. §3.5 makes
multiset-exact equivalence a hard precondition.

**Make it an RFC 0006 amendment rather than a new RFC.** RFC 0006 pins
the *self-referential* thesis-gate methodology; this introduces a
second system, an equivalence harness, and a fairness contract — enough
new surface, and enough new failure modes, to warrant its own decision
record. It references RFC 0006's harness rather than editing it.

## 5. Acceptance criteria

> **Scenario RFC0031.1 — Result-set equivalence gates every comparison**
> - **Given** a query from the §3.4 taxonomy expressed as an Ourios-DSL
>   / LogQL pair, and the fixed corpus ingested into both systems
> - **When** the harness executes both queries
> - **Then** for a line-returning class it extracts each system's
>   matching lines keyed by `(timestamp_unix_nanos, body_bytes)` and
>   asserts the two **multisets** are identical (per-key counts equal,
>   so duplicates are not silently collapsed); for the L4 aggregation
>   class it asserts the `(bucket, group_key) → count` maps are
>   identical
> - **And** if the answers differ, the harness records no `L`-metric for
>   that class, writes the symmetric-difference (or count-delta) summary
>   and up to N example keys to stderr, and exits non-zero
> - **And** no `benchmarks.md` §9 row is written for a class whose
>   equivalence check did not pass

> **Scenario RFC0031.2 — L1 selective template lookup wins on bytes read**
> - **Given** the **headline OTel-Demo corpus** ingested into both
>   systems and a template that matches `< 0.1%` of corpus lines
> - **When** the harness runs the L1 query against each and reads
>   `bytes_read` (Ourios: row-group bytes actually read per the RFC 0016
>   metric extension; Loki: `Summary.totalBytesProcessed`)
> - **Then** `ourios.bytes_read / loki.bytes_read ≤ 1 / M_L1` where
>   `M_L1` is the committed must-win margin (§7)
> - **And** the class disposition in the results is `must-win`, so a
>   result above the ratio flips `l1.pass = false` and is surfaced as a
>   pillar-level finding per `benchmarks.md` §7 (amended)
> - **And** `latency_p50`, `latency_p99` (cold and warm) are recorded
>   for both systems as corroborating, non-gating numbers

> **Scenario RFC0031.3 — L2 attribute predicate wins on bytes read**
> - **Given** the headline corpus ingested into both systems and the L2
>   predicate (`severity ≥ ERROR AND service.name = X` over a bounded
>   window) expressed equivalently in both DSLs, equivalence per
>   RFC0031.1 holding
> - **When** the harness runs L2 against each
> - **Then** `ourios.bytes_read / loki.bytes_read ≤ 1 / M_L2`
> - **And** the same pillar-level escalation as RFC0031.2 applies on
>   failure

> **Scenario RFC0031.4 — L3 trace correlation wins on bytes read (OTLP-native)**
> - **Given** the headline corpus ingested into both systems and a
>   `trace_id` present in it, with `trace_id` **not** a Loki label (per
>   the §3.3 frozen set — high-cardinality and un-labelable), equivalence
>   per RFC0031.1 holding
> - **When** the harness runs "every log line for this `trace_id`"
>   against each (Ourios: bloom-filtered promoted column; Loki:
>   label-stream scan)
> - **Then** `ourios.bytes_read / loki.bytes_read ≤ 1 / M_L3`
> - **And** the class disposition is `must-win` with the same
>   pillar-level escalation as RFC0031.2 on failure — this is a query
>   Loki's model cannot answer without a full scan (§2.3), so a loss
>   here is among the strongest possible signals against the thesis

> **Scenario RFC0031.5 — L4 frequency aggregation wins on bytes read (OTLP-native)**
> - **Given** the headline corpus ingested into both systems and a
>   frequency-aggregation query — count of one template over time,
>   grouped by an extracted param (Ourios: columnar `GROUP BY` on
>   `template_id` + a typed param column; Loki: `count_over_time` with a
>   LogQL pattern/`label_format` extraction over scanned chunks) —
>   equivalence per RFC0031.1 (the grouped-count maps) holding
> - **When** the harness runs L4 against each
> - **Then** `ourios.bytes_read / loki.bytes_read ≤ 1 / M_L4`
> - **And** the class disposition is `must-win` with the same
>   pillar-level escalation as RFC0031.2 on failure — this is the query
>   the template + typed-params pillar exists to serve (§2.3)

> **Scenario RFC0031.6 — L5 substring needle is measured and published, loss permitted**
> - **Given** an L5 query for a literal not captured by a template or a
>   promoted column (embedded in a param, so nothing prunes it),
>   equivalence per RFC0031.1 holding
> - **When** the harness runs L5 against each
> - **Then** both systems' `bytes_read` and latency are recorded with
>   class disposition `acknowledged`
> - **And** the run passes irrespective of which system wins — an
>   Ourios loss here does **not** fail the run and does **not** escalate,
>   but it **must** appear in the published `benchmarks.md` §9 table (a
>   suppressed L5 loss is a process violation)

> **Scenario RFC0031.7 — L6 broad scan stays within the floor**
> - **Given** an L6 low-selectivity wide-time-range query, equivalence
>   holding
> - **When** the harness runs L6 against each
> - **Then** `ourios.latency_p50 ≤ F_L6 × loki.latency_p50` where
>   `F_L6` is the committed floor factor (§7)
> - **And** exceeding the floor is a tuning-RFC signal, not a
>   pillar-level escalation

> **Scenario RFC0031.8 — L7 ingest throughput parity within a stated factor**
> - **Given** the OTLP replay driver feeding both systems to steady
>   state on the same hardware
> - **When** the harness measures sustained lines/s for each
> - **Then** `ourios.ingest_throughput ≥ loki.ingest_throughput / F_L7`
>   where `F_L7` is the committed parity factor (§7)
> - **And** the WAL-before-ack invariant (`CLAUDE.md` §3.4) is not
>   relaxed to obtain the number — Ourios's throughput is measured with
>   durable acks, and the config proving it is recorded

> **Scenario RFC0031.9 — Storage footprint is recorded as a diagnostic, not a gate**
> - **Given** both systems having ingested the full corpus into the
>   shared bucket
> - **When** the harness sums each system's persisted bytes
> - **Then** both `storage_footprint` values and their ratio are
>   written to `benchmarks.md` §9 as a **diagnostic** row
> - **And** no pass/fail is derived from it (parity with A1's RFC 0011
>   demotion — a byte codec captures redundancy the thesis does not
>   claim on disk)

> **Scenario RFC0031.10 — The Loki configuration is committed, competent, and machine-checked**
> - **Given** the comparative workstream under `bench/comparative/`
> - **When** a third party checks out the repo
> - **Then** the exact Loki config (index, chunk target size, S3
>   backend, retention, and the frozen label set), the OTLP-into-Loki
>   config, and the DSL↔LogQL query pairs are present and the whole
>   comparison runs with a single documented command
> - **And** a test asserts the label set is drawn from a declared
>   low-cardinality allowlist and that `trace_id`, `span_id`, and any
>   per-template id are **absent** — so neither a single catch-all label
>   (forcing Loki into a full scan) nor a high-cardinality label
>   (smuggling Ourios's promoted columns into Loki's index) can slip in;
>   the config header states this and invites challenge
> - **And** each `L`-gate row in `benchmarks.md` §9 links the config
>   commit used to produce it

> **Scenario RFC0031.11 — Losses are published and escalation follows benchmarks.md §7**
> - **Given** a completed comparative run
> - **When** results are written to `benchmarks.md` §9
> - **Then** every class in the taxonomy appears — wins and losses —
>   with its disposition, both systems' numbers, the corpus, and the
>   hardware tag
> - **And** an L1, L2, L3, or L4 bytes-read loss on the headline
>   OTel-Demo corpus is recorded as a **pillar-level** finding that
>   pauses further implementation pending a `CLAUDE.md` §2 revisit (the
>   §7 amendment), whereas a must-win *latency-only* loss with a
>   bytes-read win is recorded as a roadmap item

## 6. Testing strategy

Per `CLAUDE.md` §6.2, mapped to the §5 scenario ids:

- **Equivalence harness (RFC0031.1)** — an integration test over a
  small committed fixture corpus (not the full OTel-Demo/HDFS fetch)
  that runs a DSL↔LogQL pair against a containerised Loki and the
  in-process querier and asserts multiset-equality of the keyed line
  sets (and grouped-count maps for L4); a deliberately mismatched pair,
  and a duplicate-count mismatch, both assert the non-zero-exit /
  no-write path.
- **`L`-gate computation (RFC0031.2–RFC0031.9)** — unit tests over
  recorded/synthetic per-query metric inputs assert the ratio math, the
  pass/fail dispositions, and the diagnostic-vs-gating distinction
  (mirroring RFC 0006's `a1`/`c2` gate-math unit tests). The margins
  `M_L1`, `M_L2`, `M_L3`, `M_L4`, `F_L6`, `F_L7` are configuration, so a
  calibration test pins their wiring, not their values.
- **Bytes-read metric extension (RFC0031.2–.5)** — a querier test
  asserts the new bytes-read figure equals the summed byte length of
  the row groups the RFC 0016 path reports as `scanned` (and excludes
  `pruned`), so the primary gate metric is verified against the
  existing pruning counters rather than trusted.
- **Config machine-check (RFC0031.10)** — a test parses the committed
  Loki + OTLP-path configs, asserts the label allowlist / disallowlist
  property, and asserts the documented one-command entry point exists
  and references them.
- **Full comparative run (RFC0031.11)** — a `workflow_dispatch` job
  (indicative on `ci-runner` first, authoritative on
  `baseline-8vcpu-32gib` on opt-in) ingests the OTel-Demo capture (the
  headline) and HDFS_v1 (the secondary floor), runs the taxonomy end to
  end, and appends the §9 table. Not a per-PR gate (it fetches large
  corpora and runs two systems); it is the RFC-`validated` step,
  consistent with `benchmarks.md`'s authoritative-run cadence.

Validation (`benchmarks.md` §7): RFC 0031 reaches `validated` when the
authoritative comparative run has been recorded in §9 with L1, L2, L3,
and L4 passing on the headline OTel-Demo corpus. A must-win failure does
not block `validated` in the "we didn't finish" sense — it is a
*result*, and per §5 RFC0031.11 a pillar-level one.

## 7. Open questions

- [ ] **Must-win margins `M_L1`, `M_L2`, `M_L3`, `M_L4`.** What
  bytes-read ratio counts as "decisively wins"? Proposed starting point:
  `M ≥ 10` (Ourios reads ≤ 1/10 the bytes Loki does), mirroring B1's 10×
  framing but on the honest metric; the two Loki-can't-index classes
  (L3 trace, L4 aggregation) may warrant larger margins. Needs a
  calibration pass on a first indicative run before frozen. **Maintainer
  call.**
- [ ] **Floor / parity factors `F_L6`, `F_L7`.** How much slower on the
  broad scan (L6) and how much lower on ingest (L7) is tolerable before
  it is a finding? Proposed `F_L6 = 3`, `F_L7 = 2`, pending the first
  run.
- [ ] **L4 aggregation query shape.** Which template + param + bucket
  width best represents the real alerting/dashboard workload on the
  OTel-Demo corpus, and how is the LogQL equivalent (pattern/`label_format`
  extraction + `count_over_time … by`) pinned so RFC0031.1 equivalence
  is achievable? Confirm against LogQL's current metric-query surface at
  implementation time.
- [x] **Headline corpus — DECIDED: OTel-Demo.** Ourios is an OTLP-native
  backend, so the honest headline is real OTLP logs — the workload the
  project claims to do best — not the favourable well-templated HDFS_v1.
  HDFS_v1 is retained only as a secondary well-templated sanity floor
  (§3.3). A messier real-world captured corpus (sparse-attribute k8s
  text) is a worthwhile follow-up but not required for the first result.
  (Maintainer decision, 2026-07-11.)
- [ ] **Loki index backend.** `tsdb` (current Loki default) vs
  `boltdb-shipper`. Pick the one a competent 2026 operator would deploy;
  likely `tsdb`. Confirm against Loki's current guidance at
  implementation time.
- [ ] **OTLP → Loki path.** Loki's native OTLP endpoint vs an OTel
  Collector with the `loki` exporter. Native OTLP is the fairer
  apples-to-apples (both consume OTLP directly); confirm label derivation
  is equivalent to the frozen set either way.
- [ ] **New crate vs `ourios-bench` extension.** Does the comparative
  driver + equivalence harness live in `ourios-bench` or a new
  `bench/comparative/` (non-crate) harness plus a small querier-side
  metric addition? A new crate is a `CLAUDE.md` §7 commitment; a harness
  under `bench/` is not. Leaning `bench/` + a querier metric extension.
  **Maintainer call.**
- [ ] **Does this touch `docs/hazards.md`?** The comparison itself adds
  no runtime hazard, but the bytes-read metric extension touches the
  RFC 0016 query-metrics path; confirm no regression to those counters.

## 8. References

- `CLAUDE.md` §1 (the existence test — "just use $X"), §2 (pillars #1
  Parquet pruning, #2 template mining), §3.4 (WAL-before-ack, held in
  L7), §7 (new-crate commitment, open question).
- `docs/benchmarks.md` §1 (reference systems — amended), §7 (thesis-gate
  escalation — amended), §8 (out-of-scope: full-text latency), §9
  (results shape).
- **RFC 0006** — bench harness (the self-referential thesis-gate
  methodology this extends; A1/C1/C2 gate-math test pattern reused).
- **RFC 0007** — querier (provides the query path measured here).
- **RFC 0010** — audit-stream / drift queries (template-frequency
  aggregation precedent the L4 gate builds on).
- **RFC 0011** — A1 demotion to diagnostic (precedent for the
  storage-footprint diagnostic disposition, RFC0031.9).
- **RFC 0016** — query-serving endpoint and OTel query metrics
  (`scanned`/`pruned` counts, extended here to bytes-read).
- **RFC 0022** — promoted attribute columns (the resource-context
  pruning L2 exercises).
- **RFC 0023** — bounded template memory (the `NO_TEMPLATE` fraction on
  heterogeneous corpora that makes the OTel-Demo corpus the honest hard
  case).
- OpenTelemetry Logs specification, [Log
  Correlation](https://opentelemetry.io/docs/specs/otel/logs/#log-correlation)
  (time / execution-context / resource-context correlation — the axes
  the §3.4 must-win taxonomy is anchored to).
- Canonical OTLP-log query patterns:
  [clickhouseexporter](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/exporter/clickhouseexporter/README.md)
  (severity-count time series, service/attribute filters, substring,
  trace-id skip index) — the query classes a real OTLP log backend serves.
- OTel Night Berlin 2025, [Leveraging AI for OpenTelemetry
  data](https://github.com/open-telemetry/sig-end-user/blob/main/video-transcripts/transcripts/20250507T184836Z-otel-night-berlin-v2025-05-leveraging-ai-for-opentelemetry-data.md)
  (an OTel-native vendor doing Drain-style template mining on OTLP logs;
  the template-frequency-alert workload the L4 gate models; the "real k8s
  logs are messier than OTel-Demo" caveat).
- Grafana Loki — architecture (label index + chunk store) and the query
  `Summary` statistics (`totalBytesProcessed`, `execTime`) used for the
  Loki-side numbers.
- Jieming Zhu et al., *Loghub: A Large Collection of System Log
  Datasets for AI-driven Log Analytics*, ISSRE 2023 (HDFS_v1 corpus;
  license notice in `benchmarks.md` §1).
