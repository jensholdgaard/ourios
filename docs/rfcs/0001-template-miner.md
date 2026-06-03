---
rfc: 0001
title: Template miner (Drain-derived online log parsing)
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-04-24
supersedes: —
superseded-by: —
---

# RFC 0001 — Template miner

> **How to read this document.** §§1–4 are the design contract — the
> *what* and the *why*. §5 lists the normative `Given / When / Then`
> scenarios — the contract — grouped by parent (hazard, invariant,
> RFC-internal). §6 is the precise specification the `ourios-miner`
> crate is implemented against; its opening paragraphs name the
> gaps between the published algorithm and a production miner that
> §6.1–§6.9 then close. §7 records the alternatives we evaluated
> and rejected. §8 maps each §5 scenario to the technique that
> tests it.
>
> Cross-references to `CLAUDE.md` sections are in square brackets,
> e.g. `[§3.1]`, and name the invariant the section must preserve.

## 1. Summary

Ourios implements a Drain-derived online template miner (`ourios-miner`)
that converts each ingested OTLP `LogRecord` into a structured Parquet
record. The record shape is the OTLP `LogRecord` (with its inherited
Resource and InstrumentationScope context) plus the miner-derived
columns `(template_id, template_version, params, separators, body_kind,
body?, confidence, lossy_flag)`; see §6.1 for the full schema and the
2026-05-13 amendment that aligned it to OTLP. The miner is per-tenant
by construction `[§3.7]`, uses a three-zone confidence model that
retains the original line in the lossy zone `[§3.1]`, audits every
template widening `[§3.1]`, captures inter-token separators in a
parallel array so that bit-identical reconstruction is the default
rather than a property-test exception `[§3.3]`, bounds parameter
values at 256 B with overflow to a side `body` column `[§3.2]`, and
tracks template structural changes via a monotonic `template_version`
so that schema drift across deploys is a first-class query rather than
a silent count drop `[§3.5]`. The compression target is 50–200× over
raw bytes before any byte-level codec runs.

## 2. Motivation

This is the load-bearing pillar of the project `[§2.2]`. Three
sub-questions justify it.

**Why template mining at all.** A typical service emits 10²–10⁴
distinct `printf` templates over its entire lifetime, but raw log
volume is dominated by the parameters substituted into those
templates. Storing the template once per tenant and the parameters
per occurrence makes that redundancy explicit — and explicit
redundancy stacks with byte-level codecs rather than fighting them.
zstd over flat log text recovers ~10× on typical workloads; doing
the structural work first leaves zstd a column of short, repetitive
parameters that dictionary-encode well, where the codec then earns
its keep again. The 50–200× headline (`README.md`, `[§2.2]`) is the
product of these two layers, not a claim about either alone.

**Why online vs. offline.** Operators expect logs to be queryable
within seconds of ingest, not minutes. Any batch clustering window
long enough to do offline hierarchical clustering well is a window
the operator is blind in. Drain's fixed-depth tree gives O(d) lookup
per line at the cost of being slightly less accurate than the best
offline parsers — an acceptable trade because §3.1's audit and
confidence machinery surfaces the inaccuracy rather than hiding it.

**Why this layer.** The compression is structural, not statistical.
Doing it before Parquet's byte codecs means each Parquet column
sees small, dictionary-friendly values; doing it after means we have
already paid for storing the redundancy and zstd has to find it
again from the bytes. The order matters.

## 3. Background: Drain as published

A restatement of He et al., ICWS 2017, in the notation this RFC
uses downstream. Citations are by paper section/figure.

### 3.1 Tree structure (paper §3.2, Fig. 2)

A fixed-depth parse tree, depth `d` (default 4 in the paper). Three
node kinds, in order from root:

- **Root.** Single node; routes by token count.
- **Length-N node.** One per observed token count `N`. Children are
  prefix nodes keyed by the first token of the line.
- **Token-prefix nodes** at depths `2..=d`. Each is keyed by the
  token at position `(depth - 1)` of the line.
- **Leaf log groups** at depth `d + 1`. Each leaf holds a
  *template* — a sequence of `N` tokens, where each position is
  either a fixed string or the wildcard `<*>`.

```
                   root
                    │
        ┌───────────┼───────────┐
       len=4       len=5       len=6      ← length groups
        │           │           │
    ┌───┴───┐    ┌──┴──┐     ┌──┴──┐      ← prefix nodes (depth 1)
   "user"   …  "GET"  …    "INFO"  …
    │           │           │
   ┌┴┐         ┌┴┐         ┌┴┐
   …  …        …  …        …  …          ← prefix nodes (depth 2)
   │           │           │
 [leaf]      [leaf]      [leaf]           ← log groups
```

### 3.2 Similarity function (paper §3.3)

For a candidate line `L = (t_1, …, t_N)` and a leaf template
`T = (τ_1, …, τ_N)`:

```
simSeq(L, T) = (count of positions i where t_i == τ_i or τ_i is <*>) / N
```

Wildcards in the template count as matches. The line length and the
template length are equal by construction (the length-N node selected
the leaf candidates).

### 3.3 Threshold `st` (paper §3.4)

A configured value `st ∈ (0, 1]`. After computing `simSeq` against
every leaf at the current parent, the leaf with the highest `simSeq`
is the candidate. If `simSeq(L, T_best) ≥ st`, the line attaches to
`T_best`; otherwise a new leaf is created. The paper reports
`st = 0.4` as a default; see §6.3 for why Ourios overrides this.

### 3.4 New-log-group creation vs. leaf update (paper §3.5)

If `simSeq(L, T_best) < st`, a new leaf is created at the parent
prefix node, with `L` as its initial template (no wildcards yet).
Otherwise `T_best` is updated: at every position where
`t_i ≠ τ_i`, the template position is replaced with `<*>`. The
template never becomes more specific over time, only more general;
positions can become wildcards but cannot become fixed again.

### 3.5 Worked example

A fabricated illustration (no `testdata/corpus/` exists yet; this
example will be replaced with one drawn from the corpus once it
lands).

```
Line A:  user 42 logged in from 10.0.0.1
Line B:  user 17 logged in from 10.0.0.2
Line C:  user 99 logged out from 10.0.0.7
```

After preprocessing (§4.2), numbers and IPs are masked, so the
miner sees:

```
Line A:  user <NUM> logged in from <IP>
Line B:  user <NUM> logged in from <IP>
Line C:  user <NUM> logged out from <IP>
```

All three are length 6. They route to `root → len=6 → "user"`.

A walks the prefix path further (depth 2: token at position 1 is the
masked `<NUM>` placeholder, treated as a fixed token at this
stage). It is the first line, so a leaf is created with template
`user <NUM> logged in from <IP>`.

B walks the same path. The candidate leaf has `simSeq(B, T_A) =
6/6 = 1.0 ≥ st`. B attaches; the template is unchanged.

C walks the same path. The candidate leaf has `simSeq(C, T_A) =
5/6 ≈ 0.833`. With `st = 0.7` (Ourios default, §6.3),
`0.833 ≥ 0.7`, so C attaches. Token position 4 (`in` vs `out`,
1-indexed against the masked sequence) becomes `<*>`. The template
widens to `user <NUM> logged <*> from <IP>`. This is a *template
widening* event and must emit an audit record per §6.4.

## 4. Background: Drain3 extensions (not in the paper)

Drain3 (`logpai/Drain3`) is the maintained Python implementation. It
adds several capabilities beyond the 2017 paper. Each is recorded
here as `adopt`, `adopt with modification`, or `reject`, with one
sentence of rationale.

### 4.1 Persistent state — `adopt with modification`

Drain3 supports JSON snapshots to file, Redis, or Kafka. Ourios
adopts the snapshot concept but commits to a file/object-storage
backend; Redis and Kafka are out of scope (CLAUDE.md §3.6 names
object storage as the source of truth). Snapshot target, cadence,
and scope are open questions in §9.

### 4.2 Pre-tree-walk masking — `adopt with modification`

Drain3's most important extension: regex-based masking of common
parameter shapes (IPs, UUIDs, numbers, hex, timestamps, file
paths) *before* the tree walk, so high-cardinality tokens never
become tree branches. Without this, the tree explodes into one
branch per IP address.

The Ourios modification: a masked token is **not discarded**. It
becomes a *typed parameter* attached to the wildcard slot it
created. The masking layer emits `(type_tag, original_bytes)`
pairs; the tree walk treats the type tag as the token (so `<NUM>`
matches `<NUM>` for tree-routing purposes) while the
`original_bytes` flow into `params` so reconstruction can recover
the line exactly. Paper-pure Drain loses the original token; Ourios
retains it as a parameter. This is what makes `[§3.3]` reconstruction
possible at all.

### 4.3 Variable-length wildcards — `adopt with constraint`

Drain3's `MaskingInstruction` allows a single regex to match a
variable-length run of tokens (e.g. a multi-token user-agent
string). Ourios adopts this where the run is bounded at parse time
and produces exactly one typed parameter in the output. Reject:
unbounded variable-length wildcards, because they break leaf
identity (two lines with the same template structure but different
run lengths would land in different length-N nodes and never
deduplicate).

### 4.4 Dynamic / adaptive threshold — `reject`

Drain3 supports auto-tuning the similarity threshold per leaf based
on observed cluster sizes. Ourios rejects this. CLAUDE.md §3.1 fixes
`threshold ≥ 0.7` as a project-level invariant; auto-tuning would
silently move the merge boundary across deploys, defeating the audit
contract. Threshold tuning is a config decision per tenant, never a
runtime decision per leaf.

### 4.5 Other Drain3 features

- **Parameter-naming hints.** Drain3 lets users name `<*>` slots via
  the masking config (e.g. `<IP:client_addr>`). `adopt` — the
  type-tag mechanism in §4.2 already requires a slot name; using the
  Drain3 hint format keeps configs portable.
- **Built-in metrics surface.** Drain3 exposes a set of state
  counters via callback. `replace` — Ourios exposes OTel metrics
  directly per §6.8 (instrumented via the meter API), with names that
  match `[§3.1]`'s required set rather than Drain3's internal names.
- **Parameter masking after the fact.** Drain3 has utilities to
  retroactively mask params in already-clustered lines. `reject` —
  Ourios masks once, at ingest, deterministically. Retroactive
  masking would invalidate already-written Parquet files.

## 5. Acceptance criteria

Per `docs/verification.md` §§2–3, every CLAUDE.md §3 invariant and
every `docs/hazards.md` hazard this RFC touches has at least one
numbered scenario below. Scenarios use the bold-leading-clause
format (verification.md §2.1) and the id grammars (§2.2):
`H<n>.<m>` for hazard-rooted, `§3.<n>.<m>` for invariant-rooted,
`RFC0001.<m>` for design-internal commitments. Test code carries
each id in a doc comment per §2.3 so `grep -R "H1.1" .` resolves
bidirectionally between RFC and tests.

The hazards in scope are H1, H2, H5, H7; the invariants are §3.1,
§3.2, §3.3, §3.5, §3.7. H3 (WAL durability) and H4 (small files)
are owned by the `ourios-wal` and `ourios-parquet` RFCs; H6 (DSL)
is owned by RFC 0002; §3.4 (WAL-before-ack) and §3.6
(object-storage-as-truth) are touched only via §6.9's persistence
direction and the primary obligation lives in those other RFCs.

### 5.1 Hazards

> **Scenario H1.1 — Semantically distinct templates do not silently merge**
> - **Given** a corpus containing `user logged in <*>` and
>   `user logged out <*>`
> - **When** similarity threshold is 0.7 (the default)
> - **Then** the two remain distinct `template_id`s
> - **And** any widening produces an audit event recording both old
>   and new templates

> **Scenario H1.2 — Lossy-zone match retains body**
> - **Given** a line whose best match has confidence in the lossy
>   zone (`floor ≤ x < threshold`)
> - **When** the line is ingested
> - **Then** the `body` column contains the original line bytes
> - **And** the row carries `lossy_flag = false` (the flag is
>   reserved for tokenizer / preprocessing failure per §6.6 — the
>   lossy *zone* retains the body but reconstruction still
>   succeeds)

> **Scenario H1.3 — Every widening emits an audit event**
> - **Given** any sequence of inputs that triggers a template
>   widening
> - **When** the widening completes
> - **Then** an audit event exists naming the old template, the
>   new template, the tenant id, the timestamp, and the
>   `event_type`

> **Scenario H1.4 — `severity_number` is part of the template key (no INFO/ERROR silent merge)**
> - **Given** two `OtlpLogRecord`s with identical `body_kind = String`
>   bodies and identical `scope_name`, but `severity_number = 9`
>   (`INFO`) and `severity_number = 17` (`ERROR`)
> - **When** both are ingested via `MinerCluster::ingest`
> - **Then** the emitted records carry distinct `template_id`s
> - **And** no widening or merge ever produces a single
>   `template_id` covering both severity buckets
> - (Operationalises the §6.1 *Template-key composition*
>   commitment that `severity_number` is part of the key
>   regardless of `body_kind`.)

> **Scenario H1.5 — `scope_name` is part of the template key (no cross-scope silent merge)**
> - **Given** two `OtlpLogRecord`s with identical
>   `body_kind = String` bodies and identical `severity_number`,
>   but `scope_name = Some("lib.auth")` and
>   `scope_name = Some("lib.payments")`
> - **When** both are ingested
> - **Then** the emitted records carry distinct `template_id`s
> - **And** no widening or merge ever produces a single
>   `template_id` covering both scopes
> - **And** a third record with `scope_name = None` shares a
>   `template_id` with neither (it lives in the
>   `(severity, None)` bucket per §6.1)

> **Scenario H2.1 — Oversized parameter triggers OVERFLOW marker and forced body retention**
> - **Given** a tenant configured with the default 256 B
>   per-parameter byte limit
> - **And** a log line whose masked parameter value exceeds 256 B
>   (e.g. an embedded stack trace)
> - **When** the line is ingested
> - **Then** the corresponding `Param` entry has
>   `type_tag = OVERFLOW` carrying `(length, sha256_prefix)`
>   instead of the original value
> - **And** the `body` column contains the original line bytes
>   regardless of `lossy_flag`
> - **And** `params_overflow_total{tenant_id, service}` increments

> **Scenario H2.2 — Per-service overflow rate above 1% raises an alert**
> - **Given** the `params_overflow_ratio{tenant_id, service}`
>   gauge for some service
> - **When** the rolling rate exceeds `0.01`
> - **Then** the documented alert rule fires (the rule ships
>   alongside §6.5's metric definition)

> **Scenario H5.1 — Wildcard widening increments template_version and emits template_widened**
> - **Given** a leaf at `(template_id = X, template_version = V)`
> - **When** an attach widens a previously-fixed token at position
>   `i` into `<*>`
> - **Then** the leaf's `template_version` becomes `V + 1`
> - **And** an audit event with
>   `event_type = template_widened` is emitted naming the new
>   wildcard position(s)

> **Scenario H5.2 — Type expansion increments template_version and emits template_type_expanded**
> - **Given** a leaf whose wildcard slot `s` has
>   `slot_types[s] = {NUM}`
> - **When** an attach maps a typed parameter of `type = STR`
>   into slot `s`
> - **Then** `slot_types[s]` becomes `{NUM, STR}`
> - **And** `template_version` increments
> - **And** an audit event with
>   `event_type = template_type_expanded` is emitted naming the
>   slot and the newly-added `ParamType`

> **Scenario H5.3 — Drift query returns templates that gained a version in window**
> - **Given** the `template_audit` event stream contains
>   `template_widened` and `template_type_expanded` events for
>   templates A and B in the window `[t1, t2]`
> - **When** the §6.7 drift query runs against `[t1, t2]`
> - **Then** the result includes both A and B with their widening
>   counts

> **Scenario H7.1 — Reconstruction property holds across the corpus**
> - **Given** the committed `testdata/corpus/` (anonymised, fixed)
> - **When** every line is ingested through the miner
> - **Then** for every emitted record `r` where
>   `r.lossy_flag = false`, `reconstruct(r) == r.ingested_bytes`
>   holds byte-for-byte
> - **And** property failure is a build break, not a regression

> **Scenario H7.2 — Tokenizer failure sets lossy_flag = true and retains body**
> - **Given** a line containing an embedded NUL byte (or another
>   tokenizer-failure mode listed in §6.6)
> - **When** the line is ingested
> - **Then** a parse-failure record is emitted
> - **And** the record's `lossy_flag` is `true`
> - **And** the record's `body` column contains the original line
>   bytes

> **Scenario H7.3 — Reader emits body verbatim when lossy_flag is true**
> - **Given** a record with `lossy_flag = true`
> - **When** the reader renders the row
> - **Then** the rendered output is the `body` column verbatim
>   with the §6.6 warning marker
> - **And** `reconstruct()` is NOT called for that row

> **Scenario H7.4 — Widened literal slot reconstructs via STR fallback**
> - **Given** a leaf whose template gains a new `<*>` slot at
>   position `i` via the §6.2 widening of an originally-literal
>   token
> - **When** the triggering line is attached
> - **Then** the line's record carries
>   `params[slot_for_i] = { type_tag: STR, value: L_tok[i] }`
> - **And** `reconstruct(record) == ingested_bytes` holds

### 5.2 Invariants

> **Scenario §3.1.1 — Default similarity threshold is 0.7**
> - **Given** a tenant configuration with no threshold override
> - **When** the miner is initialised for that tenant
> - **Then** the effective threshold is `0.7`

> **Scenario §3.1.2 — Mandatory metric set is exposed**
> - **Given** a running miner
> - **When** the miner's meter (`global::meter("ourios.miner")`) is
>   collected via an SDK in-memory reader, at zero traffic
> - **Then** the collected metric stream contains every metric named
>   in §6.8's table (each present via its init-seeded data point)
>   (`template_count`, `merges_total`, `confidence`,
>   `confidence_p50`, `confidence_p01`, `body_retention_ratio`,
>   `parse_failures_total`, `params_overflow_total`,
>   `params_overflow_ratio`, `template_version_changes_total`,
>   `miner_latency_seconds`) with the instrument kinds and
>   attributes listed there

> **Scenario §3.2.1 — Default per-parameter byte limit is 256**
> - **Given** a tenant configuration with no per-parameter byte
>   limit override
> - **When** the miner is initialised for that tenant
> - **Then** the effective limit is `256` bytes

> **Scenario §3.2.2 — Configured limit above 1 KiB is rejected at startup**
> - **Given** a tenant configuration with
>   `param_byte_limit > 1024`
> - **When** the miner is initialised
> - **Then** initialisation fails with an error citing the §3.2
>   ceiling
> - **And** the process refuses to start serving that tenant

> **Scenario §3.3.1 — Separators array captured on every successful tokenization**
> - **Given** a line that tokenizes successfully
> - **When** the line is ingested
> - **Then** the emitted record's
>   `separators.len() == tokens.len() + 1`
> - **And** the per-row precondition for H7.1 holds (the
>   reconstruction proptest then asserts byte equality)

> **Scenario §3.5.1 — Snapshot format carries a leading version byte**
> - **Given** a serialised snapshot artefact written by the miner
> - **When** the artefact is inspected
> - **Then** byte 0 is the snapshot format version

> **Scenario §3.5.2 — Unknown snapshot version triggers full WAL replay**
> - **Given** a snapshot artefact whose leading version byte is
>   unknown to the running miner
> - **When** the miner loads the snapshot at startup
> - **Then** the snapshot is rejected
> - **And** the miner falls back to full WAL replay rather than
>   misinterpreting the bytes

> **Scenario §3.7.1 — Tenants' template trees never cross-pollinate**
> - **Given** a `MinerCluster` ingesting interleaved lines from
>   synthetic tenants A and B
> - **When** the corpus is fully ingested
> - **Then** no template mined under tenant A appears in tenant
>   B's tree
> - **And** no template mined under tenant B appears in tenant
>   A's tree
> - (Implements `docs/benchmarks.md` E2.)

> **Scenario §3.7.2 — Same structural template in two tenants gets distinct template_ids**
> - **Given** tenants A and B independently emit the structurally
>   identical template `user <NUM> logged in from <IP>`
> - **When** both are ingested
> - **Then** tenant A's `template_id` for that template differs
>   from tenant B's `template_id`
> - **And** no `template_id` is shared across tenants
> - **And** `template_id`s are guaranteed unique across the
>   entire cluster (not just per tenant)

> **Scenario §3.7.3 — Tenant derivation runs per `ResourceLogs`, not per export batch**
> - **Given** a single OTLP `ExportLogsServiceRequest` carrying
>   two `ResourceLogs` whose `Resource.attributes` resolve to
>   distinct tenants A and B under the configured derivation rule
> - **When** the receiver fans the batch out per RFC 0003 §6.3
>   and the miner ingests both per-tenant streams
> - **Then** every `LogRecord` under `ResourceLogs[0]` is mined
>   under tenant A
> - **And** every `LogRecord` under `ResourceLogs[1]` is mined
>   under tenant B
> - **And** no record ever appears in the wrong tenant's tree
> - (Operationalises the §6.1 *Tenant derivation* commitment
>   that the derivation rule runs once per inherited Resource,
>   not once per export batch.)

### 5.3 RFC-internal design commitments

> **Scenario RFC0001.1 — Fresh-leaf creation does not emit an audit event**
> - **Given** a parent prefix node with no leaves yet
> - **When** a line creates the first leaf at that node
> - **Then** no event is appended to the audit stream for that
>   creation
> - **And** `template_count` increments to reflect the new leaf

> **Scenario RFC0001.2 — Degenerate-template guard rejects fully-wildcard widening**
> - **Given** a leaf whose template, after a candidate widening,
>   would have zero non-wildcard tokens
> - **When** the candidate widening is attempted
> - **Then** the widening is rejected
> - **And** the line is treated as a parse failure
>   (`confidence = 0`, body retained, `parse_failures_total`
>   increments)
> - **And** an audit event with
>   `event_type = template_widening_rejected_degenerate` records
>   the rejection

> **Scenario RFC0001.3 — Tokenizer is Unicode whitespace only; punctuation stays in tokens**
> - **Given** a line `key=value, other=42` (no whitespace
>   adjacent to the punctuation)
> - **When** the line is tokenized
> - **Then** it produces two tokens (`key=value,` and `other=42`)
> - **And** no token boundary is introduced at `=`, `,`, `:`,
>   `;`, `[`, `]`, `(`, or `)`

> **Scenario RFC0001.4 — Confidence ratio = simSeq / threshold; decision boundary at 1.0**
> - **Given** a tenant with `threshold = 0.7`
> - **And** a line whose `simSeq` against the best candidate is
>   `0.7`
> - **When** the line is ingested
> - **Then** the emitted record's `confidence == 1.0`
> - **And** the line takes the clean-attach branch
> - **And** the same `simSeq` under `threshold = 0.5` would
>   yield `confidence == 1.4` (the ratio reframes
>   scale-invariantly across tenants)

> **Scenario RFC0001.5 — Bare `template_id = X` spans all versions of leaf X**
> - **Given** leaf X with versions 1, 2, 3 attached over time
> - **When** a query runs `where template_id = X`
> - **Then** the result includes rows attached against `(X, 1)`,
>   `(X, 2)`, and `(X, 3)`
> - **And** no alias resolution is involved (this is
>   by-construction, since `template_id` is stable across
>   widenings of one leaf)

> **Scenario RFC0001.6 — Bare `template_id = X` does NOT follow alias chains**
> - **Given** two distinct leaves X and Y that the alias index
>   records as semantically equivalent
> - **When** a query runs `where template_id = X`
> - **Then** only rows whose `template_id == X` are returned;
>   rows with `template_id == Y` are NOT included
> - **And** `where template_id.resolves_to(X)` (RFC 0002 §5.4) is
>   the explicit form that includes Y's rows

> **Scenario RFC0001.7 — Combined widening + type-expansion increments version twice and emits two events in order**
> - **Given** a leaf at version `V` where a single attach both
>   introduces a new wildcard slot AND introduces a
>   previously-unseen `ParamType` into an existing slot
> - **When** the attach completes
> - **Then** the leaf's `template_version == V + 2`
> - **And** the audit stream contains two events for this attach:
>   a `template_widened` event for the new wildcard, immediately
>   followed by a `template_type_expanded` event for the type
>   expansion (in that order)

> **Scenario RFC0001.8 — confidence_p50 and confidence_p01 are emitted as gauges**
> - **Given** a running miner with a non-empty `confidence`
>   histogram for some `(tenant_id, service)`
> - **When** the miner's meter is collected via an SDK in-memory reader
> - **Then** `confidence_p50` and `confidence_p01` (attributes
>   `tenant_id`, `service`) are present as gauges
> - **And** each value matches the corresponding quantile of the
>   same-attributed histogram (computed in-process on a short
>   ticker per §6.8)
>
> *(Pending the §6.8 dotted-semconv redesign: this scenario may be
> superseded if `confidence_p50` / `confidence_p01` become
> backend-derived quantiles over the exported histogram rather than
> in-process gauges. The change is a contract change to the §3.1.2
> mandatory set and is made under that redesign's own review, not
> the 2026-06-03 architecture amendment.)*

> **Scenario RFC0001.9 — `body_kind = Structured` short-circuits to a structured-template id**
> - **Given** an `OtlpLogRecord` whose `body` is
>   `Body::Structured(AnyValue)` (any non-`String` `AnyValue`
>   variant carried verbatim per RFC 0003 §6.4)
> - **When** the record is ingested
> - **Then** the §6.2 algorithm skips tokenize/mask/descend per
>   step 0 and allocates or reuses the structured-template id
>   for `(severity_number, scope_name, BodyKind::Structured)`
> - **And** the emitted record has `body_kind = Structured`
> - **And** the `body` column carries the OTLP-canonical JSON
>   encoding of that `AnyValue`, produced at Parquet-write time
>   (the in-memory record carries the decoded `AnyValue` itself
>   per the §6.4 amendment)
> - **And** `params` and `separators` are empty
> - **And** `confidence == 1.0` (the §6.1 sentinel)
> - **And** `lossy_flag == false`

> **Scenario RFC0001.10 — `time_unix_nano` is preserved verbatim from the wire**
> - **Given** an `OtlpLogRecord` with
>   `time_unix_nano = 1_715_700_000_000_000_000`
> - **When** the record is ingested and committed to Parquet
> - **Then** the emitted row has
>   `time_unix_nano == 1_715_700_000_000_000_000`
> - **And** a query
>   `WHERE time_unix_nano BETWEEN 1_715_600_000_000_000_000 AND 1_715_800_000_000_000_000`
>   returns the row
> - (Gates `docs/benchmarks.md` B1 — time-range queries — by
>   making the underlying column measurable.)

> **Scenario RFC0001.11 — `severity_number = 0` and `scope_name = None` are distinct key buckets**
> - **Given** four `OtlpLogRecord`s with identical
>   `body_kind = String` body, varying only in
>   `(severity_number, scope_name)` across
>   `(0, None)`, `(0, Some("lib.x"))`, `(9, None)`,
>   `(9, Some("lib.x"))`
> - **When** all four are ingested
> - **Then** four distinct `template_id`s are emitted, one per
>   key bucket
> - **And** no widening or merge ever coalesces the
>   `severity_number = 0` (`UNSPECIFIED`) bucket with any
>   specified-severity bucket
> - **And** no widening or merge ever coalesces the
>   `scope_name = None` bucket with any
>   `scope_name = Some(_)` bucket
> - (Locks the §6.1 explicit edge-case rules: `0 = UNSPECIFIED`
>   is a valid OTLP severity that gets its own bucket, and
>   absent scope is its own bucket.)

## 6. Proposed design

The Ourios miner in detail. This is the section that the
`ourios-miner` crate is implemented against; §5's Acceptance
criteria operationalise the commitments here, and §8 maps each
§5 scenario to the technique that tests it.

**Why §6 exists.** Published Drain (§3) and Drain3 (§4) do not
address the properties Ourios requires. Each row below is a gap
this section closes:

| Gap in published Drain | Ourios invariant that fills it | §6 subsection |
|---|---|---|
| No confidence score on a match | `[§3.1]` body retention below threshold | §6.3 |
| No audit trail on group merges | `[§3.1]` merge audit events | §6.4 |
| No inter-token whitespace preservation | `[§3.3]` bit-identical reconstruction | §6.6 |
| No per-parameter byte bound | `[§3.2]` param length limit, overflow to `body` | §6.5 |
| No multi-tenant scoping of the tree | `[§3.7]` per-tenant template trees | §6.1 |
| No template versioning / drift story | `[§3.5]`, hazard H5 | §6.7 |

### 6.1 Data model

> **Amendment 2026-05-13.** Section rewritten to align the record
> schema with the OTLP `LogRecord` shape — the project's stated
> ingest contract per `docs/glossary.md` (entry **OTLP**: *"we do
> not invent our own format"*). The investigation that surfaced
> the gap is
> [`docs/architecture/otlp-log-format.md`](../architecture/otlp-log-format.md).
> The pre-amendment schema treated logs as raw text strings; the
> amended schema treats every log as a structured OTLP record from
> the moment it enters the system. §6.2's algorithm and its ingest
> signature were aligned to this amendment in a companion edit
> the same day (see §6.2's amendment note below): the `body.kind`
> fork is at the top of the algorithm, the descent step
> incorporates the §6.1 template-key tuple, and the
> `MinerCluster::ingest` signature now takes a structured
> `OtlpLogRecord` rather than a raw `&str`.

The miner emits one record per ingested OTLP `LogRecord`. The
record shape mirrors the wire shape of OTLP logs (the
`opentelemetry-proto` `LogRecord` plus its inherited Resource and
InstrumentationScope context) plus the miner-derived columns that
template mining produces.

#### Record columns

The record carries three groups of columns. The OTLP-derived
group preserves the structured shape the wire promised; the
miner-derived group is what this RFC introduces; the
reconstruction group exists only when the body was mineable
(`body.kind = String`).

**Identity and partitioning:**

| Field | Rust type (informal) | Source | Purpose |
|---|---|---|---|
| `tenant_id` | `TenantId` | derived from `Resource.attributes` | Multi-tenant scoping `[§3.7]`; default rule below |
| `template_id` | `u64` | miner-allocated | Cluster-wide unique; see "Template identity" |
| `template_version` | `u32` | miner-allocated | Increments on widening; see "Template version" |

**OTLP-derived columns** (faithful to `opentelemetry-proto`):

| Field | Rust type (informal) | OTLP source | Purpose |
|---|---|---|---|
| `time_unix_nano` | `u64` | `LogRecord.time_unix_nano` | Event time at source; `0` = unknown. Required for thesis-gate B1 (time-range queries) |
| `observed_time_unix_nano` | `Option<u64>` | `LogRecord.observed_time_unix_nano` | Collector observation time |
| `severity_number` | `u8` | `LogRecord.severity_number` | OTLP `SeverityNumber`: `0` = `UNSPECIFIED` (a valid OTLP value for records that omit severity), `1..=24` = TRACE..FATAL with sub-levels. Part of the template key (see below); `0` is a distinct key value — UNSPECIFIED records cluster together, never with TRACE/INFO/etc. |
| `severity_text` | `Option<String>` | `LogRecord.severity_text` | Source's original severity string |
| `scope_name` | `Option<String>` | `InstrumentationScope.name` | Library/module emitter; part of the template key (see below) |
| `scope_version` | `Option<String>` | `InstrumentationScope.version` | Drift / debugging |
| `attributes` | `Vec<KeyValue>` | `LogRecord.attributes` | Per-occurrence structured context |
| `dropped_attributes_count` | `u32` | `LogRecord.dropped_attributes_count` | Truncation indicator |
| `resource_attributes` | `Vec<KeyValue>` | `Resource.attributes` | Source identity (`service.name`, `host.*`, etc.) |
| `trace_id` | `Option<[u8; 16]>` | `LogRecord.trace_id` | Trace correlation |
| `span_id` | `Option<[u8; 8]>` | `LogRecord.span_id` | Trace correlation |
| `flags` | `u32` | `LogRecord.flags` | Lower 8 bits = W3C trace flags |
| `event_name` | `Option<String>` | `LogRecord.event_name` | Identifier for structured-event records |

**Body and miner-derived reconstruction:**

| Field | Rust type (informal) | Source | Purpose |
|---|---|---|---|
| `body_kind` | `BodyKind` | derived from `LogRecord.body` | Discriminator: `String` \| `Structured` (see "Body representation") |
| `body` | `Option<Bytes>` | `LogRecord.body` | When `body_kind = Structured`: the OTLP-canonical JSON encoding of the `AnyValue` (see "Body representation" for the canonical-encoding rule). When `body_kind = String` lossy: the original line bytes. When overflow: per §6.5. |
| `params` | `Vec<Param>` | from masking | One entry per `<*>` slot. Always empty when `body_kind = Structured` |
| `separators` | `Vec<Separator>` | from tokenize | `tokens.len() + 1` entries. Always empty when `body_kind = Structured` |
| `confidence` | `f32` | miner-derived | `simSeq / threshold` at attach time. `1.0` (sentinel) when `body_kind = Structured` |
| `lossy_flag` | `bool` | miner-derived | True iff `reconstruct(record) ≠ ingested_body_bytes` is possible. Always `false` when `body_kind = Structured` (the verbatim `body` column is the source of truth) |

Where:

- `Param` = `{ type_tag: ParamType, value: Bytes }`. `ParamType`
  is one of `IP, UUID, NUM, HEX, TS, PATH, STR, OVERFLOW`. `STR`
  is the unmasked-wildcard fallback — used when a slot was
  created by template widening of a previously-fixed literal
  token (the literal itself becomes the param value); `OVERFLOW`
  carries `(length: u32, sha256_prefix: [u8; 8])` instead of the
  original value (§6.5). `params.len() == count(<*> in template)`,
  always (in the `body_kind = String` branch); §6.2 enforces
  this when a widening introduces new wildcard slots.
- `Separator` is a small inline byte string (typically 1–3 bytes
  in practice). Encoding in Parquet is an implementation detail
  that does not affect this RFC.
- `KeyValue` mirrors the OTLP `KeyValue` message: a `key:
  String` and a `value: AnyValue`. `AnyValue` is a discriminated
  union over `string | bool | int | double | bytes | array | kvlist`.
  Storing `AnyValue` faithfully in Parquet (rather than flattening
  to a string) is what keeps query expressions like
  `attributes["client.address"] = "10.0.0.1"` typed.
- `BodyKind` is a two-variant enum (`String`, `Structured`) — not
  the full `AnyValue` discriminator. The body column carries the
  encoded `AnyValue` payload; `body_kind` is the cheap routing
  flag the query planner uses to decide whether reconstruction is
  defined for this row.

#### Body representation (`AnyValue` handling)

OTLP's `LogRecord.body` is `AnyValue` — string, bool, int, double,
bytes, array, or kvlist. The spec is explicit (Logs Data Model
§Body): *"Body MUST support AnyValue to preserve the semantics of
structured logs emitted by the applications."* Real OTel emitters
send structured Body routinely, not just text.

Ourios distinguishes two body shapes at ingest:

- **`body_kind = String`** — `LogRecord.body` is `AnyValue::String`.
  The miner runs the §6.2 algorithm over the unwrapped string:
  tokenize, mask, descend the tree, attach to or create a leaf.
  `params`, `separators`, `confidence`, `lossy_flag` are populated
  per the existing semantics.
- **`body_kind = Structured`** — `LogRecord.body` is any other
  `AnyValue` variant (kvlist, array, int, double, bool, bytes).
  The miner does **not** run the §6.2 algorithm. The body is
  encoded canonically (see *Canonical encoding* below) and
  stored in the `body` column; no template is mined, no
  `params`/`separators` are emitted. `template_id` is allocated
  per the *Template-key composition* rule below — for this branch
  the key is `(severity_number, scope_name,
  BodyKind::Structured)`, so all structured-Body records sharing
  a `(severity, scope)` share one `template_id`. The leaf the id
  points at carries the `Structured` marker and an empty
  `body_template`. `confidence = 1.0` (sentinel),
  `lossy_flag = false` (the canonically-encoded body is
  authoritative; nothing is reconstructed from a template).

This is the **conservative default**. It preserves the structural
content of the body (the canonical-encoding rule below makes
`[§3.3]` reconstruction well-defined for the structured branch:
`stored_bytes ↔ AnyValue` is bidirectional and deterministic),
it avoids inventing template structure for arbitrary `AnyValue`
trees, and it sidesteps the spec ambiguity of "what is *the*
template for `{"msg": "x", "user_id": 42}`." A future opt-in
**mine-inner-field** mode (e.g., mine `body.kvlist["msg"]` as
the line if present) is a configurable knob, not the default;
that decision lives with the maturity-stage move from `red` →
`green` once corpus evidence informs which inner-field
conventions are worth specifying.

A third path — **render-to-string + mine** (canonicalise
structured Body to JSON-ish text and run it through the §6.2
mining algorithm) — was rejected because mining over the JSON
serialisation produces token templates that depend on the
serialiser's whitespace and field-ordering choices, which is
both fragile (changing serialisers shifts every template) and
defeats the §3.3 reconstruction guarantee for any record where
the original wire form was protobuf rather than JSON. Storing
the canonical encoding (without mining over it) is different
from this rejected path: storage is faithful, it just doesn't
get a template extracted.

**Canonical encoding for `body_kind = Structured`.** The `body`
column carries the **OTLP-canonical JSON encoding** of the
`AnyValue` per the OTLP specification's HTTP/JSON binding (the
proto3 JSON mapping with OTLP-specific overrides — e.g.,
`trace_id`/`span_id` as hex strings, `bytes` as base64).
Receivers that take input via OTLP/gRPC (protobuf wire format)
decode the `AnyValue` and re-serialise to canonical JSON before
storing; receivers that take input via OTLP/HTTP+JSON canonicalise
the incoming bytes (proto3 JSON allows a small amount of
latitude — field ordering, whitespace, `int64` as string vs
number — which canonicalisation removes). Without a canonical
rule, the `lossy_flag = false` promise for the structured branch
is unmeetable: two receivers handling the same logical
`AnyValue` could produce different stored bytes, and queries
that join records by body content would silently miss matches.
The OTLP-canonical JSON form makes the round-trip
`stored_bytes ↔ AnyValue` well-defined and provider-neutral.

#### Template-key composition

A template's identity (the discriminator the Drain tree uses to
decide *"is this the same template?"*) depends on the body shape:

- **`body_kind = String`** — key tuple is
  `(severity_number, scope_name, masked_body_tokens)`.
- **`body_kind = Structured`** — key tuple is
  `(severity_number, scope_name, BodyKind::Structured)`. All
  structured-Body records sharing a `(severity_number, scope_name)`
  share one `template_id`. This intentionally forfeits
  structured-body shape clustering — the rationale is that the
  structured-Body branch's value comes from the faithful
  preservation of `attributes` and the canonically-encoded
  `body`, not from grouping similar `AnyValue` shapes.
  Operators who need shape-level clustering can opt into a
  future `body_shape_fingerprint` column (a stable hash over
  the `AnyValue`'s structural skeleton — kvlist key-set, nested
  shape, leaf-type sequence; values ignored) as a reserved
  extension; the gate for adding it is "we have a concrete
  consumer," not "it might be useful."

The bullet rationale below applies to both branches:

- **`severity_number` is part of the key** because `INFO` and
  `ERROR` versions of the same body text are semantically distinct
  events. *"user logged in"* at INFO is a routine signal; *"user
  logged in"* at ERROR is an alarm (or an emitter bug) — collapsing
  them to one `template_id` would surface either as the other on
  query, which is a `[§3.1]` "no silent merges" violation in
  disguise. The OTLP-spec-valid `severity_number = 0`
  (`UNSPECIFIED`) is a distinct key value, not coalesced with
  any specified severity.
- **`scope_name` is part of the key** because the same body text
  emitted from two different instrumentation scopes
  (`myapp.login` vs `myapp.checkout`) describes two different
  events. The scope is the OTel-canonical "which code path
  emitted this," directly analogous to the package/logger name in
  traditional logging frameworks. Records with no scope
  (`scope_name = None`) cluster as their own `(severity, None)`
  bucket.
- **`resource_attributes` are NOT part of the key.** They identify
  *who* sent the record (service, host, k8s pod), not *what*
  event was emitted. The `tenant_id` derivation (below) already
  encodes the partition decision over Resource. Folding Resource
  into the template key would explode template cardinality
  proportionally to the deployment fleet size without adding
  semantic discrimination — the same `myapp.login` template from
  two replicas of `service.name = api` is the same template.
- **`event_name` is not in the key today** but is reserved as a
  candidate addition. RFC 0001 stays at the OTLP-canonical
  severity+scope key; promoting `event_name` into the key is a
  follow-up RFC patch once corpus evidence justifies it.

The Drain tree's implementation of this tuple (extra prefix
levels above length-N, tuple-keyed leaf lists, separate trees per
`(severity, scope)`, etc.) is §6.2 implementation territory and
may be revisited based on cardinality observations from the
corpus benchmark. The RFC pins only the semantic key.

#### Tenant derivation

`tenant_id` is derived **per `ResourceLogs` group**, not per OTLP
export batch. Each `ResourceLogs` carries its own
`Resource.attributes`, and a single OTLP export can contain
multiple `ResourceLogs` groups from different sources — so one
export can route records to multiple tenants. The derivation
runs once per inherited Resource; the resulting `tenant_id`
applies to every `LogRecord` under that `ResourceLogs` group
(across all its `ScopeLogs`), and the receiver fans the records
out into per-tenant streams.

The default per-Resource rule:

```
tenant_id := resource.attributes["service.name"]   if present
          ?: <operator-required fallback rule>
```

`service.name` is the conventional OTel unit of "what application
emitted this," and it maps directly onto Ourios's per-tenant
template-tree partitioning (`[§3.7]`). Operators with a different
multi-tenant model (per-namespace, per-customer-id-attribute,
composite of multiple attributes) configure an alternative rule;
the receiver does not invent a tenant identity that the operator
hasn't declared.

If a `ResourceLogs` group's Resource resolves to no tenant under
either the default rule or the operator's fallback, the receiver
rejects the **entire export batch** with a controlled error (no
panic, no silent assignment to a "default" tenant; the sender
sees the failure and either fixes its emitter or its deployment).
Per-Resource rejection within an otherwise-valid batch is **not**
supported in this RFC — the all-or-nothing failure mode is
simpler to reason about for the sender, and OTLP's batch-level
acknowledgement model fits all-or-nothing more naturally than
partial-success. The receiver-side specification of this
rejection path (and any future opt-in for partial acceptance)
lives in RFC 0003 — OTLP receiver (forthcoming).

#### Template identity

`template_id` is a cluster-wide unique monotonic `u64` (with each
tenant seeing a monotonic subsequence), allocated when a new leaf
is created and never reused or reassigned. The id space is shared
across tenants so that the same `u64` value never refers to two
different leaves; the per-tenant subsequence guarantee preserves
`[§3.7]` by making each tenant's allocation order observable in
isolation. Cross-tenant *content* identity is intentionally not
guaranteed — two tenants emitting the structurally identical
template (same `(severity_number, scope_name, masked_body_tokens)`
tuple) will have different `template_id`s, so a `template_id`
alone never links structurally-equivalent templates across
tenants. (The `u64` value itself is cluster-wide unique, per the
previous paragraph; what is not guaranteed is that *the same
template* across two tenants resolves to the same id.) This
preserves `[§3.7]` (per-tenant template trees) by construction;
cross-tenant analytics that need content identity (deduplication
across tenants for storage savings, shared template dashboards)
are an opt-in concern and are not provided by the miner. A future
`template_fingerprint` side column may carry a canonical content
hash over `(severity_number, scope_name, masked_body_tokens)` for
opt-in cross-tenant use; the gate for adding it is "we have a
concrete consumer," not "it might be useful."

**Template version.** `template_version` starts at 1 when the
template is created and increments by 1 on every widening event:
either a new wildcard slot opens (a previously fixed token at
position `i` becomes `<*>`), or an existing wildcard's typed
parameter set changes (e.g. a `<NUM>` slot starts seeing `<STR>`
values). To detect the second case, every leaf carries — alongside
its template — a `slot_types: Vec<HashSet<ParamType>>` indexed by
wildcard slot, recording every `ParamType` observed in that slot.
A type expansion is the addition of a `ParamType` to one of these
sets. The pair `(template_id, template_version)` uniquely
identifies one structural state of a template. Queries against
`template_id = X` return all versions; queries against
`(template_id, template_version) = (X, V)` return only the named
state. The DSL surface is RFC 0002's concern, not this RFC's, but
the data model must support both.

**Why two integers and not a content hash.** A content hash makes
identity global by construction; in a multi-tenant backend that is
a tenant-isolation leak rather than a feature. A content hash also
makes `template_version` redundant — once the canonical template
string changes, the hash changes, so versioning collapses into
alias-mapping between hashes. Per-tenant monotonic ints with an
explicit version field are smaller in the Parquet column, easier to
reason about under `[§3.7]`, and keep `(template_id,
template_version)` as a meaningful compound key.

### 6.2 Algorithm

> **Amendment 2026-05-13.** Rewritten to take a structured OTLP
> `LogRecord` rather than a raw `&str`, in line with the §6.1
> amendment. The algorithm now opens with the `body.kind` fork
> from §6.1's *Body representation*: `AnyValue::String` runs the
> Drain mining steps (the prior algorithm, preserved verbatim
> below); every other `AnyValue` variant short-circuits to the
> structured emit per §6.1's *Template-key composition* fork.
> Step 3's descent now incorporates `(severity_number,
> scope_name)` into the tree key, again per §6.1 — the
> implementation choice (extra prefix layers, tuple-keyed leaf
> lists, separate trees per `(severity, scope)`) stays in §6.2
> as the algorithm's responsibility, but the semantic key is
> pinned by §6.1. The ingest signature on `MinerCluster` becomes
> `ingest(record: &OtlpLogRecord)`; pre-amendment callers were
> `ingest(tenant_id, raw: &str)`.

The miner sees an already-tenant-resolved `(tenant_id,
record: OtlpLogRecord)` pair. The receiver (RFC 0003) is
responsible for resolving `tenant_id` per `ResourceLogs` and
fanning records into per-tenant streams before the miner
sees them; §6.1's *Tenant derivation* pins that contract.

For each ingested OTLP `LogRecord`:

```
0.  match record.body.kind:

      AnyValue::String(s):
          # Continue with the Drain mining algorithm in steps
          # 1–5 below, treating `s` as the `L_raw` of the prior
          # spec. body_kind = String.

      AnyValue::Bool | Int | Double | Bytes | Array | KVList:
          # Structured short-circuit per §6.1 *Body
          # representation*. The miner does NOT run the Drain
          # mining steps. body_kind = Structured.
          encoded = canonicalise_to_otlp_json(record.body)
              # OTLP-canonical JSON encoding per the OTLP HTTP/JSON
              # binding (proto3 JSON mapping with OTLP-specific
              # overrides — hex trace_id/span_id, base64 bytes).
              # For records arriving over OTLP/gRPC the receiver
              # decodes protobuf and re-serialises here; for
              # records arriving over OTLP/HTTP+JSON it
              # canonicalises the incoming bytes (whitespace,
              # field-ordering, int64-as-string normalisation).
              # Without canonicalisation the lossy_flag = false
              # promise is unmeetable — see §6.1 for the why.
          template_id = allocate_or_reuse_structured_template_id(
              record.severity_number,
              record.scope_name,
          )
              # Per §6.1 *Template-key composition*, the
              # structured-Body key is (severity_number,
              # scope_name, BodyKind::Structured). All structured
              # records sharing a (severity, scope) share one
              # template_id. The leaf the id points at carries the
              # `Structured` marker and an empty body_template.
          attach_structured(record, encoded, template_id,
                            confidence = 1.0,
                            lossy_flag = false)
              # confidence = 1.0 sentinel; lossy_flag = false
              # because the canonicalised body is authoritative,
              # nothing is reconstructed from a template.
          return

1.  L_tok, separators = tokenize(L_raw)
        # tokenize splits on Unicode whitespace only — every
        # codepoint matching `char::is_whitespace()` (ASCII space,
        # tab, CR, LF, plus the broader Unicode whitespace classes
        # U+0085, U+00A0, U+1680, U+2000–U+200A, U+2028, U+2029,
        # U+202F, U+205F, U+3000). Every other byte (including
        # punctuation such as `=`, `:`, `,`, `;`, `[`, `]`, `(`,
        # `)`) stays inside a token; structured separators are the
        # masking layer's responsibility (§4.2 / step 2). The
        # captured whitespace runs go into `separators` so that
        # reconstruction (§6.6) is byte-identical.
        # On failure (malformed UTF-8, embedded NUL, line longer
        # than max-line-bytes): emit a parse-failure record and
        # increment parse_failures_total. Skip the rest.
        # Note: an empty-after-whitespace string (the AnyValue
        # carries `""` or only whitespace) is not a parse failure
        # — it has zero tokens and the miner short-circuits with
        # the cluster's `NO_TEMPLATE` sentinel rather than
        # descending the tree. The pre-amendment cluster code
        # already routes this case; the spec just records it.

2.  L_masked, typed_params = mask(L_tok)
        # mask applies the configured masking rules in order;
        # any token matching a rule is replaced with its type
        # tag (e.g. <IP>) and the original bytes are pushed
        # into typed_params with that tag. Unmasked tokens
        # remain literal.

3.  parent = tree.descend(record.severity_number,
                           record.scope_name,
                           len(L_masked),
                           L_masked[0..d-1])
        # Per §6.1 *Template-key composition*, the discriminator
        # for "is this the same template?" is the tuple
        # (severity_number, scope_name, masked_body_tokens).
        # Step 3 incorporates severity_number and scope_name into
        # the descent key alongside the masked-token prefix used
        # by published Drain. The implementation may layer extra
        # prefix levels above the length-N node, key leaf lists
        # by (severity, scope), or maintain separate trees per
        # (severity, scope) — the choice is cardinality-driven
        # and revisitable from corpus observations. The
        # severity_number = 0 (UNSPECIFIED) and scope_name = None
        # cases are valid distinct key positions; they cluster as
        # their own buckets, never coalesced with any specified
        # severity or named scope.
        # if a node along the path does not exist, create it.

4.  candidate = argmax over leaf in parent.leaves of
                  simSeq(L_masked, leaf.template)
    if candidate is None:
        # no leaves under parent yet; create one. Creation does not
        # emit an audit event — `template_count` already reflects
        # leaf allocation, and §6.4 reserves the audit stream for
        # widening events whose semantics need cross-referencing.
        leaf = new Leaf(template = L_masked)
        parent.leaves.push(leaf)
        # On fresh-leaf creation the template is L_masked verbatim,
        # so every <*> in it came from mask(); params == typed_params.
        attach(L_masked, typed_params, separators, leaf,
               confidence = 1.0, lossy_flag = false)
        return

5.  similarity = simSeq(L_masked, candidate.template)
    confidence = similarity / threshold

    if similarity >= threshold:
        # clean or lossy attach; widen the template if needed.
        # widen() returns:
        #   widened           — the new template (existing fixed
        #                       positions that mismatched L_masked
        #                       become <*>)
        #   new_wildcards     — the set of positions that just
        #                       became <*> (the audit payload)
        widened, new_wildcards = widen(candidate.template, L_masked)
        if new_wildcards > 0:
            candidate.template = widened
            candidate.version += 1
            emit_audit(event_type = template_widened,
                       template_id = candidate.id,
                       old_version, new_version = candidate.version,
                       positions_widened = new_wildcards.positions,
                       ...)
            merges_total.inc()

        # Build the params array. One entry per <*> in the (possibly
        # just-widened) template, in template order. For each slot:
        #   - if the slot existed before this attach AND mask() emitted
        #     a typed_params entry for it, use that entry verbatim.
        #   - if the slot is a fresh wildcard from this widening (the
        #     position held a literal token in candidate.template before
        #     the widen call), the original literal at that position in
        #     L_tok is captured as { type_tag: STR, value: L_tok[pos] }.
        # Without this step §6.1's "one entry per <*> slot" invariant
        # is violated and §6.6's reconstruct() has no value to insert
        # at the freshly-widened position.
        params = build_params(candidate.template, typed_params,
                              L_tok, new_wildcards)

        # Type-expansion: if any wildcard slot now sees a typed param
        # whose type tag is not already in that slot's observed-type
        # set, widen the slot's type set, bump the version, and
        # audit. The leaf carries `slot_types: Vec<HashSet<ParamType>>`
        # alongside its template (data model in §6.1).
        new_types = update_slot_types(candidate, typed_params)
        if not new_types.is_empty():
            candidate.version += 1
            emit_audit(event_type = template_type_expanded,
                       template_id = candidate.id,
                       old_version, new_version = candidate.version,
                       slots_expanded = new_types,
                       ...)

        attach(L_masked, params, separators, candidate,
               confidence,
               lossy_flag = false)  # §6.6: lossy_flag is set only on
                                    # tokenizer/preprocessing failure,
                                    # not on confidence < 1.0
        return

    if similarity >= floor:
        # lossy zone: the line is "close" but doesn't meet
        # threshold. Create a new leaf rather than force-merging.
        # Body retention is unconditional in this branch.
        leaf = new Leaf(template = L_masked)
        parent.leaves.push(leaf)
        # As in the candidate-is-None branch, the new leaf's template
        # is L_masked verbatim, so params == typed_params.
        attach(L_masked, typed_params, separators, leaf,
               confidence,
               lossy_flag = false,
               body = L_raw)  # forced body retention
        body_retention_ratio.observe(retained = true)
        return

    # similarity < floor: parse failure
    parse_failures_total.inc()
    emit_failure_record(L_raw, reason = "no candidate above floor")
```

Branching invariants:

- Step 0's structured short-circuit never enters the Drain
  mining steps (1–5). Structured-Body records do not widen, do
  not emit `template_widened` or `template_type_expanded` audit
  events, do not contribute to `merges_total`, and never carry
  `params`/`separators`. The structured branch's `[§3.1]`
  preservation is vacuous: no template merge happens, so no
  silent merge is possible.
- The tree only deepens on first observation of a
  `(severity_number, scope_name, length, prefix tokens)` shape
  (the §6.1 template-key tuple, anchored at this section's
  step 3).
- Leaves are split (new leaf created) when the best candidate is in
  the lossy zone; they are never split when the candidate is clean.
- A leaf is *widened* (wildcards introduced) when a clean attach
  would otherwise mismatch positions. Every widening emits an audit
  event (§6.4).
- A leaf's wildcard slot is *type-expanded* when an attach maps a
  typed parameter whose `ParamType` is not already in that slot's
  `slot_types[slot]` set. Type expansion increments
  `template_version` and emits a `template_type_expanded` audit
  event (§6.4).
- A single attach can trigger both wildcard-widening and
  type-expansion in the same leaf; in that case `template_version`
  increments twice and two audit events are emitted, in that
  order.
- The leaf's `template_version` only increments on widening or
  type-expansion, not on a clean attach. Structured-Body leaves
  are never widened or type-expanded; their `template_version`
  stays at 1 for the lifetime of the leaf.

### 6.3 Confidence scoring `[§3.1]`

`confidence = simSeq / threshold`. The ratio framing makes the
decision boundary land at `confidence == 1.0` regardless of the
configured threshold, which gives `confidence_p50` and
`confidence_p01` (`[§3.1]` required metrics) a stable interpretation
across tenants with different thresholds: the p01 value tells you
how close the *bottom 1% of attaches* are to the merge boundary.
A collapsing p01 means many lines are barely passing — a tuning
signal even though the threshold itself has not moved.

Three zones, with concrete defaults:

- `confidence ≥ 1.0` (i.e. `simSeq ≥ threshold`): **clean attach**.
  No body retention.
- `floor / threshold ≤ confidence < 1.0`: **lossy zone**. The line
  attaches to a freshly created leaf rather than being force-merged
  into the candidate (see §6.2 step 5). `body` is retained
  unconditionally; `lossy_flag` follows the §6.6 rule (set only on
  reconstruction failure, not on lossy zone alone — the body is
  available either way).
- `confidence < floor / threshold`: **parse failure**.
  `parse_failures_total` increments; the line is written to a
  failure record with the original bytes intact.

**Defaults.** `threshold = 0.7`, `floor = 0.4`. The threshold floor
is fixed by `[§3.1]` ("threshold ≥ 0.7, lowering requires an RFC,
not a config change"); the lossy-zone floor is a tuning knob
between threshold and 0. `floor = 0.4` matches the paper's reported
default threshold, on the reasoning that lines below the paper's own
bar are likely genuinely different events. Tuning the floor is a
per-tenant config decision; it is not load-bearing for any
invariant.

### 6.4 Merge policy `[§3.1]`

A *template widening* (per §6.2 step 5) is the operation that
`[§3.1]` calls a "merge." Every widening emits an audit event with
the schema:

```
{
  event_type: AuditEventType,  # enum:
                               #   template_widened
                               #   template_type_expanded
                               #   template_widening_rejected_degenerate
  tenant_id: TenantId,
  template_id: u64,
  old_version: u32,
  new_version: u32,
  old_template: String,        # canonical form, with <*> for wildcards
  new_template: String,
  triggering_line_hash: [u8; 16],  # blake3 of L_raw, for cross-ref
  triggering_line_sample: Option<String>,  # first 256 B of L_raw
  positions_widened: Vec<u16>, # token positions that became <*>
                               # (empty for template_type_expanded)
  slots_expanded: Vec<SlotExpansion>,
                               # slot index + newly added ParamType(s)
                               # (empty for template_widened)
  timestamp: SystemTime,
}
```

`event_type` is the field §6.7's drift-detection query filters on.
`merges_total` increments on every event whose `event_type` is
`template_widened` or `template_type_expanded` (the two structural
widenings); `template_widening_rejected_degenerate` is recorded but
does not increment `merges_total`. The audit stream is written to
the same WAL as the data records and ends up in a dedicated audit
Parquet file per tenant per compaction window (schema in
`ourios-parquet`'s RFC, not this one).

**Default policy: strict.** Widening is permitted whenever the
clean-attach path in §6.2 would otherwise mismatch positions. The
audit event is mandatory — no widening, of any reason, ever
proceeds without one. Code paths that would emit a widening without
emitting an audit event are blocked at PR review per `hazards.md`
H1.

**WAL durability ordering of audit events.** A single `attach`
may emit two audit events in order (RFC0001.7: `template_widened`
immediately followed by `template_type_expanded`) and one data
record. The contract this RFC requires from the future
`ourios-wal` RFC is an *ordering-plus-durability-barrier*: a
data record carrying `template_version = V` must not become
durable before every audit event justifying the leaf's
progression to V is durable. Crash recovery may then observe
some prefix of `[event_1, event_2, …, data_record]`, but never
a state in which the data record exists without the events that
caused its version stamp. Any framing strategy that satisfies
this — a composite multi-record frame, batched-fsync ordering, a
two-phase write-then-link, anything else — is acceptable; the
framing is `ourios-wal`'s choice, the ordering barrier is this
RFC's requirement. Without it, replay would bump
`template_version` fewer times than the in-memory leaf did and
the surviving data records would reference a version the audit
stream cannot substantiate.

**Degenerate template guard.** If a widening would leave the
template with zero non-wildcard tokens (the entire template becomes
`<*> <*> … <*>`), the widening is rejected, the line is treated as
a parse failure (`confidence = 0`, retain body, increment
`parse_failures_total`), and an audit event with `event_type =
template_widening_rejected_degenerate` records the rejection. A
fully-wildcard template provides no parsing value and would swallow
arbitrary lines.

### 6.5 Parameter handling `[§3.2]`

**Per-parameter byte limit.** Default 256 B, configurable up to
1 KiB (the `[§3.2]` ceiling). Above 1 KiB requires an RFC.

**Overflow behaviour.** When a parameter value (post-masking)
exceeds the configured limit, the parameter slot is replaced by an
`OVERFLOW` marker:

```
Param {
  type_tag: ParamType::OVERFLOW,
  value: encode(length: u32, sha256_prefix: [u8; 8]),
}
```

The original line `L_raw` is captured into the `body` column
unconditionally (overflow forces body retention, regardless of
`lossy_flag`). The 8-byte SHA-256 prefix lets queries
"find rows where this exact long parameter occurred" without
storing the long value in the columnar data. Reconstruction
honours overflow: `reconstruct(record)` falls back to `body` when
any param has `type_tag == OVERFLOW`.

**Telemetry.** Two metrics for `[§3.2]` and hazard H2:

- `params_overflow_total` (counter, attributes `tenant_id`,
  `service`): increments per overflow.
- `params_overflow_ratio` (gauge, attributes `tenant_id`,
  `service`): rolling overflow rate. Alert at `> 0.01` per service
  per `[§3.2]`.

### 6.6 Body reconstruction `[§3.3]`

**Capture, always.** Every successful tokenization in §6.2 step 1
populates the `separators` array with the bytes between adjacent
tokens (and the leading and trailing bytes of the line). The array
length is `tokens.len() + 1`. There is no whitespace heuristic and
no "is this whitespace trivial" decision — the bytes are captured
verbatim. Storage cost is bounded (typical separator is one space;
the array dictionary-encodes well in Parquet) and the
implementation has no fuzzy boundary that could decide to drop
bytes silently.

**Reconstruction function.**

```
fn reconstruct(record: &Record) -> Bytes {
    if record.lossy_flag {
        return record.body.expect("lossy implies retained body");
    }
    if record.params.iter().any(|p| p.type_tag == OVERFLOW) {
        return record.body.expect("overflow implies retained body");
    }
    let template = lookup(record.template_id, record.template_version);
    let mut out = BytesMut::new();
    out.extend_from_slice(&record.separators[0]);
    for (i, token) in template.tokens.iter().enumerate() {
        match token {
            Token::Fixed(s) => out.extend_from_slice(s),
            Token::Wildcard(slot) => {
                out.extend_from_slice(&record.params[slot].value)
            }
        }
        out.extend_from_slice(&record.separators[i + 1]);
    }
    out.freeze()
}
```

**`lossy_flag` semantics.** Set to `true` if and only if
reconstruction is not guaranteed to equal the ingested bytes:

- The tokenizer failed (malformed UTF-8 inside a token, embedded
  NUL, line exceeded the configured `max_line_bytes` cap before
  tokenization completed).
- A preprocessing rule explicitly rejected the line.

The lossy zone in §6.3 (low confidence) does **not** automatically
set `lossy_flag`: the body is retained either way, and
reconstruction from template + params + separators is still
expected to match. The flag is reserved for the cases where the
record genuinely cannot be reconstructed.

**Reader behaviour.** When rendering rows, the reader checks
`lossy_flag` first. For lossy rows it emits the `body` verbatim
with an explicit warning marker ("this row's body cannot be
reconstructed from the template; the displayed value is the
original bytes as ingested"). For non-lossy rows it calls
`reconstruct`. The reader never silently substitutes one for the
other.

**Property test.** For every row `r` in the corpus where
`r.lossy_flag == false`:

```
reconstruct(r) == ingested_bytes(r)
```

Failure is a build break, not a regression — `[§3.3]` and hazard
H7 both name this as the property test that gates merges.

### 6.7 Template versioning and drift `[§3.5]`, hazard H5

A template's structural state changes over time as widenings (§6.4)
and parameter-type expansions accrue. Each change increments
`template_version` and emits an audit event of type
`template_widened` or `template_type_expanded`.
`template_version_changes_total` counts these.

**Two distinct cross-cutting questions.** "Same leaf, different
structural snapshots" and "different leaves that mean the same
thing" are separate problems, with separate query forms:

- *Cross-version (within one leaf).* A leaf's `template_id` is
  stable across every widening of that leaf (§6.1, "Template
  identity"); only `template_version` advances. So a literal
  predicate `where template_id = X` already returns rows from
  every version of leaf X by construction — no alias resolution
  required. To pin to a single structural snapshot, query
  `where (template_id, template_version) = (X, V)`.
- *Cross-alias (across leaves).* When a deploy changes a log line
  enough that the miner allocates a new leaf instead of widening
  the existing one, the operator has two `template_id`s for what
  is semantically the same template. Resolving "all rows for the
  thing X represents" then requires walking an alias set that
  spans leaves. RFC 0002 §5.4 exposes this as
  `where template_id.resolves_to(X)`; bare `where template_id = X`
  does **not** follow alias chains.

The data model in §6.1 supports both shapes: cross-version is free
because `template_id` is stable across widenings; cross-alias is
served by a separate alias index that maps a representative
`template_id` to the equivalence class of `template_id`s the
operator (or a future inference layer) considers semantically the
same template.

**Alias index lifecycle.** Cross-alias is structurally distinct
from cross-version: widening is intra-leaf and increments
`template_version` (one `template_id`, several versions);
aliasing is inter-leaf and groups `template_id`s the miner allocated
separately (different `template_id`s, the operator asserts they
mean the same thing). `template_widened` events therefore do **not**
populate the alias index — they live on the cross-version axis. The
alias index has no creation event in this RFC: the candidate
writers (operator-driven, automatic-inference, deferred entirely)
and the shape of the write API they would expose are open
questions in §9. Until those questions resolve, RFC 0002's
`template_id.resolves_to(X)` operates against an alias index
whose contents are produced out of band; this RFC does not
specify how.

**Drift detection as a first-class query.** "Templates that gained
a new version in the window `[t1, t2]`" is a query against the
audit event stream:

```
SELECT template_id, MIN(old_version), MAX(new_version),
       COUNT(*) AS widening_count,
       MIN(timestamp), MAX(timestamp)
FROM template_audit
WHERE event_type IN ('template_widened', 'template_type_expanded')
  AND timestamp BETWEEN $t1 AND $t2
GROUP BY template_id
ORDER BY widening_count DESC
```

(SQL shown for spec clarity; the user-visible form is the RFC 0002
DSL, not raw SQL — see hazard H6.) Operators use this query after
deploys to spot templates whose structure changed; a sudden cluster
of `template_widened` events correlated with a deploy timestamp is
exactly the H5 detection signal.

### 6.8 Telemetry `[§3.1]`, §6.3

> **Amendment 2026-06-03.** Telemetry export is realigned from a
> Prometheus client/scrape model to the **OpenTelemetry SDK** (the
> maintainer direction recorded against RFC 0009 §3.6 and the roadmap
> §5 note). This amendment fixes the *export architecture* and the
> Prometheus-era terminology (registry → meter provider, scrape →
> OTLP push, labels → attributes) throughout §§6.8–6.9 and the §5
> scenarios. It deliberately does **not** rename the metrics: the
> identifiers in the table below stay as-is, pending a dedicated
> dotted-semconv redesign that converts them to the
> `ourios.miner.*` scheme, adds them to the `semconv/registry/`
> weaver registry alongside the compaction set (RFC 0009 §3.6), and
> resolves the instrument-type and quantile questions that are
> genuine contract changes to the §3.1.2 mandatory set — including
> whether `confidence_p50` / `confidence_p01` remain in-process
> gauges (RFC0001.8) or become collector-/backend-derived quantiles
> over the exported histogram, the OTLP-native idiom. That redesign
> moves the mandatory-set contract under its own review rather than
> riding this architecture change.

#### Export architecture (OTel SDK + OTLP)

Metrics are instrumented through the OpenTelemetry **meter API** and
exported via the OTel SDK's **OTLP metric exporter** (push, over OTLP
to a collector / endpoint). There is no `prometheus` client crate and
no `/metrics` scrape endpoint; any Prometheus compatibility is a
downstream collector concern, not Ourios's.

The dependency split follows the standard OTel layering so the heavy
SDK and transport crates do not leak into every library:

- **Instrumented crates** (`ourios-miner`, `ourios-parquet`,
  `ourios-ingester`, `ourios-querier`) depend only on the lightweight
  `opentelemetry` **API** crate and resolve instruments through
  `global::meter("ourios.<subsystem>")`. No SDK, no OTLP, no
  transport dependency in a library crate.
- A new **`ourios-telemetry`** crate owns the heavy deps — the
  `opentelemetry_sdk` and `opentelemetry-otlp` crates (the upstream
  package names, underscore and hyphen respectively) plus the OTLP
  transport.
  It exposes an `init()` that builds the OTLP push `MeterProvider`
  (periodic-reader export, interval configurable), installs it as the
  process-global provider, and returns a guard whose `shutdown()`
  flushes pending metrics on exit. The binary (`ourios-server`) calls
  `init()` once at start-up; benches and integration tests call the
  same entry point or substitute an in-memory reader. Adding this
  crate extends the `CLAUDE.md` §7 target layout; the new-crate
  commitment is blessed here, in this RFC, per §7's rule.

Dimensions are OTel **attributes**, not Prometheus labels, and OTel
splits them in two: **resource attributes** identify the telemetry
producer and are set once on the `MeterProvider`; **data-point
attributes** vary per measurement. Ourios's own identity —
`service.name = ourios-<role>` (e.g. `ourios-ingester`, `ourios-querier`,
matching the role the `ourios-telemetry` crate initialises the provider
for; with `service.version`, etc.) — is a **resource attribute**: per
the semantic conventions it MUST be set once on the provider's
`Resource` and MUST NOT be repeated on individual data points.

The per-measurement dimensions in the table below — among them
`tenant_id`, the originating **service** of the ingested logs, and
per-metric dimensions like `event_type` — are **data-point
attributes**. A single ingester multiplexes many tenants and many
source services, and `[§3.1]` / `[§3.2]` require per-`(tenant,
service)` breakdowns — notably the §6.5 / H2.2 *per-service* overflow
alert — which a single producer-level resource attribute could not
provide. The `service` dimension here is the *log's source* service
(the value §6.1's tenant derivation reads), **distinct from Ourios's
own `service.name`** — it must not reuse that reserved resource key.
The table below shows the current attribute names (`tenant_id`,
`service`); like the metric names, they are converted to the
namespaced `ourios.*` dotted-semconv scheme by the deferred redesign
(which fixes the exact key for the source-service dimension) — not
here.

OTel's metric model is **collect-on-read**: a reader / exporter sees
the data points produced during a collection cycle, not a registry of
instruments, and a synchronous instrument that has recorded no
measurement contributes no data point. To preserve §3.1.2's
"full mandatory set is exposed" guarantee even at zero traffic, the
miner **emits an initial data point for each mandatory instrument at
init**, so every metric in the table below appears in the first
collection cycle regardless of traffic. The per-kind mechanism is an
implementation detail of the instrumentation slice: a zero `add()`
seeds additive instruments (counters / up-down-counters) without
distorting anything, and observable instruments report through their
callback; histograms are seeded so they surface **without** a
synthetic `record(0)` polluting their distribution. Verification is
therefore by *collecting the metric stream* (an SDK in-memory reader
in tests), not by enumerating registered instruments.

The metrics enumerated in `[§3.1]` are mandatory. Full set (names and
instrument kinds pending the dotted-semconv redesign noted above; the
dimensions shown are exported as **attributes**, not labels):

| Metric | Instrument kind | Attributes | Source invariant / hazard |
|---|---|---|---|
| `template_count` | gauge | `tenant_id` | `[§3.1]` |
| `merges_total` | counter | `tenant_id`, `event_type` | `[§3.1]`, H1 |
| `confidence` | histogram | `tenant_id`, `service` | `[§3.1]`, §6.3 |
| `confidence_p50` | gauge | `tenant_id`, `service` | `[§3.1]` |
| `confidence_p01` | gauge | `tenant_id`, `service` | `[§3.1]` |
| `body_retention_ratio` | gauge | `tenant_id` | `[§3.1]`, `[§3.3]` |
| `parse_failures_total` | counter | `tenant_id`, `service` | `[§3.1]` |
| `params_overflow_total` | counter | `tenant_id`, `service` | `[§3.2]`, H2 |
| `params_overflow_ratio` | gauge | `tenant_id`, `service` | `[§3.2]`, H2 |
| `template_version_changes_total` | counter | `tenant_id` | `[§3.5]`, H5 |
| `miner_latency_seconds` | histogram | `tenant_id` | hot-path budget (D1) |

**`confidence_p50` and `confidence_p01`.** The `confidence`
histogram is the source of truth; the two gauges are convenient
named views derived from it in-process. The miner recomputes them
on a short ticker (default 10 s, configurable; the cost is one
quantile evaluation over the histogram per tenant per service per
tick — negligible relative to the hot path) and caches the value
between ticks so a metric export cycle never blocks on
recomputation. The gauges exist so alerting rules and runbooks can
name them directly per `[§3.1]` rather than spelling out a
`histogram_quantile(...)` expression at every reference.

The histogram bucket boundaries are tuned to straddle the decision
boundary at 1.0 (see §6.3): default buckets
`[0.1, 0.3, 0.5, 0.7, 0.9, 0.95, 1.0, 1.05, 1.2, 1.5, 2.0, +Inf]`.

### 6.9 Persistence and recovery

**Hot path.** The per-tenant tree lives in process memory on the
ingester. Tree operations (descend, simSeq, attach, widen) are
hot-path; persistence does not happen synchronously per line.

**Durability via WAL replay.** `[§3.4]` (WAL-before-ack) means
every line that reached the miner is in the WAL before the
ingester acknowledged it. The tree state is therefore *derivable*
from the WAL: a cold start with no snapshot replays the WAL in
order through the miner and reconstructs the trees. This is
correct but slow at scale; the snapshot mechanism is an
optimisation on top.

**Replay mode.** Cold-start replay re-walks `attach`, `widen`, and
`expand_slot_types` against the same code path live ingest uses.
Doing so naively would re-fire every counter increment, every
histogram observation, and every gauge update for the entire
replay window, polluting steady-state metrics for the post-restart
horizon (a 10-minute replay on a high-volume tenant could shift
`merges_total` by orders of magnitude in a few seconds). The miner
therefore runs in **replay mode** until the WAL cursor reaches the
live tip: domain events are processed and tree state is mutated
exactly as in live ingest, but updates to the §6.8 metrics are
suppressed (counters do not increment, histograms do not observe,
gauges retain their previous value or, if the miner has never
served live traffic, their zero / empty initialisation value). The
instruments' init-seeded data points keep the full §6.8 set visible to
the exporter during replay — suppressing the *update* path leaves each
metric at its seeded / last value, so §3.1.2's "full set exposed"
invariant holds across replay without recording replay-window
measurements. A single `wal_replay_progress` gauge
(attribute `tenant_id`, value: fraction of the tenant's replay
window completed in `[0.0, 1.0]`) is exposed during replay so
operators can see the cold-start curve and confirm replay finished.
This metric is replay-only and is not part of the §3.1 mandatory
set; it is documented here, not in §6.8's table.

**Snapshot mechanism (direction).** Periodically, the per-tenant
trees are serialised to a snapshot artefact. Recovery on ingester
restart loads the most recent snapshot per tenant and replays the
WAL from the snapshot's high-water mark. The serialisation format
includes a leading version byte; readers that encounter an unknown
version fall back to full WAL replay rather than misinterpreting
the bytes.

The three sub-questions — *target store* (object storage vs local
disk vs both), *cadence* (per N lines, per N minutes, per WAL
segment), and *scope* (per-tenant per-snapshot vs cluster-wide
rolling) — are deferred to §9. The direction is committed; the
parameters are not.

**Migration.** When the in-memory data model in §6.1 changes (new
field, retired field, semantic change), the snapshot format's
version byte increments. Old snapshots are read-compatible only if
the change is additive (new optional fields tolerated). For
breaking changes, snapshots from the prior version are discarded
and the tree is rebuilt from WAL replay. `[§3.5]`'s schema-change
discipline applies: the change goes through an RFC.

## 7. Alternatives considered

Alternatives to Drain itself, evaluated as primary algorithms.
Each is rejected for the reason given; some have a possible
secondary role noted.

### Spell (LCS-based online parser)

Spell uses longest-common-subsequence to compare a new line against
existing templates. Per-line cost is O(template_count × line_length)
without depth bounding, which is several orders of magnitude
slower than Drain's O(d) tree walk at the template counts we
expect (10²–10⁴ per tenant). LCS also makes parameter positions
ambiguous on lines where the same token recurs, because the LCS
alignment can shift; Drain's positional matching gives unambiguous
parameter slots. Rejected as the primary algorithm.

### IPLoM (iterative partitioning)

IPLoM does three passes over the entire log, each splitting
clusters by a different criterion (token count, position, token
uniqueness). This requires the full log up front and is offline by
design. Rejected as the primary algorithm. *Possible secondary
role:* a periodic offline reconciliation pass could use IPLoM to
detect template fragmentation that Drain's online structure
missed (e.g. two leaves that should have been one because their
discriminating token was spurious). This is a follow-up RFC topic,
not a §6 commitment.

### LenMa (length-based clustering)

LenMa groups lines by token-count length, then finds templates
within each length group via a similarity-based second pass. The
length-only initial grouping is close to Drain's first level, but
the absence of the token-prefix tree leads to more spurious merges
within a length group (any two same-length lines are candidates,
not just same-length-and-same-prefix lines). Drain's tree is a
strict refinement of LenMa's grouping. Rejected as the primary
algorithm — Drain dominates on the same workload.

### LogPPT / LILAC / LLM-based parsers

Transformer-based parsers achieve higher accuracy on benchmark
corpora (LogPAI scores) but require model inference per line. At
the D1 hot-path budget (≥ 100k lines/s/core), per-line transformer
inference is infeasible without specialised hardware that
contradicts §1's "single Rust binary" framing. Rejected as the
primary algorithm. *Possible secondary role:* offline labeling-aid
on the `testdata/corpus/` to bootstrap a labeled set for
confidence calibration; or as a periodic reconciliation pass
similar to IPLoM. Both are deferred to follow-up RFCs.

### Offline clustering (e.g. nightly hierarchical agglomerative)

Quality is high; latency is unacceptable. Logs ingested at 14:00
would not be queryable until the next clustering window completes.
This contradicts §2's online motivation. Rejected as the primary
algorithm. *Possible secondary role:* the same reconciliation
pass mentioned under IPLoM and LLM-based could use offline
clustering to validate Drain's online output and surface drift; a
follow-up RFC if and when reconciliation becomes a real concern.

## 8. Testing strategy

Mapping to `[§6.2]`. Each technique below names the §5 scenarios
it operationalises; the test code carries the matching id in a doc
comment per `docs/verification.md` §2.3 so `grep -R "H1.1" .`
resolves bidirectionally between RFC and tests.

- **Unit tests** for tree operations: `tokenize`, `mask`,
  `descend`, `simSeq`, `widen`, `attach`, `build_params`. Each
  operation tested in isolation against fabricated inputs.
  *Covers:* RFC0001.3 (tokenizer whitespace-only),
  RFC0001.4 (confidence ratio + decision boundary),
  RFC0001.7 (combined widening + type-expansion in one attach).

- **`proptest`** for §6.6 reconstruction: for every generated line
  shape (length, separator distribution, masking outcome),
  `reconstruct(mine(line)) == line` or `mine(line).lossy_flag ==
  true`. Property failure blocks merge.
  *Covers:* H7.1, H7.4, §3.3.1.

- **Corpus tests** on `testdata/corpus/` (fixed, anonymised; see
  `docs/benchmarks.md` §1): assert bounds on `template_count`,
  `merges_total`, reconstruction accuracy, parameter overflow
  rate. Regressions are build failures, not warnings.
  *Covers:* H1.1 (login/logout corpus arm), H7.1 (corpus arm).

- **Confidence calibration test**: on a labelled subset of the
  corpus, verify the three-zone classification in §6.3 against the
  human labels.
  *Covers:* H1.2.

- **Merge-audit assertion** (negative + positive): no widening or
  type-expansion completes without a matching audit event, and
  fresh-leaf creation does *not* emit one. Runs on every corpus
  pass and on the synthetic widening fixtures.
  *Covers:* H1.3, H5.1, H5.2, RFC0001.1 (negative — no event on
  creation), RFC0001.2 (rejection event for degenerate widening),
  RFC0001.7 (event ordering arm).

- **Multi-tenant isolation** (negative test): interleave lines from
  two synthetic tenants through a single `MinerCluster`; assert that
  templates mined under tenant A never appear in tenant B's tree
  and vice versa. Implements `docs/benchmarks.md` E2.
  *Covers:* §3.7.1, §3.7.2.

- **Per-`ResourceLogs` tenant derivation** (miner-side stub): assert
  that when records carrying distinct derived `tenant_id`s arrive
  in the same ingest sequence, each lands in its derived tenant's
  tree. The receiver-side test — that the wire-decode layer
  actually derives `tenant_id` per `ResourceLogs.resource` rather
  than per `ExportLogsServiceRequest` — is owned by RFC 0003 (see
  RFC 0003 §6.3); RFC 0001 owns only the miner-side contract.
  *Covers:* §3.7.3.

- **OTLP-aligned template-key tests**: hand-curated `OtlpLogRecord`
  fixtures exercising the §6.1 *Template-key composition* tuple.
  Assert that varying only `severity_number` produces distinct
  `template_id`s, varying only `scope_name` produces distinct
  `template_id`s, the `severity_number = 0` (`UNSPECIFIED`) and
  `scope_name = None` edge buckets are each their own key value,
  and `body.kind != AnyValue::String` short-circuits per §6.2
  step 0 with the §6.1 sentinel `confidence = 1.0`,
  `lossy_flag = false`. The `time_unix_nano` round-trip is a
  small unit test against the §6.1 record schema.
  *Covers:* H1.4, H1.5, RFC0001.9, RFC0001.10, RFC0001.11.

- **Drift detection test**: ingest a corpus where a template
  deliberately drifts mid-stream; assert that the drift query in
  §6.7 returns the drifted template within the expected window.
  *Covers:* H5.3.

- **Crash recovery test (snapshot + WAL replay)**: SIGKILL the
  ingester between snapshot writes; assert that recovery
  reconstructs the same tree state that was acknowledged before
  the kill. Also corrupt the snapshot's leading version byte and
  assert WAL fallback. This is `[§3.4]`'s crash-recovery test
  extended to cover the miner's persistence layer.
  *Covers:* §3.5.1, §3.5.2.

- **Configuration tests**: assert default values and the rejection
  of out-of-bounds settings at startup.
  *Covers:* §3.1.1 (default threshold = 0.7),
  §3.2.1 (default param byte limit = 256),
  §3.2.2 (limit > 1 KiB rejected).

- **Metric collection test**: collect the miner's meter
  (`global::meter("ourios.miner")`) of a freshly-initialised miner via
  an SDK in-memory reader and assert the collected stream contains
  every §6.8 metric name (each present via its init-seeded data
  point), with the instrument kinds and attributes in §6.8's table,
  and that the `confidence_p50` / `confidence_p01` gauges track the
  same-attributed `confidence` histogram quantiles.
  *Covers:* §3.1.2, RFC0001.8.

- **Data-model contract tests**: small unit tests against the
  `template_id` query semantics that RFC 0002's DSL compiles to.
  These cover the cross-version vs. cross-alias distinction at the
  data-model layer; the DSL surface itself is tested in RFC 0002.
  *Covers:* RFC0001.5, RFC0001.6.

- **Reader behaviour test**: assert the §6.6 reader emits the
  `body` column verbatim (with the warning marker) for
  `lossy_flag = true` rows, and calls `reconstruct()` for the
  rest. The reader never silently substitutes one for the other.
  *Covers:* H7.3.

- **Overflow-path tests**: synthesize a parameter exceeding the
  configured byte limit; assert the `OVERFLOW` marker, forced
  body retention, and metric increments. Wire the alert-rule
  fixture for the >1% rate trigger.
  *Covers:* H2.1, H2.2.

- **Tokenizer-failure tests**: feed lines with embedded NULs,
  malformed UTF-8, and over-cap lengths; assert the parse-failure
  path retains the body and sets `lossy_flag = true`.
  *Covers:* H7.2.

- **Benchmark (`criterion`)**: per-line miner latency (target:
  median ≤ 10 µs/line on the §1 hardware baseline), ingest
  throughput (target: ≥ 100k lines/s/core, per `docs/benchmarks.md`
  D1). No §5 scenario; satisfies thesis-gate D1 directly at the
  *Validated* stage.

## 9. Open questions

Decisions explicitly deferred. Each must be resolved before this
RFC's status flips to `accepted`.

**Persistence (from §6.9).**

- [ ] Snapshot **target store**: object storage (S3-compatible)
      only, local disk only, or both with a cache hierarchy?
      Object storage matches `[§3.6]`; local-disk-only sacrifices
      durability. The likely answer is "both, with object storage
      as truth," but the cache rules need spelling out.
- [ ] Snapshot **cadence**: per N lines, per wall-clock window,
      per WAL segment rotation, or composite? The right choice
      depends on the WAL segment size (RFC 0003) and on the
      acceptable cold-start replay budget.
- [ ] Snapshot **scope**: one snapshot artefact per tenant per
      cadence point, or one cluster-wide snapshot containing all
      tenants? Per-tenant is cleaner under `[§3.7]`; cluster-wide
      may be cheaper for the long tail of tiny tenants.

**Algorithm tuning (open until corpus exists).**

- [ ] Floor default 0.4 — confirm against the corpus. If the
      lossy zone is too wide (many lines retained that "should
      have" been parse failures), tighten; if too narrow (too
      many parse failures on lines a human would accept),
      loosen. This is per-tenant tunable; the question is the
      out-of-the-box default.
- [ ] Tree depth `d`. Paper default 4; Drain3 default 4. Open
      question: do any of our representative corpora benefit from
      `d = 3` or `d = 5`?
- [ ] Max children per node. Drain3 caps at 100; the cap acts as
      a safety against unbounded fan-out from a bad masking rule.
      Confirm 100 is right for our corpora, or motivate a
      different number.

**Edge cases.**

- [ ] Lines that contain a literal `<*>` (the wildcard sentinel
      we use in template strings) — escape on tokenize, or
      replace with a non-collision character (e.g. U+E000)?
- [ ] Multi-line log entries (stack traces). Paper assumes
      single-line. Ourios position: deferred to RFC TBD on the
      OTLP receiver, since multi-line reassembly happens before
      the miner sees the line.

**Multi-tenancy and operational lifecycle.**

- [ ] **Tenant lifecycle.** §3.7 commits to per-tenant trees but
      does not name when a tree is allocated (lazily on first
      ingest? eagerly via a control-plane command?), nor whether
      tenants can be paused, evicted under memory pressure, or
      deleted. Likely deferred to a future operator-console RFC,
      but the bookend events (`TenantInitialised`, `TenantPaused`,
      `TenantDeleted`) need to exist somewhere before §3.7 is
      operationally complete.
- [ ] **Per-tenant fairness and back-pressure.** A noisy tenant can
      monopolise WAL bandwidth, blow up the tree, and starve
      well-behaved tenants. RFC 0001 has no rate-limit or
      back-pressure event in scope; this overlaps with the OTLP
      receiver's responsibility and likely lives in a future
      `ourios-ingester` RFC.
- [ ] **Alias index creation mechanism (from §6.7).** Three
      candidates surfaced: operator-driven (manual "these two
      leaves are the same"), automatic-inference (post-deploy
      heuristic that proposes aliases from `template_widened`
      bursts), or deferred entirely. The choice gates RFC 0002's
      `template_id.resolves_to(X)` semantics; until it resolves,
      the alias index has no specified write path.

**Cross-RFC contracts pending.**

- [ ] **Querier ↔ live template registry (from §6.6).**
      Reconstruction's `lookup(template_id, template_version)`
      is called by `ourios-querier`, which runs in a separate
      process from the ingester that owns the live tree.
      Candidates: querier reads snapshots from object storage
      (eventually consistent with live), querier asks the
      ingester via RPC at query time (couples query latency to
      ingester health), or templates ride a separate Parquet
      side-stream alongside records (eventually consistent, no
      RPC, new data plane). RFC 0002 needs the answer before its
      DSL can compile; this RFC names the seam.
- [ ] **Audit-event Parquet schema (from §6.4).** The Rust audit
      struct is specified in §6.4; the on-disk Parquet column
      layout for `template_audit` belongs to a future
      `ourios-parquet` RFC. The §6.7 drift query assumes the
      schema exposes `event_type`, `template_id`, `old_version`,
      `new_version`, and `timestamp` as columns suitable for
      predicate pushdown.

**Deferred to follow-up RFCs.**

- [ ] Reconciliation pass (IPLoM / offline clustering / LLM-based
      labeling) — if real-world drift turns out to be more than
      §6.4's online widening can handle, a periodic offline pass
      becomes interesting. RFC at that point.
- [ ] Cross-tenant `template_fingerprint` side column — only if a
      concrete consumer materialises (storage dedup across
      tenants, shared dashboards). Until then, do not add.

## 10. References

- He, P., Zhu, J., Zheng, Z., Lyu, M.R. "Drain: An Online Log
  Parsing Approach with Fixed Depth Tree." ICWS 2017.
  <!-- DOI / PDF link to be added once confirmed. -->
- Drain3: <https://github.com/logpai/Drain3> (specific commit
  pinned in this RFC at the Specified-gate PR).
- LogPAI logparser benchmark: <https://github.com/logpai/logparser>
- `CLAUDE.md` §§ 2, 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 4, 6.2,
  6.3, 6.6.
- `docs/hazards.md` H1, H2, H5, H7.
- `docs/benchmarks.md` C1, C2, C3, C4, D1, E1, E2.
- `docs/rfcs/0002-query-dsl.md` §5.4 (template primitives in the
  DSL surface; required to expose drift detection).
- `docs/verification.md` §§ 2, 3, 6 (the maturity model and the
  acceptance-criteria contract this RFC will inherit at the
  Specified gate).
- Future: `docs/architecture/miner.md` (this RFC graduates there
  on acceptance).
