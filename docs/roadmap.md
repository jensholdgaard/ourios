# Roadmap to MVP

> Living document. Refreshed at phase boundaries (§4) and whenever
> a merged PR materially changes the *current state* in §3.
> Last updated: **2026-06-14** — RFC 0001 advanced to `validated` on the
> maturity ladder (C1/C2 pass authoritatively on §1 baseline hardware,
> `benchmarks.md` §9.6; A1 is diagnostic per RFC 0011). RFC 0008 is
> `green`. The §§4+ phase narrative below predates this and is not
> re-verified here
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

Five `[THESIS]`-marked goals in
[`benchmarks.md`](./benchmarks.md) define MVP-done. Hitting all
five on a representative corpus means the thesis holds; missing
any of them means a pillar (`CLAUDE.md` §2) is wrong and a PR
won't fix it — an RFC will.

| Gate | What it measures | Why it matters |
|---|---|---|
| **A1** | End-to-end compression ratio vs. zstd-alone over flat text | Pillar 1 (Parquet) + Pillar 2 (template mining) stack, or they don't |
| **B1** | Predicate-pushdown query latency on time/template/tenant filters | Pillar 1 (footer reads + min/max stats skip row groups) actually skips |
| **B2** | Template-exact query latency (`where template_id = X`) | Pillar 2's `template_id` column is a usable index, not a curiosity |
| **C1** | Bit-identical reconstruction rate over the corpus | The hardest invariant (`CLAUDE.md` §3.3) holds in practice, not just in unit tests |
| **C2** | Template-count convergence (Drain finds a small, stable number of templates) | Pillar 2 (template mining) extracts the structure we believed was there |

`A2`, `B3`, `C3`, `C4`, `D*`, `E*` in `benchmarks.md` are
relevant but not MVP-blocking — they're tuning goals, honesty
goals, or post-MVP shipping concerns.

---

## 3. Current state (as of 2026-06-14)

**RFC 0001 §5 scenarios green: 42 / 42 — RFC 0001 status: `validated`**
(2026-06-14; every scenario has a live passing test, incl. the
cross-crate criteria relocated to `ourios-querier`/`ourios-ingester`).
`validated` reached: the C1/C2 thesis-gates pass on a representative
≥ 1 M-line corpus, authoritatively on the §1 baseline hardware
(`benchmarks.md` §9.6); A1 is a diagnostic, not a gate (RFC 0011). See
the RFC's status note. RFC 0008 is likewise `green` (all §5 arms .1–.10).
RFC 0005 §5 scenarios green: **10 / 11** (RFC0005.6 — the
≥256 MiB row-group sizing scenario — defers until a corpus run
exists; see `docs/rfcs/0005-parquet-storage.md` §6). RFC 0005
status: still `drafted` in frontmatter; a follow-up maintainer
sign-off bumps it to `green` once RFC0005.6 has a corpus
fixture or is explicitly deferred via the §7 open-question on
slow-test CI cadence.

What the code does today:

- **`ourios-core`** — `MinerConfig` + `TenantConfig` per-tenant
  overrides (RFC 0004), `TenantId` newtype, `OtlpLogRecord` /
  `AnyValue` (RFC 0001 §6.1 OTLP-aligned record shape),
  `MinedRecord` with the full RFC 0001 §6.1 / RFC 0005 §3.2
  column set (severity, scope, attributes, resource attributes,
  trace / span / flags, event name, body kind + raw body,
  params + separators, confidence, lossy flag), `AuditEvent` /
  `AuditEventKind` / `ParamType` / `SlotExpansion` with the
  `ParamType::Unknown(i32)` §3.9 catch-all, `audit::AuditSink`
  trait + `InMemoryAuditSink` / `NoOpAuditSink` placeholders,
  `confidence` / `clock` primitives.
- **`ourios-miner`** —
  - `tokenize` (Unicode-whitespace splitting, separators
    captured and threaded through `reconstruct()`).
  - `mask` — the private `MaskTag` enum carries the
    mask-emitted subset (`Uuid`, `Ip`, `Num`); `ParamType` in
    `ourios-core` carries the full RFC 0001 §6.1 alphabet
    (including the reserved `Hex` / `Ts` / `Path` and the
    non-mask-emitted `Str` / `Overflow`). `Str` is produced by
    widening (§6.2 step 5b — slot type expansion); `Overflow`
    is produced by the per-parameter byte-limit check (§6.5,
    in `overflow`). `Hex` / `Ts` / `Path` are reserved with no
    emitter yet.
  - `sim_seq` + `confidence_ratio` + `Token` (RFC §3.2 / §6.3
    primitives, driving best-candidate attach selection).
  - `tree` — Drain prefix tree with descend + descend_mut.
  - `MinerCluster` + `TenantState` — best-candidate attach via
    `sim_seq`, widening with `TemplateWidened` audit, type
    expansion with `TemplateTypeExpanded` audit, degenerate-
    template guard with `TemplateWideningRejectedDegenerate`
    audit, three-zone confidence branching (clean / lossy /
    parse-failure), per-parameter byte-limit + OVERFLOW marker
    + forced body retention.
  - `reconstruct()` — bit-identical reconstruction from
    template + params + separators when `lossy_flag = false`;
    returns the retained body verbatim when `lossy_flag = true`.
    The H7.1 corpus property test is green.
- **`ourios-parquet`** — RFC 0005 §3 fully implemented:
  - `data_schema()` / `audit_schema()` Arrow schemas pinned by
    `tests/schema_pin.rs` (RFC0005.10).
  - `Writer` — `Writer::open` → `append_records` → `close` with
    atomic `<uuid>.parquet.tmp` + rename publish, `Drop` cleanup,
    §3.5 row-group flush at 128 MiB, §3.6 per-column encoding
    policy (dict + page index + `template_id` bloom filter), and
    a `Poisoned` state on `ArrowWriter` write/flush errors that
    refuses to publish.
  - `Reader::open_partition` / `open_file` / `read_all` with the
    §3.9 forward-/backward-compatibility contract (unknown
    columns ignored, missing OPTIONAL → `None`, missing baseline
    REQUIRED → hard error) and §3.4 row-vs-path validation
    (tenant + UTC year/month/day/hour).
  - `AuditWriter` / `AuditReader` for the §3.7 audit-event file
    series, partitioned at `audit/tenant_id=…/year=…/month=…/
    day=…/` (no hour segment per §3.4), with the same atomic
    publish + poisoning contract as the data writer.
  - `PartitionKey::derive` + `data_path` / `audit_path` with
    percent-encoded tenant IDs.
- **Crates not yet on disk.** `ourios-wal`, `ourios-ingester`,
  `ourios-querier`, `ourios-server`, `ourios-bench` are listed
  in the workspace `Cargo.toml` as comments. Per §5 the WAL,
  ingester, server, and full snapshot mechanism are deferred
  post-MVP; the querier and bench are the Phase 3 work.

What's specifically remaining for the thesis gates:

| Gate | Blocker(s) |
|---|---|
| **A1** | `ourios-bench` corpus runner driving the miner → Parquet path; A1's compression ratio is measured by the bench. Writer + reader are in place. |
| **B1** | `ourios-querier` (DataFusion table provider over the Parquet partition layout); predicate-pushdown wiring against `time_unix_nano` / `tenant_id`. Reader already enforces row-vs-path validation. |
| **B2** | Same as B1 plus `template_id` as a queryable column (the column is in the schema and is the §3.6 bloom-filter target — the wiring is what's missing). |
| **C1** | Bench harness that exercises the H7.1 reconstruction property on a corpus end-to-end (the unit-level property test is green; the gate measures it at corpus scale). |
| **C2** | Bench harness that measures template-count convergence over a representative corpus — algorithm primitives are in place (widening + type expansion + degenerate guard); the measurement is the missing piece. |

For `cargo test --all-features`'s outer-loop view: **225 passed
/ 18 ignored**. The 17 ignored test stubs (plus one doctest)
map to RFC 0001 §5 scenarios whose dependencies (`§6.8`
metrics surface, `§6.9` snapshot mechanism, RFC 0002 drift
query) are post-Phase-1 or post-MVP work.

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
