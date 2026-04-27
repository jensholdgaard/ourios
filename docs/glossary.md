# Glossary

Vocabulary used in the Ourios docs. Entries marked **(Ourios)**
carry a project-specific meaning that may differ from the
industry-default. Cross-references in *italics* point to other
entries here.

---

**Audit event.** A structured record emitted by the *miner* every
time a *template* is widened (parameters generalised), merged with
another template, or versioned. Audit events are themselves stored
as logs and are queryable. They are the trail by which an operator
can answer "did this template silently change yesterday?" See
`hazards.md` H1.

**Bit-identical reconstruction.** The property that, for any
ingested log line, either Ourios can reproduce the exact original
byte sequence from what it stored, or the row carries
`lossy_flag = true`. Never an in-between. Tested as a *property
test* against the corpus. See `CLAUDE.md` §3.3, `hazards.md` H7.

**Body.** The free-form text content of a log record. In OTel terms,
the `body` field of a `LogRecord`. In Ourios storage, the body is
either reconstructible from `template + params` (most rows) or
retained verbatim in a dedicated column (lossy rows, parse failures,
or tenants who opted in to always-retain).

**Compaction.** Background process that merges many small *Parquet*
files into fewer large ones, targeting *row-group* sizes of 128 MB
to 1 GB and file sizes of 256 MB to 2 GB. Driven by the small-file
hazard (H4).

**Confidence.** A scalar in [0, 1] assigned by the *miner* to each
matched row, measuring how well the row matched its assigned
template. The three-zone model partitions confidence into clean
match (≥ threshold), lossy match (floor ≤ x < threshold), and parse
failure (< floor). **(Ourios)** — extends Drain, which is
binary-classifying.

**Corpus.** A collection of anonymised log lines used as test input.
Lives under `testdata/corpus/`. Public LogPAI corpora form the
floor; self-collected corpora per deployment archetype are added
over time. Reconstruction, *template-count convergence*, and
*merge rate* are all measured against the corpus on every miner
change.

**DataFusion.** The Apache project providing the query engine
Ourios uses. Ingests logical plans, optimises them, executes against
*Parquet*. Ourios extends DataFusion with two custom logical nodes
(`render`, `template_id.resolves_to`) but otherwise treats it as a
black box. *DataFusion specifics never leak into the user-facing
DSL* (H6).

**Drain.** The 2017 paper (He, Zhu, Zheng, Lyu — ICWS 2017) that
introduces a fixed-depth tree algorithm for online log parsing. The
basis of the *miner*. See `docs/rfcs/0001-template-miner.md` and
`docs/talks/0001-template-miner.md`.

**Drain3.** The IBM-maintained fork of Drain that adds persistent
state, masking, variable-length wildcards, and dynamic thresholds.
Some of its extensions are adopted in Ourios; some are explicitly
not. RFC 0001 §4 lists the per-extension verdict.

**Drift.** The phenomenon where a service's log format changes
between deploys, producing new templates that are aliases of older
ones. **(Ourios)** — drift is detected as a first-class query, not
an after-the-fact discovery. See H5 and RFC 0001 §6.7.

**DSL.** The user-facing query language for Ourios logs (RFC 0002).
Compiles to DataFusion logical plans; does not expose SQL. Two
candidate predicate sublanguages (OTTL-borrowed vs. distanced) and
three top-level surfaces (SQL-clause, LogQL-pipe, Insights-verb)
are under design.

**Floor.** The lower bound of confidence below which the *miner*
declares a parse failure. Default ~0.3. Below the floor, the row is
stored body-only and `parse_failures_total` increments. **(Ourios)**
— not present in the Drain paper.

**Fsync.** The POSIX call that forces buffered writes to durable
storage. The *WAL* fsyncs before acknowledging an OTLP batch. See
H3, `CLAUDE.md` §3.4.

**Hazard.** A named failure mode that, if not actively mitigated,
silently corrupts data or destroys the project's value
proposition. The seven current hazards are catalogued in
`docs/hazards.md`. New hazards are added via a `meta:` RFC.

**Ingester.** The Ourios role that receives OTLP over gRPC/HTTP,
mines templates, writes to the *WAL*, and (eventually) flushes to
*Parquet* via the compactor. One half of the ingester/querier
binary split.

**Length group.** The first-level partition in the Drain *parse
tree*: one branch per distinct token count. Drain assumes lines of
different length are probably from different call sites and uses
length as a cheap initial filter.

**Log group.** Drain's term for a *template* together with the rows
that have matched it. A *leaf* in the parse tree contains a list of
log groups.

**Lossy.** A row whose `lossy_flag` is set, indicating that
reconstruction from `template + params` may not be byte-identical.
Always paired with the original *body* being retained on that row.
See H7.

**LogPAI.** The benchmark-corpus project for log parsing
(github.com/logpai/logparser). Ourios uses LogPAI corpora (HDFS,
BGL, Spark, Apache, OpenSSH, Windows) as the public-corpus floor
for benchmarks.

**Masking.** Pre-tokenisation regex rules that replace volatile
sub-strings (IPs, UUIDs, numbers) with placeholders before the
miner walks the tree. A Drain3 extension. Whether and where Ourios
applies masking is a design choice in RFC 0001 §4.

**Merge.** When the *miner* widens an existing template to absorb
a new line — e.g. replacing a literal token with a wildcard. Every
merge fires an *audit event*. Strict thresholds make merges rare;
audit makes them visible. See H1.

**Miner.** Short for *template miner* — the Ourios subsystem that
runs Drain online over ingested log lines and emits
`(template_id, params, confidence, lossy_flag)` for each row.
Lives in the `ourios-miner` crate. Designed in RFC 0001.

**OTLP.** OpenTelemetry Protocol, the wire format for telemetry
data. The Ourios ingest contract: incoming logs are OTLP over gRPC
or HTTP. We do not invent our own format.

**OTTL.** OpenTelemetry Transformation Language, the OTel
Collector's text-based DSL for filtering and mutating telemetry in
processor pipelines. Ourios deliberates between borrowing OTTL's
predicate sublanguage and distancing from it (RFC 0002).

**Parquet.** The Apache columnar file format Ourios uses for
on-disk storage. Per-column compression, predicate pushdown via
min/max statistics, bloom filters, page indexes. The on-disk truth
of the system; local disk is cache and *WAL* only. See `CLAUDE.md`
§§2.1, 3.6.

**Params.** The variable parts of a log line that the *miner*
extracts when matching a *template*. Bounded per-parameter to
256 B by default; overflow spills to the *body* column. See H2.

**Parse failure.** A row whose match confidence falls below the
*floor*. Stored body-only; `parse_failures_total` counter
increments.

**Predicate pushdown.** A query-engine optimisation where filter
predicates are applied as early as possible — at the storage layer
rather than after a full scan. Parquet's min/max page statistics
make this nearly free for time-range and equality filters. The
mechanism by which predicate queries beat `zstdcat | grep` (B1).

**Property test.** A test that asserts an *invariant* over many
randomly-generated inputs (typically via `proptest`). In Ourios:
*reconstruction* is always a property test; the parser
round-trips; the miner's tree operations preserve invariants. See
`CLAUDE.md` §6.2.

**Querier.** The Ourios role that accepts queries (over the *DSL*),
plans them through DataFusion, scans Parquet, and returns results.
Other half of the ingester/querier split.

**Reconstruction.** The act of producing the original *body* of a
log line from the stored `template + params` (and, where retained,
the captured whitespace state). Subject to the *bit-identical*
guarantee. See H7.

**Row group.** Parquet's unit of compression and predicate-pushdown
locality — a horizontal partition of rows within a file. Target
size 128 MB to 1 GB. Smaller row groups mean faster row-group skip
but worse compression and more metadata overhead.

**Similarity.** The Drain match score between an incoming line and
a *log group*'s *template*: the fraction of token positions where
the line matches the template (wildcards count as matches). The
single most important knob in the system. See RFC 0001 §3.

**SUMMARY.md.** mdBook's table-of-contents file (`docs/SUMMARY.md`)
that defines book navigation. Drafts (no link target) appear as
greyed-out entries.

**Template.** The structural pattern of a class of log lines, with
variable parts replaced by wildcards. E.g.
`ERROR db connection failed for user <*>`. The *miner* extracts
templates online from raw logs. **(Ourios)** — every template is
scoped per *tenant*; the same string in two tenants is two
templates.

**Template id.** The identifier of a *template* within a *tenant*.
Either a hash of the canonical template string or a per-tenant
monotonic integer (open question, RFC 0001 §6.1).

**Template tree.** The Drain parse tree, scoped per *tenant*. Its
shape is `root → length group → token-prefix nodes (depth d) →
leaf log groups`. **(Ourios)** — Drain assumes one tree; we keep
one per tenant (`CLAUDE.md` §3.7).

**Template version.** A monotonic integer that bumps when a
template's representation changes (e.g. token order, new wildcard).
The *logical identity* of the template persists across versions via
the alias mechanism. See *drift*, RFC 0001 §6.7.

**Tenant.** An isolation boundary: a customer, a project, an
environment. Every code path that touches data takes a `tenant_id`;
every Parquet file is partitioned by tenant; every template tree is
scoped per tenant. Multi-tenancy is *not bolted on*
(`CLAUDE.md` §3.7).

**Thesis-gate.** A benchmark goal whose failure on representative
corpora invalidates an *architectural pillar* — meaning the
response is an RFC to revisit `CLAUDE.md` §2, not a tuning sprint.
The five thesis-gates are catalogued in `docs/benchmarks.md` §7.

**Threshold (`st`).** The Drain similarity cutoff above which a
line is assigned to an existing *log group* rather than opening a
new one. Ourios default ≥ 0.7; values below 0.7 require an RFC
(H1, `CLAUDE.md` §3.1).

**Token-prefix node.** Drain's intermediate tree level: branches on
the value of the line's first N tokens (depth `d`, paper default
3–4). Below it, at the *leaf*, is a list of *log groups*.

**Truncation marker.** The placeholder that replaces an oversized
*params* slot when the per-parameter byte limit is exceeded. The
original value spills to the *body* column. See H2.

**WAL.** Write-ahead log. The Ourios *ingester* writes every
acknowledged batch to the WAL, fsyncs, and only then acknowledges
to the OTLP client. WAL segments are eventually flushed to *Parquet*
by the compactor. The crash-recovery test SIGKILLs the ingester
mid-batch and asserts no acknowledged data is lost. See H3,
`CLAUDE.md` §3.4.
