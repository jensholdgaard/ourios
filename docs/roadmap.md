# Roadmap to MVP

> Living document. Refreshed at phase boundaries (§4) and whenever
> a merged PR materially changes the *current state* in §3.
> Last updated: **2026-09-27** — a status reconciliation, not a new
> milestone. §3's ladder is regenerated from every RFC's `status:`
> front matter, RFC 0001 through RFC 0057: the 2026-08-25
> completed-backlog batch flip moved most of the `green` rows to
> `accepted`, and RFC 0007 and RFC 0009 are `accepted`, not
> `validated`. Since the prior entry, tenancy moved out of band
> (RFC 0046, superseding RFC 0045) and gained graph-fed visibility and
> its operational surfaces (RFC 0047, RFC 0048); upstream-derived
> templates (RFC 0050) and the `ourios-serving` extraction (RFC 0051)
> landed; DataFusion 55 arrived (RFC 0021 phase 2a, #773). The open arc
> is WAL reclamation (RFC 0052, `red`, slices A–D1 merged), with
> backpressure, unwind safety, publication frontiers and audit
> durability drafted behind it (RFC 0053–0056).
>
> Prior entry — 2026-07-27: the typed-promotion arc landed
> end-to-end and the dashboard decision is made. **RFC 0042** (typed numeric
> promotion, RFC 0022 §7.1 enacted) went spec → `green` in two days
> (#647–#653), closing with RFC0042.9 verified on live telemetry: the
> agent queried its own spend over MCP — the
> `sum(attr.cost_usd) by attr.model` query returned 35.28 USD across
> 10 requests from the typed `Float64` column. Getting the capture
> to flow fixed a latent env bug (#654: per-signal OTLP endpoints are
> used as-is per spec, so the dogfood env now carries explicit
> `/v1/<signal>` paths). **RFC 0041** went `specified` → **`green`**
> the same date: build now, **Perses first** — the three plugins shipped
> in the dedicated `ourios-perses-plugin` repository (PRs #1–#6), and
> the capstone FinOps dashboard (RFC0041.6, `examples/perses/`, #661)
> rendered unmodified against the live dogfood capture. One recorded
> deferral: RFC0041.5's `latest` matrix leg waits for the next server
> release (the first with typed columns); Grafana remains an ungated
> follow-up. The unreleased breaking changes on `main` since
> v0.5.0: #641 (RFC0002.21 severity) and #645 (Helm otel values) —
> the next tag is not a patch.
>
> Prior entry — 2026-07-25: the self-observability arc landed and
> the RFC 0036 arc closed. RFC 0038 (self-tracing), RFC 0039 (inbound
> trace-context propagation) and RFC 0040 (DataFusion operator
> instrumentation) are all `green`: Ourios continues a caller's trace
> instead of starting its own, and a finished physical plan is
> reconstructed post-hoc into a span tree by `ourios-df-otel` — a
> crate carrying only `datafusion` + `opentelemetry`, kept extractable
> for upstream. RFC 0037 (GenAI / structured-event logs) is `green`.
> **v0.5.0** shipped. §3's ladder now covers RFC 0001 through RFC 0041.
> RFC 0041 (dashboard datasource plugins) was `drafted` with §5/§6
> deliberately empty — its §7 asked whether the work is worth doing at
> all. *(Resolved in the current entry: yes, Perses first.)*
>
> One **unreleased breaking change** sat on `main` behind v0.5.0 at
> this entry: RFC0002.21 (#641), unspecified severity aligned with the
> OTel SDK. *(The current entry tracks the full unreleased-breaking
> list.)*
>
> Prior entry — 2026-07-21: the comparative program closed and
> the ingest-capacity arc landed; §3's ladder now covers RFC 0001
> through RFC 0036. RFC 0031's first fully authoritative comparative
> run (`baseline-8vcpu-32gib`, `benchmarks.md` §9.24) passed all 11
> frozen gate decisions — L1 97.82× / L3 22.52× storage-primary, L2
> 38.37× / L4 85.14× processed-primary, both L6 latency floors — and
> the RFC flipped to `validated` (`accepted` is a maintainer flip).
> The D1 arc shipped alongside: RFC 0034 (`specified`, enacted)
> recast D1 as a per-node bar, and RFC 0035 (`green`) split ingest
> into an ordered mining phase and a concurrent encode/publish phase
> to clear it — the §9.23 asserting soak holds an offered 100k
> lines/s per node (99.92% achieved, p99 153.63 ms) on the baseline
> hardware. RFC 0036 (`specified`) opens the next arc: write-side
> layout (compaction-time service/time sort), the remaining storage
> lever against hazard #4. *(That arc has since closed — see the
> current entry.)*
>
> Prior entry — 2026-07-15: a month of post-MVP shipping work
> landed since the prior entry below; §3's RFC ladder now covers
> RFC 0001 through RFC 0033 and §5's deferred-capabilities table
> (eight rows) is rewritten: six have shipped outright (the WAL, the
> OTLP wire endpoints, the snapshot mechanism, the §6.8 telemetry
> surface, the query DSL, and the `ourios-server` binary + Helm
> chart), multi-tenancy-at-runtime is partially landed (auth +
> tenant binding shipped via RFC 0026 `accepted`; rate-limit/
> eviction/lifecycle orchestration is still open), and the Perses
> datasource plugin remains fully deferred. Current work is RFC 0031
> (comparative evaluation against
> Grafana Loki) — a post-MVP thesis-strengthening effort, not a new
> MVP gate — with the `L1`/`L3`/`L6` classes frozen and gate-enforcing
> per its §7 and the last must-win class (`L4`, frequency aggregation)
> mid-dispatch. Phases 1–3 in §4 are all complete; that section is
> historical narrative only from this point forward.
>
> Prior entry — 2026-06-15: RFC 0013 (object storage, S3-compatible)
> drafted → `specified` → `red` (first shipping-milestone spine; `store`
> module skeleton + §5 stubs landed); RFC 0009 (background
> compaction) flipped to `validated` (RFC0009.7 D2/D3/B2-post measured on
> `baseline-8vcpu-32gib`, §9.7); RFC 0005 (Parquet storage) and RFC 0010
> (audit-stream / drift
> queries) flipped to `green` (RFC0005.6 row-group sizing landed; RFC 0010's
> eight §5 drift scenarios all pass). Earlier, on
> 2026-06-14, RFC 0001, RFC 0008, and RFC 0011 flipped
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

## 3. Current state (as of 2026-09-27)

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

**RFC ladder status** (read from each RFC's `status:` front matter on
`main`, 2026-09-27):

| RFC | Area | Status |
|---|---|---|
| 0001 | Template miner | **`accepted`** |
| 0002 | Query DSL | **`accepted`** |
| 0003 | OTLP receiver (gRPC + HTTP) | **`accepted`** |
| 0004 | Configuration policy | **`accepted`** |
| 0005 | Parquet storage | **`accepted`** |
| 0006 | Bench harness | **`accepted`** |
| 0007 | Querier (DataFusion + logs DSL) | **`accepted`** |
| 0008 | WAL | **`accepted`** |
| 0009 | Background compaction | **`accepted`** — RFC0009.7 D2/D3/B2-post measured authoritatively on `baseline-8vcpu-32gib` (`benchmarks.md` §9.7) |
| 0010 | Audit-stream / drift queries | **`accepted`** |
| 0011 | A1 re-scope | **`accepted`** |
| 0012 | meta: CLAUDE.md §2 pillar-#2 wording | **`accepted`** |
| 0013 | Object storage (S3-compatible) | **`accepted`** |
| 0014 | Ingest write path: record sink and flush policy | **`accepted`** |
| 0015 | Fuzzing harness: cargo-fuzz + ClusterFuzzLite CI | **`accepted`** |
| 0016 | Query-serving endpoint: HTTP query API over the logs DSL | **`accepted`** |
| 0017 | Read-time template registry & query-row rendering | **`accepted`** |
| 0018 | OTLP log-spec compliance amendments | **`accepted`** |
| 0019 | Storage-backend selection (local vs S3) | **`accepted`** |
| 0020 | Server configuration file (YAML + env-var substitution) | **`accepted`** |
| 0021 | Coordinated DataFusion / Arrow upgrade | `green` — phase 2a landed (DataFusion 55 / arrow 59 / parquet 59, #773); phase 2b (object_store 0.14) and dropping the `collect_statistics = false` row-group-pruning workaround wait on DataFusion 56 (draft #846), and the upstream fix does not yet cover the RFC 0044 `body ==` shape |
| 0022 | Queryable attribute columns (RFC 0005 amendment) | **`accepted`** |
| 0023 | Bounded template memory (RFC 0001 amendment) | **`accepted`** |
| 0024 | OTLP-envelope property testing (RFC 0006 amendment) | **`accepted`** |
| 0025 | Absent-body representation (RFC 0005 amendment) | **`accepted`** |
| 0026 | Authentication and tenant binding (ingest + query) | **`accepted`** |
| 0027 | MCP query surface (agent-facing read tools) | **`accepted`** |
| 0028 | Build-feedback program (test-harness + workspace decomposition) | **`accepted`** |
| 0029 | OIDC bearer layer (issuer-agnostic, Dex-validated) | **`accepted`** |
| 0030 | TLS/mTLS on the data-plane listeners | **`accepted`** |
| 0031 | Comparative evaluation against Grafana Loki | **`accepted`** — the first fully authoritative run (`benchmarks.md` §9.24) passed all 11 frozen gate decisions; losses published per §5 |
| 0032 | Query-schema and cost-model resource for the MCP surface | **`accepted`** |
| 0033 | Cached template-map artifact | **`accepted`** |
| 0034 | D1 re-scope: per-node ingest-throughput bar | **`accepted`** — satisfied by the §9.20–§9.23 measurement series |
| 0035 | Ingest concurrency (ordered mining, concurrent encode/publish) | **`accepted`** |
| 0036 | Write-side layout (compaction-time service/time sort) | **`accepted`** |
| 0037 | GenAI / structured-event log support | `green` |
| 0038 | Self-tracing (OTel traces for Ourios itself) | **`accepted`** |
| 0039 | Inbound trace-context propagation | **`accepted`** |
| 0040 | DataFusion operator instrumentation | **`accepted`** |
| 0041 | Dashboard datasource plugins (Grafana / Perses) | **`accepted`** — Perses plugins shipped in `ourios-perses-plugin`; Grafana an ungated follow-up |
| 0042 | Typed numeric promotion (RFC 0022 amendment) | **`accepted`** |
| 0043 | Derive `event_name` from the legacy `event.name` attribute | **`accepted`** |
| 0044 | Template-aware body equality (`body ==` two-arm compile) | **`accepted`** |
| 0045 | Operator-configured composite tenant derivation | `superseded` by RFC 0046 |
| 0046 | Out-of-band tenancy (the credential names the tenant) | **`accepted`** |
| 0047 | ReBAC resolver (OpenFGA) and graph-fed visibility | **`accepted`** |
| 0048 | Graph operational surfaces (tenant grammar, identity keys, erasure, backfill) | **`accepted`** |
| 0049 | Agent delegation and the RFC 8693 `act` claim | `specified` |
| 0050 | Upstream-derived templates (`log.record.template`) | **`accepted`** |
| 0051 | `ourios-serving`: shared serving infrastructure out of the ingest crate | `green` |
| 0052 | WAL reclamation and quiesce recovery | `red` — slices A1 (#820), A2 (#821), B (#823), C (#828) and D1 (#831, the publication barrier) merged; four slice-D stubs and slices E–F remain |
| 0053 | WAL backpressure | `drafted` |
| 0054 | Publish unwind safety | `drafted` |
| 0055 | Publication frontiers and tenant settlement | `drafted` |
| 0056 | Audit-sink durability on permanent write failure (RFC 0005 amendment) | `drafted` |
| 0057 | meta: move CLAUDE.md to AGENTS.md | `drafted` |

**Crates — fourteen workspace crates, thirteen of them product crates**
(`ourios-core`, `-config`, `-miner`, `-wal`, `-parquet`, `-ingester`,
`-querier`, `-server`, `-serving`, `-bench`, `-semconv`, `-telemetry`,
`-df-otel`; the fourteenth, `-testgen`, is dev-only):

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
- **`ourios-ingester`** — RFC 0003 `accepted`: the OTLP gRPC + HTTP receiver
  with WAL-before-ack, out-of-band tenancy (RFC 0046: the credential
  names the tenant, one export is one tenant), the windowed
  group-commit coordinator, and the startup recovery driver; also hosts
  the RFC 0009 compaction runner and the RFC 0035 two-phase pipeline
  (ordered mining, concurrent encode/publish).
- **`ourios-querier`** — RFC 0007 / RFC 0002 `accepted`: the logs
  DSL over DataFusion with predicate + partition (time-window) pruning,
  alias resolution, the RFC 0010 drift query, `param(n)`/`bucket(width)`
  aggregation (RFC 0002's L4 amendment), and the RFC 0032 query-schema +
  cost-model MCP resource.
- **`ourios-bench`** — RFC 0006 `accepted`: drives the A1/B1/B2/C1/C2
  measurements over OTLP-Demo + LogHub corpora, records results to
  `benchmarks.md` §9, and (RFC 0031) runs the comparative dispatch
  against a real Loki container.
- **`ourios-df-otel`** — RFC 0040 `accepted`: a post-hoc `ExecutionPlan` →
  OTel span-tree walk, backdating each operator span from its
  `BaselineMetrics` timestamps. Its *runtime* dependencies are `datafusion`
  and `opentelemetry` alone — no `ourios-*` crate among them — so it stays
  extractable as an upstream contribution.
- **`ourios-serving`** — RFC 0051 `green`: the plumbing both roles
  share — bearer authentication and tenant binding, the OIDC and
  OpenFGA clients, listener TLS/mTLS, and inbound trace-context
  extraction — extracted from `ourios-ingester` so the querier role no
  longer depends on the ingest crate.
- **`ourios-core`** / **`-config`** / **`-semconv`** / **`-telemetry`** /
  **`-server`** — shared types + tenancy + record/audit shapes; the RFC
  0004 miner tunables (split out per RFC 0028 §3.2); the
  weaver-generated OTel name constants; the OTel export surface —
  metrics (RFC 0018) and, since RFC 0038/0039, traces; the two-role
  binary, now with TLS/mTLS (RFC 0030), an OIDC bearer layer (RFC 0029),
  and the S3-native Helm chart, deploy-validated on kind.

The full `cargo test --all-features` suite is green in CI — the `cargo
test` job gates every PR on the exact head; the coverage job runs
alongside it but is informational (`continue-on-error`), not gating.

**What remains** is no longer "reach MVP" — that bar cleared a month
before this entry (§3's gate table is unchanged and still authoritative)
and the shipping milestone that followed (WAL, wire endpoints, DSL,
auth, S3, Helm — the whole §5 table below except Perses) is
substantially done. What's actually open:

- **The Perses plugins** — RFC 0041 is `accepted` (2026-07-28,
  terminal): all three plugins shipped in `ourios-perses-plugin`, the
  RFC0041.6 dashboard is committed in this repo and verified rendering.
  What remains is RFC0041.5's recorded deferral (the `latest` e2e leg +
  wire-level `sum`, unblocked by the next server release, tracked in
  the plugin repo) and the Grafana datasource as an ungated follow-up.
- ~~RFC 0040 → `accepted`~~ — done (2026-07-28), alongside RFC 0041 and
  RFC 0042: all three flipped `accepted` on maintainer sign-off.
- **WAL reclamation and its follow-ons** — RFC 0052 is `red` with
  slices A1–D1 merged (#820, #821, #823, #828, #831); the remaining
  slice-D stubs, the timer/telemetry slice E and the crash/soak slice F
  are open. RFC 0053 (backpressure), RFC 0054 (publish unwind safety),
  RFC 0055 (publication frontiers) and RFC 0056 (audit durability) are
  `drafted` and depend on it.
- RFC 0049 (agent delegation, the `act` claim) is `specified`; RFC 0057
  (`meta:` CLAUDE.md → AGENTS.md) is `drafted`.
- Scattered §7/§9 open items on already-`green`/`accepted` RFCs (e.g.
  the recurring D1/D2 soak cadence now that the harness has shipped
  (§9.19/§9.23); RFC 0021's phase 2b and the `collect_statistics`
  pruning workaround, now waiting on DataFusion 56 (#846) since
  DataFusion 55 landed in #773; RFC 0028's musl cargo-dist re-add;
  RFC 0031's deferred F_L7) — none block anything downstream.

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

Each item is a real production concern. The reason it was deferred
*for MVP* is *"answering 'does the thesis hold?' doesn't require
it,"* not *"we don't think it matters."* As of this entry, seven of
the eight original rows have shipped outright — the Perses row's
plugin half landed with RFC 0041 (`accepted`; CRDs/operator stay
ungated) — and one (multi-tenancy at runtime) is partially landed,
as part of the post-MVP shipping milestone (§3); the table below
records what shipped and what's still genuinely open.

| Capability | Why deferred for MVP | Status |
|---|---|---|
| **Write-ahead log** (`ourios-wal`) | Corpus replay is bounded and reproducible; durability is irrelevant for thesis-proving | **Landed** — RFC 0008 `accepted`: append/sync, real-SIGKILL crash recovery, snapshot-restore, group-commit batched fsync |
| **OTLP wire endpoints** (gRPC + HTTP listeners) | Bench reads OTLP from disk, not the network — see Phase 3 | **Landed** — RFC 0003 `accepted`: gRPC + HTTP receivers, WAL-before-ack; tenancy out of band per RFC 0046 |
| **Snapshot mechanism** (RFC 0001 §6.9) | Corpus runs from cold start; replay budget moot | **Landed** — part of RFC 0008 (`accepted`), v2 restore format |
| **Full §6.8 telemetry surface** | One or two metrics suffice for the bench; the §3.1.2 mandatory set is a production observability concern | **Landed** — OTel meters + OTLP metric exporter (RFC 0018 `accepted`); Ourios's own logs ship via its own OTLP exporter (dogfooded: one deployment ingests another's telemetry). **Traces landed too** (2026-07-24/25): RFC 0038 gives request-scoped spans on ingest/query/`/mcp`/sweep, RFC 0039 continues an inbound caller's trace rather than starting a new one, and RFC 0040 adds a DataFusion operator span tree under a query. All three signals are now configured through the standard `OTEL_*` env vars |
| **Query DSL** (RFC 0002) | Raw SQL through DataFusion serves the bench; DSL is operator UX | **Landed** — RFC 0002 `accepted`, including the `param(n)`/`bucket(width)` aggregation amendment |
| **Multi-tenancy at runtime** (rate limits, eviction, lifecycle) | Bench uses one tenant; the type is in place but no orchestration around it | **Partially landed** — authentication + enforced tenant binding (RFC 0026), out-of-band tenancy where the credential names the tenant and the data never does (RFC 0046, superseding RFC 0045), ReBAC graph-fed visibility inside a tenant (RFC 0047), and the tenant id grammar, erasure and backfill surfaces (RFC 0048) are all `accepted`; rate-limit/eviction/lifecycle orchestration is still open, tied to an operator-console RFC that hasn't been drafted (RFC 0001 §9) |
| **`ourios-server` binary + Helm chart** | Bench is a binary in `ourios-bench`; full deployment shape is shipping concern | **Landed** — two-role binary with TLS/mTLS (RFC 0030) + OIDC (RFC 0029); S3-native Helm chart shipped and deploy-validated on kind |
| **Perses dashboard integration** (datasource plugin + possible CRDs) | The data plane has to work first — a Perses plugin queries a query interface that doesn't exist yet. A native datasource plugin is small and downstream-friendly *once* the query API is stable; CRDs / operator (`PersesDashboard`-style declarative pipeline + miner config) would extend Ourios into managed-service territory, which contradicts `CLAUDE.md` §1's "Not a managed service" line | **Landed** (the plugin half) — **RFC 0041 `accepted`** (`green` 2026-07-27): three plugins (`Datasource`, `LogQuery`, `TimeSeriesQuery`) shipped in the dedicated `ourios-perses-plugin` repository, and the committed FinOps dashboard (RFC0041.6, `examples/perses/`) rendered unmodified against the live dogfood capture. Both hosts were spiked and measured first; RFC0041.5 carries a recorded deferral until the next server release; the Grafana datasource is an ungated follow-up. CRDs/operator still requires a `meta:` RFC against `CLAUDE.md` §1 first, no commitment to land |

**Note on OTLP scope (historical).** The pre-amendment roadmap
listed "OTLP receiver (gRPC + HTTP)" as a single post-MVP item.
PR #20 + #21 split that scope: the **OTLP record shape**
(`OtlpLogRecord` consumption, the canonical JSON encoding,
the OTLP-aligned Parquet schema) was in MVP from the start — it's
a prerequisite for thesis-gate **C2**'s validity, because the
template-count convergence the corpus measures has to be over
records that look like real OTel traffic, not over flat-text
caricatures of it. Only the **wire endpoints** — the actual
gRPC/HTTP listeners that decode OTLP off the network — were
deferred past MVP, and that's the row in the table above; RFC 0003
(`accepted`) has since landed them, so nothing in this note is
still-open scope.

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
