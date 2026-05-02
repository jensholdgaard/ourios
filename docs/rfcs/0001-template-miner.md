---
rfc: 0001
title: Template miner (Drain-derived online log parsing)
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-04-24
supersedes: —
superseded-by: —
---

# RFC 0001 — Template miner

> **How to read this document.** §§1–4 are the design contract — the
> *what* and the *why*. §5 (Acceptance criteria) is reserved per
> `docs/rfcs/README.md` and is added in the PR that moves this RFC
> from Drafted to Specified; while in Drafted the section is a
> placeholder. §6 is the precise specification the `ourios-miner`
> crate is implemented against; its opening paragraphs name the
> gaps between the published algorithm and a production miner that
> §6.1–§6.9 then close. §7 records the alternatives we evaluated
> and rejected.
>
> Cross-references to `CLAUDE.md` sections are in square brackets,
> e.g. `[§3.1]`, and name the invariant the section must preserve.

## 1. Summary

Ourios implements a Drain-derived online template miner (`ourios-miner`)
that converts each ingested log line into a structured record
`(tenant_id, template_id, template_version, params, separators, body?,
confidence, lossy_flag)`. The miner is per-tenant by construction
`[§3.7]`, uses a three-zone confidence model that retains the original
line in the lossy zone `[§3.1]`, audits every template widening
`[§3.1]`, captures inter-token separators in a parallel array so that
bit-identical reconstruction is the default rather than a property-test
exception `[§3.3]`, bounds parameter values at 256 B with overflow to a
side `body` column `[§3.2]`, and tracks template structural changes via
a monotonic `template_version` so that schema drift across deploys is a
first-class query rather than a silent count drop `[§3.5]`. The
compression target is 50–200× over raw bytes before any byte-level
codec runs.

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
  counters via callback. `replace` — Ourios exposes Prometheus
  metrics directly per §6.8, with names that match `[§3.1]`'s
  required set rather than Drain3's internal names.
- **Parameter masking after the fact.** Drain3 has utilities to
  retroactively mask params in already-clustered lines. `reject` —
  Ourios masks once, at ingest, deterministically. Retroactive
  masking would invalidate already-written Parquet files.

## 5. Acceptance criteria

*Reserved.* Per `docs/rfcs/README.md` ("Required sections"), every
RFC's §5 carries the normative `Given / When / Then / And`
scenarios that operationalise the commitments in §6. This RFC is
at the Drafted gate; the Acceptance criteria are added in the PR
that moves it to Specified. Scenario ids will follow the conventions in
`docs/verification.md` §2: `H1.x`, `H2.x`, `H5.x`, `H7.x` for the
hazards this RFC mitigates; `§3.1.y`, `§3.2.y`, `§3.3.y`, `§3.5.y`,
`§3.7.y` for the invariants it preserves; `RFC0001.x` for
design-internal scenarios with no invariant or hazard parent.

## 6. Proposed design

The Ourios miner in detail. This is the section that the
`ourios-miner` crate is implemented against; §5's Acceptance
criteria (added at the Specified gate) operationalise the
commitments here.

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

The miner emits one record per ingested line. The record shape is:

| Field | Rust type (informal) | Purpose |
|---|---|---|
| `tenant_id` | `TenantId` | Multi-tenant scoping `[§3.7]` |
| `template_id` | `u64` | Per-tenant monotonic; identifies the template |
| `template_version` | `u32` | Per-template monotonic; increments on widening |
| `params` | `Vec<Param>` | One entry per `<*>` slot, in order |
| `separators` | `Vec<Separator>` | `tokens.len() + 1` entries; reconstruction support |
| `body` | `Option<Bytes>` | Original line bytes; populated on lossy or overflow |
| `confidence` | `f32` | `simSeq / threshold` at attach time |
| `lossy_flag` | `bool` | True iff `reconstruct(record) ≠ ingested_bytes` is possible |

Where:

- `Param` = `{ type_tag: ParamType, value: Bytes }`. `ParamType` is
  one of `IP, UUID, NUM, HEX, TS, PATH, STR, OVERFLOW`. `STR` is
  the unmasked-wildcard fallback; `OVERFLOW` carries `(length:
  u32, sha256_prefix: [u8; 8])` instead of the original value
  (§6.5).
- `Separator` is a small inline byte string (typically 1–3 bytes
  in practice). Encoding in Parquet is an implementation detail
  that does not affect this RFC.

**Template identity.** `template_id` is a per-tenant monotonic
`u64`, allocated when a new leaf is created and never reused or
reassigned. Cross-tenant identity is intentionally not guaranteed
— two tenants emitting the structurally identical template will
have different `template_id`s. This preserves `[§3.7]` (per-tenant
template trees) by construction; cross-tenant analytics that need
identity (deduplication across tenants for storage savings, shared
template dashboards) are an opt-in concern and are not provided by
the miner. A future `template_fingerprint` side column may carry a
canonical content hash for opt-in cross-tenant use; the gate for
adding it is "we have a concrete consumer," not "it might be
useful."

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

For each ingested line `L_raw`:

```
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

2.  L_masked, typed_params = mask(L_tok)
        # mask applies the configured masking rules in order;
        # any token matching a rule is replaced with its type
        # tag (e.g. <IP>) and the original bytes are pushed
        # into typed_params with that tag. Unmasked tokens
        # remain literal.

3.  parent = tree.descend(len(L_masked), L_masked[0..d-1])
        # descend through root → length node → prefix nodes.
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
        attach(L_masked, typed_params, separators, leaf,
               confidence = 1.0, lossy_flag = false)
        return

5.  similarity = simSeq(L_masked, candidate.template)
    confidence = similarity / threshold

    if similarity >= threshold:
        # clean or lossy attach; widen the template if needed
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

        attach(L_masked, typed_params, separators, candidate,
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

- The tree only deepens on first observation of a `(length, prefix
  tokens)` shape.
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
  type-expansion, not on a clean attach.

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

- `params_overflow_total` (counter, labelled `tenant_id`,
  `service`): increments per overflow.
- `params_overflow_ratio` (gauge, labelled `tenant_id`,
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

**Alias semantics — by-default-broad, version-pinned-on-request.**
A query that says `template_id = X` is interpreted as "rows whose
`template_id == X`, any version" — i.e. follow the alias chain
across all versions. This is the common case (operators usually
mean "show me this template's events," not "show me this exact
structural snapshot of the template"). A query that says
`(template_id, template_version) = (X, V)` returns only rows from
that exact state.

The DSL surface for this is RFC 0002 §5.4 (`template_id.resolves_to(X)`
is the explicit cross-version form; bare `template_id = X` is the
implicit form). The data model in §6.1 supports both shapes
unambiguously.

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

The metrics enumerated in `[§3.1]` are mandatory. Full set:

| Metric | Type | Labels | Source invariant / hazard |
|---|---|---|---|
| `template_count` | gauge | `tenant_id` | `[§3.1]` |
| `merges_total` | counter | `tenant_id`, `event_type` | `[§3.1]`, H1 |
| `confidence` | histogram | `tenant_id`, `service` | `[§3.1]`, §6.3 |
| `body_retention_ratio` | gauge | `tenant_id` | `[§3.1]`, `[§3.3]` |
| `parse_failures_total` | counter | `tenant_id`, `service` | `[§3.1]` |
| `params_overflow_total` | counter | `tenant_id`, `service` | `[§3.2]`, H2 |
| `params_overflow_ratio` | gauge | `tenant_id`, `service` | `[§3.2]`, H2 |
| `template_version_changes_total` | counter | `tenant_id` | `[§3.5]`, H5 |
| `miner_latency_seconds` | histogram | `tenant_id` | hot-path budget (D1) |

`confidence_p50` and `confidence_p01` from `[§3.1]` are derived from
the `confidence` histogram via standard Prometheus quantile rules,
not separate metrics. The histogram bucket boundaries are tuned to
straddle the decision boundary at 1.0 (see §6.3): default buckets
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

Mapping to `[§6.2]`. Acceptance criteria scenario ids (added in the
Specified-gate PR) will be referenced from the corresponding test
doc-comments per `docs/verification.md` §2.3.

- **Unit tests** for tree operations: `tokenize`, `mask`,
  `descend`, `simSeq`, `widen`, `attach`. Each operation tested in
  isolation against fabricated inputs.
- **`proptest`** for §6.6 reconstruction: for every generated line
  shape (length, separator distribution, masking outcome),
  `reconstruct(mine(line)) == line` or `mine(line).lossy_flag ==
  true`. Property failure blocks merge.
- **Corpus tests** on `testdata/corpus/` (fixed, anonymised; see
  `docs/benchmarks.md` §1): assert bounds on `template_count`,
  `merges_total`, reconstruction accuracy, parameter overflow
  rate. Regressions are build failures, not warnings.
- **Confidence calibration test**: on a labelled subset of the
  corpus, verify the three-zone classification in §6.3 against the
  human labels.
- **Merge-audit assertion** (negative test): no widening completes
  without an audit event; assertion runs on every corpus pass.
- **Multi-tenant isolation** (negative test): interleave lines from
  two synthetic tenants through a single `MinerCluster`; assert that
  templates mined under tenant A never appear in tenant B's tree
  and vice versa. Implements `docs/benchmarks.md` E2.
- **Drift detection test**: ingest a corpus where a template
  deliberately drifts mid-stream; assert that the drift query in
  §6.7 returns the drifted template within the expected window.
- **Crash recovery test (snapshot + WAL replay)**: SIGKILL the
  ingester between snapshot writes; assert that recovery
  reconstructs the same tree state that was acknowledged before
  the kill. This is `[§3.4]`'s crash-recovery test extended to
  cover the miner's persistence layer.
- **Benchmark (`criterion`)**: per-line miner latency (target:
  median ≤ 10 µs/line on the §1 hardware baseline), ingest
  throughput (target: ≥ 100k lines/s/core, per `docs/benchmarks.md`
  D1).

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
