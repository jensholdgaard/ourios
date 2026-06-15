# Roadmap to MVP

> Living document. Refreshed at phase boundaries (§4) and whenever
> a merged PR materially changes the *current state* in §3.
> Last updated: **2026-06-14** — RFC 0001, RFC 0008, and RFC 0011 flipped
> to `accepted` (maintainer sign-off). RFC 0001 reached `validated` first
> (C1/C2 pass authoritatively on the `benchmarks.md` §1 baseline hardware,
> §9.6; A1 is diagnostic per RFC 0011); RFC 0008's `validated` is vacuous
> (no thesis gate); RFC 0011 is a tuning RFC. The §§4+ phase narrative
> below predates this and is not re-verified here
> (PR #41 RFC 0005, then PR-D through PR-G landed
> `ourios-parquet` end-to-end: schemas, writer, reader, audit
> stream). The deferred-capabilities table in §5 is unchanged:
> WAL durability and the OTLP wire endpoints stay post-MVP.

This document answers two questions in one place: *what does
"MVP" mean for Ourios*, and *how far are we from it*. The
artifact is parallel to [`hazards.md`](./hazards.md) and
[`benchmarks.md`](./benchmarks.md): hazards say what we mustn't
break, benchmarks say what success looks like, and this file
says how we get from here to there.

---

## 1. What "MVP" means here

**MVP for Ourios is thesis-proving, not production-ready.**

The thesis (`CLAUDE.md` §2) claims that Parquet + Drain-derived
template mining + DataFusion collapses the inverted index, the
compression layer, the storage tier, and the query engine into
one stack of off-the-shelf parts plus thin glue. That claim is
falsifiable. The MVP is the smallest stack that lets us run the
**thesis-gate benchmarks** in [`benchmarks.md`](./benchmarks.md)
on a real corpus and either confirm the claim or kill it.

Production-shape concerns — gRPC OTLP receiver, WAL durability,
snapshot mechanism, Helm chart, the full §6.8 telemetry surface,
the RFC 0002 query DSL — are deliberately **out of MVP scope**
(§5). Each is a real shipping concern, but none of them changes
the answer to "does the thesis hold." We defer to keep the
critical path as short and honest as possible.

---

## 2. The MVP gate: thesis benchmarks

**Four** gating `[THESIS]` goals in
[`benchmarks.md`](./benchmarks.md) define MVP-done. Hitting all
four on a representative corpus means the thesis holds; missing
any of them means a pillar (`CLAUDE.md` §2) is wrong and a PR
won't fix it — an RFC will.

| Gate | What it measures | Why it matters |
|---|---|---|
| **B1** | Predicate-pushdown query latency on time/template/tenant filters | Pillar 1 (footer reads + min/max stats skip row groups) actually skips |
| **B2** | Template-exact query latency (`where template_id = X`) | Pillar 2's `template_id` column is a usable index, not a curiosity |
| **C1** | Bit-identical reconstruction rate over the corpus | The hardest invariant (`CLAUDE.md` §3.3) holds in practice, not just in unit tests |
| **C2** | Template-count convergence (Drain finds a small, stable number of templates) | Pillar 2 (template mining) extracts the structure we believed was there |

**A1** (end-to-end compression vs. zstd-alone) *was* a fifth gating
goal, but **RFC 0011 (`accepted`) demoted it to a recorded
diagnostic**: it is refuted on every corpus class — including the
maximally-templated one — for structural reasons (the more templated a
corpus, the more a whole-stream byte codec captures the same
redundancy), so template mining's compression value is *logical* /
query-pruning, captured by B1/B2, not on-disk bytes vs a codec. A1 is
still measured and recorded (`benchmarks.md` §7/§9 — the columnar
queryability premium + a codec-regression guard) but **does not block
MVP-done or any RFC's `validated`**.

`A2`, `B3`, `C3`, `C4`, `D*`, `E*` in `benchmarks.md` are
relevant but not MVP-blocking — they're tuning goals, honesty
goals, or post-MVP shipping concerns.

---

## 3. Current state (as of 2026-06-14)

**The thesis is proven on representative corpora.** All four gating
thesis-gates pass authoritatively on the `benchmarks.md` §1 baseline
hardware (the §9.4 / §9.6 runs), so the MVP thesis-proving bar (§2) is met:

| Gate | Result | Source |
|---|---|---|
| **B1** predicate-pushdown | **PASS** — 34.2× / 25.4× vs `zstdcat \| grep` at ~1 GB, exact row-count agreement | §9.4 |
| **B2** template-exact | **PASS** — windowed latency flat across 0.57→1.04 GB; flat on HDFS_v1 (11.2 M rows, 1/14 row groups) | §9.4 |
| **C1** reconstruction | **PASS** — `1.000000` on HDFS_v1 (11.2 M lines, authoritative) | §9.6 |
| **C2** template convergence | **PASS** — 40-template plateau, sub-linear, formal gate applies | §9.6 |

**A1** (compression vs zstd) *fails*, but RFC 0011 (`accepted`)
reclassified it a recorded **diagnostic**, not a gate: the failure is
structural and template mining's value is logical / query-pruning,
captured by B1/B2 (see `benchmarks.md` §2 / §7).

**RFC ladder status:**

| RFC | Area | Status |
|---|---|---|
| 0001 | Template miner | **`accepted`** |
| 0002 | Query DSL | `green` |
| 0003 | OTLP receiver (gRPC + HTTP) | `green` |
| 0004 | Configuration policy | `green` |
| 0005 | Parquet storage | **`green`** — all 14 §5 scenarios pass; RFC0005.6 row-group sizing is the `#[ignore]`d `tests/sizing.rs` (manual `cargo test --ignored`, not CI-gated per §7) |
| 0006 | Bench harness | `green` |
| 0007 | Querier (DataFusion + logs DSL) | **`validated`** |
| 0008 | WAL | **`accepted`** |
| 0009 | Background compaction | **`green`** — §5 RFC0009.1–.6 pass (manifest + `gc_orphans` + sweep; #206/#207/#208/#209); .7 (D2/D3 benches) deferred to `validated` |
| 0010 | Audit-stream / drift queries | `specified` — the `drift` surface is implemented; general audit aggregation deferred |
| 0011 | A1 re-scope | **`accepted`** |

**Crates — all ten product crates are implemented** (`ourios-core`,
`-miner`, `-wal`, `-parquet`, `-ingester`, `-querier`, `-server`,
`-bench`, `-semconv`, `-telemetry`):

- **`ourios-miner`** — the Drain-derived miner, RFC 0001 `accepted`:
  `(severity, scope)` keying, three-zone confidence, widening +
  type-expansion with audit events, 256 B param-overflow spill,
  bit-identical reconstruction + the H7.3 render contract, structured-body
  canonical encoding, and §6.9 snapshot + v2 restore. Zero
  `#[ignore]`/`todo!()` acceptance stubs.
- **`ourios-wal`** — RFC 0008 `accepted`: append/sync, crash recovery (the
  real-SIGKILL CI gate), snapshot-restore, segment rotation, group-commit
  batched fsync, checkpoint-driven truncation; §5 arms .1–.10 green.
- **`ourios-parquet`** — RFC 0005 §3: atomic-publish writer + reader with
  the §3.9 compat contract, the §3.7 audit-event series, and the §3.6
  encoding policy (dict + page index + `template_id` bloom filter).
- **`ourios-ingester`** — RFC 0003 `green`: the OTLP gRPC + HTTP receiver
  with WAL-before-ack, per-`ResourceLogs` tenant derivation, the windowed
  group-commit coordinator, and the startup recovery driver; also hosts
  the RFC 0009 compaction runner.
- **`ourios-querier`** — RFC 0007 `validated` / RFC 0002 `green`: the logs
  DSL over DataFusion with predicate + partition (time-window) pruning,
  alias resolution, and the RFC 0010 drift query.
- **`ourios-bench`** — RFC 0006 `green`: drives the A1/B1/B2/C1/C2
  measurements over OTLP-Demo + LogHub corpora and records results to
  `benchmarks.md` §9.
- **`ourios-core`** / **`-semconv`** / **`-telemetry`** / **`-server`** —
  shared types + tenancy + record/audit shapes; the weaver-generated OTel
  name constants; the OTel metrics/export surface; the two-role binary.

The full `cargo test --all-features` suite is green in CI — the `cargo
test` job gates every PR on the exact head; the coverage job runs
alongside it but is informational (`continue-on-error`), not gating.

**What remains** is post-MVP shipping shape (§5 — Helm chart, the
production deployment surface) plus the open RFCs above: RFC 0005 →
`green`, RFC 0009 → `validated` (the D2/D3 compaction benches), and the
items tracked in the RFCs' §7/§9 open-questions.

---

## 4. Path to MVP — three phases

Phase scope only; per-PR breakdown lives in the planning that
opens each phase, not in this doc, so the file stays stable as
mid-stream design decisions land.

### Phase 1 — Finish the miner

**Goal:** the miner mines, audits, retains bodies, reconstructs.
By the end of this phase the miner self-contained covers RFC
0001 §6.2 / §6.3 / §6.4 / §6.5 / §6.6 end-to-end and most §5
scenarios are green.

**Capabilities to land:**

- Drain tree (root → length-N nodes → prefix nodes → leaves)
  with `descend`.
- Best-candidate selection in `MinerCluster::ingest` via
  `sim_seq` (replaces the exact-match `HashMap` placeholder).
- `widen` step + `template_widened` audit emission +
  type-expansion + `template_type_expanded` audit + degenerate-
  template guard.
- Three-zone confidence branching (clean / lossy / parse-failure)
  + body retention in the lossy zone.
- Separators preservation through the ingest pipeline +
  `reconstruct()` + `lossy_flag` semantics per §6.6.
- Per-parameter byte-limit check + `OVERFLOW` marker + forced
  body retention.
- **`MinerCluster::ingest` consumes a structured `OtlpLogRecord`**
  (per RFC 0001 §6.1 as amended), not a raw `&str`. The
  `body_kind = String` / `body_kind = Structured` fork lands
  with the §6.2 algorithm rewrite (a follow-on PR to the §6.1
  amendment). Severity, scope, and the OTLP-canonical JSON
  encoding for structured bodies all flow through the miner from
  this phase forward.

**Unblocks:** thesis gates **C1** (reconstruction) and **C2**
(template-count convergence). RFC 0001 §5 scenarios H1.\*,
H2.\*, H5.\*, H7.\*, §3.3.1, RFC0001.\* should mostly flip in
this phase.

### Phase 2 — Records to Parquet

**Goal:** mined records become Parquet files. By the end of this
phase a corpus run produces on-disk Parquet that any
DataFusion-aware reader can open.

**Capabilities to land:**

- New crate `ourios-parquet`.
- Record schema matching the amended RFC 0001 §6.1: identity +
  partitioning columns, the OTLP-derived columns (`time_unix_nano`,
  `severity_number` + `severity_text`, `scope_name` +
  `scope_version`, `attributes`, `resource_attributes`,
  `trace_id` + `span_id` + `flags`, `event_name`,
  `dropped_attributes_count`), and the body / miner-derived
  columns (`body_kind`, `body?`, `params`, `separators`,
  `confidence`, `lossy_flag`).
- Writer: record batch → Parquet file (with row-group sizing
  from `hazards.md` H4 — target 128 MB–1 GB row groups).
- Reader: Parquet file → record batch (for verification + the
  Phase 3 DataFusion path).
- Audit-event Parquet stream (the contract called out in RFC
  0001 §9 *"Cross-RFC contracts pending"*).

**Unblocks:** thesis gate **A1** (compression ratio). The Parquet
column codec earns its share of the 50–200× headline only once
records actually land on disk in this format.

**Out of MVP scope, parked here:** background compaction
(small-file problem, `hazards.md` H4) — corpus runs are bounded,
a single Parquet file per phase is acceptable; production
compaction is a post-MVP PR.

### Phase 3 — DataFusion + bench

**Goal:** the thesis-gate benchmarks run.

**Capabilities to land:**

- New crate `ourios-querier` — register the Phase 2 Parquet
  files with DataFusion and accept raw SQL. **No DSL** — RFC
  0002's surface is a post-MVP concern; the bench can use SQL
  directly.
- New crate `ourios-bench` — corpus runner that reads
  pre-recorded OTLP `LogsData` test data into a stream of
  `OtlpLogRecord`s, hands them to the miner, writes Parquet,
  runs the A1/B1/B2/C1/C2 measurements, and reports numbers
  that go into `benchmarks.md` §9 (Status). **No network
  receiver** in MVP — the bench reads OTLP from disk, not from
  a gRPC/HTTP listener (those stay post-MVP per §5).
- `testdata/corpus/` — anonymised real-log corpus committed to
  the repo (or a download script if size demands), serialised
  as OTLP `LogsData` (canonical JSON or protobuf) so the bench
  exercises the same record shape an OTel deployment would
  produce.

**Unblocks:** thesis gates **B1** (predicate-pushdown latency)
and **B2** (template-exact latency). At the end of this phase,
`benchmarks.md` §7 (the thesis-gate summary) has measured
numbers for every `[THESIS]` row, and either the thesis holds
or it doesn't.

---

## 5. Deliberately out of MVP

Each item is a real production concern. The reason it's deferred
is *"answering 'does the thesis hold?' doesn't require it,"* not
*"we don't think it matters."*

| Capability | Why deferred for MVP | When it lands |
|---|---|---|
| **Write-ahead log** (`ourios-wal`) | Corpus replay is bounded and reproducible; durability is irrelevant for thesis-proving | First post-MVP shipping PR series — required before any non-corpus traffic |
| **OTLP wire endpoints** (gRPC + HTTP listeners) | Bench reads OTLP from disk, not the network — see Phase 3. The wire-decode layer (`tonic`, `axum`, `opentelemetry-proto`) is independent of the record shape and adds no signal to thesis gates | First post-MVP shipping PR series — paired with WAL since both gate non-corpus ingest. RFC 0003 (forthcoming) specifies the wire-decode design |
| **Snapshot mechanism** (RFC 0001 §6.9) | Corpus runs from cold start; replay budget moot | After WAL — snapshots are an optimisation on top of WAL replay |
| **Full §6.8 telemetry surface** | One or two metrics suffice for the bench; the §3.1.2 mandatory set is a production observability concern | After Phase 1 finishes — the metrics depend on the miner's hot path being final. Implementation note (maintainer direction, 2026-05-19, **updated 2026-06-03**): instrument through the **OpenTelemetry metrics API** (meters create the instruments) and export the resulting metrics through the OTel SDK's **OTLP metric exporter** (push), *not* the legacy `prometheus` client crate and *not* a `/metrics` scrape endpoint — any Prometheus compatibility is a downstream collector concern, keeping the project one metric-model end-to-end. The RFC 0001 §6.8 architecture amendment (2026-06-03) reframes the export model and terminology; the dotted-semconv name redesign (joining `semconv/registry/`, RFC 0009 §3.6) is a tracked follow-up |
| **Query DSL** (RFC 0002) | Raw SQL through DataFusion serves the bench; DSL is operator UX | Post-MVP — RFC 0002 already drafted but not specified |
| **Multi-tenancy at runtime** (rate limits, eviction, lifecycle) | Bench uses one tenant; the type is in place but no orchestration around it | Post-MVP, tied to operator-console RFC (see RFC 0001 §9 *"Multi-tenancy and operational lifecycle"*) |
| **`ourios-server` binary + Helm chart** | Bench is a binary in `ourios-bench`; full deployment shape is shipping concern | Post-MVP, sequencing TBD |
| **Perses dashboard integration** (datasource plugin + possible CRDs) | The data plane has to work first — a Perses plugin queries a query interface that doesn't exist yet. A native datasource plugin is small and downstream-friendly *once* RFC 0002 stabilises the query API; CRDs / operator (`PersesDashboard`-style declarative pipeline + miner config) would extend Ourios into managed-service territory, which contradicts `CLAUDE.md` §1's "Not a managed service" line. Splitting the concern: the plugin is an additive RFC against a stable query API; the CRDs/operator path is a charter change, not an RFC. Discussion captured 2026-05-18 (Grok prompt → maintainer review) | Plugin: after RFC 0002 lands, as `RFC 0010 — Perses datasource plugin`, scoped to plugin-only and living in a separate repo. CRDs/operator: requires a `meta:` RFC against `CLAUDE.md` §1 first, no commitment to land |

**Note on OTLP scope.** The pre-amendment roadmap listed
"OTLP receiver (gRPC + HTTP)" as a single post-MVP item.
PR #20 + #21 split that scope: the **OTLP record shape**
(`OtlpLogRecord` consumption, the canonical JSON encoding,
the OTLP-aligned Parquet schema) is in MVP — it's a
prerequisite for thesis-gate **C2**'s validity, because the
template-count convergence the corpus measures has to be over
records that look like real OTel traffic, not over
flat-text caricatures of it. Only the **wire endpoints** —
the actual gRPC/HTTP listeners that decode OTLP off the
network — remain post-MVP, and that's the row in the table
above.

---

## 6. Update cadence

This file refreshes:

- After every merged PR that materially changes §3 (current
  state) — the merging PR's author (or their drafting
  assistant) updates the table and the §5 scenario count.
- At phase boundaries (§4) — when Phase 1 finishes, §3's
  current state and §4's "blockers" tables are reconciled, and
  the next-phase opening planning PR is summarised here.
- When a thesis-gate result lands in `benchmarks.md` §9 — this
  doc gets a one-line note in §3 acknowledging the result.

The doc is intentionally *not* refreshed on every spec edit —
RFC patches and `hazards.md` edits don't change the road map
unless they change what MVP requires. If you find yourself
updating §3 every PR, the doc has become an activity log; the
fix is to be more selective, not to stop updating.
