---
name: rfc-check
description: Decide whether a proposed change to Ourios needs an RFC before implementation, which accepted RFCs it would amend, and whether the PR description addresses the invariants and hazards it touches. Use when planning a change, before opening a PR, when splitting a PR, or when asked "does this need an RFC?".
allowed-tools: Read, Grep, Glob, Bash(git diff:*), Bash(git log:*), Bash(git status:*), Bash(gh pr diff:*), Bash(gh api -X GET:*)
metadata:
  adapted-from: huggingface/openenv .claude/skills/rfc-check
---

# RFC check

`CLAUDE.md` §5.1 states the rule; this skill makes it a repeatable triage
with a written verdict. The rule: any change that touches an architectural
pillar (§2), an invariant (§3) or a hazard (§4) needs RFC coverage before code: either an RFC that already specifies it (see step 3's covered-by outcome) or a new one.
Bug fixes, dependency bumps and internal refactors do not — **but a trigger
wins over an exemption**: a "bug fix" that changes behaviour behind a §3
invariant or §4 hazard is a contract change and needs the RFC; the
exemption covers only a fix that restores the contract an accepted RFC
already states (a conformance fix, see the wire-contract row). **If unsure,
assume RFC.**

Run it on a diff, a plan, or a PR. Never on nothing: the verdict has to
name files and sections.

## Steps

1. **Establish the change.** For a branch, `git diff --stat main...HEAD`,
   `git diff HEAD --stat` (staged and unstaged work, since triage runs
   before the PR exists) **and** `git status --porcelain --untracked-files=all` for untracked
   files (without `--untracked-files=all` a new directory collapses to
   one `?? dir/` line and its files never get diffed) — a new RFC or
   source file is usually still untracked at this point, and
   `git diff --no-index /dev/null <file>` shows its content —
   then the diffs themselves; for a plan, the files and functions it names.
   List every crate touched.

2. **Apply the triggers.** Grep the touched code for the surfaces below and
   mark each trigger that applies. A trigger is "touched" if the diff changes
   behaviour behind it, not merely if it compiles against it. The "where it
   lives" column is a **starting inventory, not an exhaustive owner list**:
   the mechanism is (a) grep the diff for the surface named in the trigger
   and (b) when the inventory lacks a mitigation, the `docs/hazards.md`
   headings and the owning RFC's design section are the source of truth.
   Paths are given at crate or module-directory granularity where a
   directory exists; a single file is named only where that file owns a
   contract.

   | Trigger | Where it lives | Verdict |
   |---|---|---|
   | Pillar — Parquet on-disk format, Drain-derived miner, DataFusion as the engine (§2) | `ourios-parquet`, `ourios-miner`, `ourios-querier` | **Required** |
   | Invariant §3.1 template merges, §3.2 `params` cardinality, §3.3 bit-identical reconstruction | `ourios-miner`; for §3.1's "every merge emits an audit event" also `ourios-core/src/audit.rs` (the wire-stable event contract), its Parquet form in `ourios-parquet/src/{audit_record_batch,audit_writer,audit_reader}.rs`, and `ourios-ingester/src/audit_sink.rs` (the audit buffer, write and retention), `ourios-parquet/src/audit_sink.rs` (`ParquetAuditSink::try_write` derives the audit partition and writes the event) and `ourios-ingester/src/publish.rs` (`PublishCoordinator::write_ordered` is what enforces audit-before-record); `ourios-config/src/lib.rs` (owns the similarity threshold/floor and `param_byte_limit` defaults and their validation); `ourios-core/src/record.rs` (`MinedRecord` body and `lossy_flag` contract); `ourios-parquet/src/{record_batch,reader}.rs` (enforce and decode retained bodies); for §3.3 also the wire decode `ourios-ingester/src/receiver/decode.rs` and the String/Structured body fork `receiver/materialize.rs`, the post-decode `OtlpLogRecord`/`Body` model `ourios-core/src/otlp.rs` and the read-path renderer `ourios-querier/src/log_row.rs`, which must return retained bodies for lossy rows | **Required** |
   | Invariant §3.4 WAL-before-ack: ack ordering, fsync, checkpoint, truncation, rotation, recovery | `ourios-wal`, `ourios-ingester/src/receiver/pipeline.rs` (`IngestPipeline`, the `Journal` append/sync boundary ahead of the ack), `ourios-ingester` commit/recovery/publish paths, `ourios-server/src/receiver.rs` (startup recovery and the post-recovery, rotation and shutdown `flush_then_snapshot` barriers) | **Required** |
   | Invariant §3.5 Parquet schema, §3.6 object storage as truth, §3.7 tenancy | `ourios-parquet` (for §3.5's reader contract, `reader.rs` and `decode.rs` own absent- and unknown-column compatibility, `ourios-querier/src/schema_adapt.rs` the RFC 0042 type-mismatch-as-absent rule), storage, every tenant-bearing path | **Required** |
   | Every hazard section of `docs/hazards.md` — enumerate the file, do not assume the count. Today: H1 miner correctness → `ourios-miner`; H2 params cardinality → `ourios-miner/src/overflow.rs` (the byte limit, the `Overflow` marker in `params` and the original bytes kept in `body`; there is no separate overflow column; structured bodies bypass that limit and are only partially guarded: `docs/hazards.md` lists the `structured_body_bytes` metric plus a per-service alert as a proposed mitigation, and today only the metric exists, recorded in `ourios-miner/src/cluster/mod.rs` and `metrics.rs` per RFC 0037, so a change there is not fully covered until the alert lands) and the per-column dictionary policy in `ourios-parquet/src/writer.rs`; H3 WAL durability → `ourios-wal`, `ourios-ingester/src/receiver/pipeline.rs` (the append/sync boundary ahead of the ack), `ourios-ingester/src/snapshot_store.rs`, `ourios-ingester/src/receiver/commit.rs`, `ourios-ingester/src/{recovery,publish,record_sink}.rs` (replay, audit-ordered publication and its barriers), `ourios-server/src/receiver.rs`; H4 small files → `ourios-ingester/src/record_sink.rs` (file/row-group cut), `ourios-server/src/main.rs` (constructs and runs the compactor role) and `ourios-server/src/config/resolve.rs` (`OURIOS_COMPACTION_ENABLED`, `compaction.interval_secs`), `ourios-server/src/config/file.rs` (`CompactionSection`: the YAML fields and their `${env:VAR}` substitution), `ourios-parquet/src/{writer,parquet_io}.rs` (row-group rotation threshold), `ourios-parquet/src/compaction/` (RFC 0009/0036 sealed-partition compaction), `ourios-ingester/src/compactor/` (the compactor role, backfill and erasure); H5 template schema evolution → `ourios-miner/src/cluster/` (the widen step, `template_version`, snapshot payload), `ourios-miner/src/{tree,snapshot}.rs`, `ourios-core/src/alias.rs` (alias-set semantics), `ourios-querier/src/{template_registry,template_map,alias_store,drift}.rs` (the read-path `(template_id, template_version) → tokens` fold and its RFC 0033 cached form); H6 DSL vs SQL → `ourios-querier/src/{dsl,plan}/` and `exec.rs`, `ourios-querier/src/{api,lib}.rs` (the public `QueryError` and its re-export) and the error scrubbing in `ourios-server/src/{querier,mcp}.rs`; H7 reconstruction → as §3.3; H8 replication dedup → no surface yet, any replication proposal trips it. The headings in `docs/hazards.md` are the source of truth; when a hazard names a mitigation this map lacks, follow the hazard | **Required** |
   | Wire contract: OTLP receiver behaviour, error mapping, query DSL surface, HTTP and MCP query endpoints | `ourios-ingester/src/receiver/` (`decode.rs` is the wire-decode boundary, `materialize.rs` the String/Structured body fork, RFC 0003/0043), `ourios-core/src/otlp.rs` (the post-decode `OtlpLogRecord`/`Body` model), `ourios-server/src/main.rs` (role startup: which listeners a role exposes), `ourios-server/src/receiver.rs` (listener wiring, gzip acceptance, auth-before-decode, service install), `ourios-querier/src/dsl` and `api.rs`, `ourios-server/src/{querier,mcp,visibility}.rs` (`visibility::reject` picks the 401/403/503 the query surfaces return), `ourios-querier/src/visibility.rs` (the RFC 0047 read-side contract: masked-column rejection, tenant and conversation predicates, row masking), `ourios-serving` | **Required** if it changes what a client observes; a conformance fix that only makes existing behaviour spec-correct is **Recommended** (open an issue naming the spec clause), and when a change is both — a conformance fix that alters a client-visible response — the conformance-fix branch wins |
   | New crate | `Cargo.toml`, `crates/` | **Required** |
   | New or changed persisted layout — WAL segment/checkpoint/snapshot format, Parquet partition or object layout | `ourios-wal/src`, `ourios-parquet/src` (`parquet_io::object_key`, `manifest.rs` for the per-partition `manifest.json` that names the live files), `ourios-ingester/src/record_sink.rs` (`object_key`), `ourios-ingester/src/compactor/` (erasure and backfill marker keys), `ourios-querier/src/template_map.rs` (the `template_map.v2.json.zst` object under each tenant's audit root: key, format version, tmp/rename/CAS publication), `ourios-miner/src/snapshot.rs` and `ourios-miner/src/cluster/` (snapshot codec and payload), `ourios-ingester/src/snapshot_store.rs` (the `<tenant>.snap` name, tenant-path encoding and `.tmp`/rename install protocol), `ourios-server/src/receiver.rs` (the snapshots root under `wal_root/snapshots`) | **Required** |
   | New config field or new config semantics | `ourios-config` (`MinerConfig`), `ourios-wal/src/lib.rs` (`WalConfig`), every other crate-local `*Config` type, `ourios-server/src/config` | Every field has an owning RFC and a classification: RFC 0004 §3.2–§3.5 give the miner's classification table and the closed-list rule (§3.3: a new knob in an invariant area is an invariant proposal that goes through the RFC process, not review; §3.5 the escalation path, a `meta:` RFC first if it relaxes a `CLAUDE.md` §3 invariant), RFC 0023 §3.1 and RFC 0050 §3.2 own the `MinerConfig` fields they added, RFC 0008 §6.9 classifies the WAL tunables (`root` is a location, not a tunable). A change to an existing field is checked against its owning RFC's classification; a new field or new semantics is **Required** |
   | Wiring an already-approved field onto the deployment surface | `ourios-server/src/config`, Helm, `${env:VAR}` substitution | **Recommended** |
   | Telemetry: new metric, log event or attribute *name* | anywhere | Not an RFC trigger by itself. An Ourios-owned name goes through the shared `ourios-semconv` registry — say so in the verdict; a standard OTel, GenAI or MCP attribute (`error.type` in `ourios-ingester/src/metrics.rs`, the `gen_ai.*`/`mcp.*` names RFC 0038 §3.6 takes from the upstream GenAI registry) follows its upstream convention and is deliberately not in the registry. Either way the name is checked against the OTel semantic conventions first. Span names are not registry-backed here: RFC 0038 §3.5 fixes the request-span names and RFC 0040 §3.3 names operator spans from `node.name()` (OTel has no convention for per-operator spans) while its `datafusion.operator.*` names are *attributes* and do go through the registry, so a new span name must match those RFCs first and OTel's `{action} {target}` guidance only where a convention exists |
   | Span lifecycle: adding, removing, renaming or re-scoping a span | `ourios-server`, `ourios-ingester`, `ourios-df-otel`, `ourios-telemetry`, `ourios-querier/src/stats.rs` (the `record_plan_spans` call) | **Recommended**, and cross-reference RFC 0038 §3.5, RFC 0039 (inbound propagation and the `/mcp` SERVER-span and tool-span parentage) and RFC 0040, which specify the spans; **Required** if it contradicts any of them |
   | Scope expansion past `CLAUDE.md` §1 "Is not": a metrics or traces backend, SIEM, a Loki/Mimir/ClickHouse clone, a managed service | anywhere | **Required**: §1 says the default is to reject the PR and capture the idea as an RFC, so the RFC comes first |
   | Bug fix, dependency bump, refactor preserving every public signature, every on-disk byte and every observable behaviour (ack ordering, tenant isolation, query semantics; a trigger above still wins), test additions and fixtures that leave every asserted behaviour and acceptance criterion unchanged, non-normative docs (guides, talks), and clerical or corrective result-only edits to `docs/benchmarks.md` that leave its gate definitions and acceptance criteria unchanged and report no degraded thesis gate (a result that degrades a gate is **Required**: README §Lifecycle spawns a tuning RFC per `benchmarks.md` §7) | — | **Not required**. Weakening, deleting or re-targeting a passing assertion is a `CLAUDE.md` §6.2 contract change: it needs explicit approval, and an RFC when the assertion is a §5 criterion |
   | Process and contract documents: `CLAUDE.md` (its footer requires a `meta:` RFC), an *accepted* RFC's text (`accepted` is terminal per `docs/rfcs/README.md` §Lifecycle, so its text changes only by a later RFC's amendment, a `meta:` RFC, successor-driven supersession (§Lifecycle: "a later RFC replaces part or all of this one. Both frontmatters are updated" — the successor RFC plus the banner and front-matter update in the superseded one), or §Lifecycle's regression rule: "A regression detected after `Validated` either reopens the RFC (if a criterion is invalidated) or spawns a tuning RFC per `benchmarks.md` §7"), `docs/hazards.md`, `docs/benchmarks.md` gate definitions and acceptance criteria (§Lifecycle's `validated` stage runs on them; the exemption row covers result-only edits), `docs/rfcs/README.md` (the RFC process `CLAUDE.md` §5.1 delegates to) and `docs/verification.md` (self-described as "the process spec") | those files | **Required** for `CLAUDE.md`, accepted RFCs, `docs/benchmarks.md` gate definitions or criteria, and a new hazard or a changed hazard contract (`docs/hazards.md` §Adding a new hazard requires a `meta:` RFC); **Recommended** for hazard wording that changes no mitigation, and for `docs/rfcs/README.md` and `docs/verification.md`: neither text requires a `meta:` RFC of itself, so a change to the process they define needs maintainer sign-off on the PR, and a `meta:` RFC only when it also changes `CLAUDE.md` §5 |

3. **Cross-reference the accepted RFCs.** For each trigger marked, grep
   `docs/rfcs/` for the section that specifies that surface; when the plan
   or PR names an RFC the tree does not hold yet (an open RFC PR), fetch
   the complete file from the PR's head branch with `gh api -X GET -H "Accept: application/vnd.github.raw+json" repos/<owner>/<repo>/contents/<path>?ref=<branch>` (without the raw media type the Contents API returns base64 in `.content`; the skill is read-only, so only the GET form is allowed) — `gh pr diff <n>` yields a patch, so use it only for patch context or a file the PR adds whole —,
   or take its status and the relevant text as input, and route it by
   status exactly like an in-tree RFC — `red`/`green`/`validated`/`accepted`
   and implemented by the change goes under `Covered by`, `drafted` or
   `specified` under `Coordinate` with the `Blocked` verdict, any other
   overlap with a live pre-accepted RFC under `Coordinate` (an accepted
   contradiction is `Amends`, `superseded` routes through its successor,
   `rejected` is ignored, as below) — rather than reporting "none" — the RFC's
   *design* section and its §5 acceptance criteria, wherever the design
   lives (RFC 0003's is §6, not §3) — and quote the sentence the change
   would contradict or extend. Read the `status:` frontmatter first, then
   honour inline supersession banners and dispositions inside an accepted
   RFC before quoting a sentence as live (RFC 0003 §6.3 carries a
   "Superseded by RFC 0046" banner while the file stays `accepted`; RFC
   0001 keeps a v1 paragraph marked superseded by its v2 amendment), and
   route every status `docs/rfcs/README.md` defines: only `accepted` is
   binding (README §Relationship to architecture docs: "An accepted RFC is
   a contract for how something will be built"), so a contradiction with an
   *accepted* RFC's design text or §5 criterion — a format or algorithm
   clause counts even when no criterion duplicates it — is **Required**
   whatever the table says and the verdict must name the RFC
   and section it amends ("amends RFC NNNN §X" belongs in the new RFC's
   status note and §8); an overlap with a `drafted`, `specified`, `red`,
   `green` or `validated` RFC is reported as "coordinate with RFC NNNN",
   never as an amendment; `superseded` text is cited only through its
   successor — supersession is partial here (README §Lifecycle: "a later
   RFC replaces part or all of this one"), so follow `superseded-by:`,
   read the successor's scope or fate statement (RFC 0046 §3.4 keeps
   RFC 0045's `Store::resolve` fix and `TenantId` opacity while removing
   the derivation) and treat explicitly kept clauses as live under the
   successor; and `rejected` text is simply ignored, since a rejected RFC
   has no replacement to follow. A change that merely *implements* an RFC
   — it is what RFC NNNN §X and its §5 criteria specify, and contradicts
   or extends nothing — is reported as "covered by RFC NNNN §X" **only
   when that RFC's status permits implementation**: `red`, `green`,
   `validated` or `accepted` (README §Lifecycle: implementation begins at
   Red). Then no new RFC is needed, the gate is that RFC's own ladder, and
   `Amends` stays reserved for text an accepted RFC would have to change.
   A direct implementation of such an RFC is the explicit exception to
   "coordinate", which is kept for a change that touches an in-ladder
   RFC's surface without implementing it — and for an implementation of a
   `drafted` or `specified` RFC, whose verdict is `Blocked (RFC NNNN[, RFC
   MMMM] not yet red)`: no new RFC, the existing RFC's red gate is unsatisfied, and
   `Coordinate` names it until it reaches `red`. Covered-by overrides the trigger
   table's verdict: a matched Required trigger whose behaviour the
   covering RFC's §5 already specifies yields `Not required (covered by
   RFC NNNN[, RFC MMMM])`. This is the step that finds
   the hidden amendment before review does.

4. **Check the PR description** (when there is one). Fetch it with
   `gh api -X GET repos/<owner>/<repo>/pulls/<n> --jq .body`, or take the
   body as input. `CLAUDE.md` §4 makes it
   review-blocking: for every §3 invariant and every hazard the change
   touches, the description must say how the change preserves it. §4 is
   the rule; the headings of `docs/hazards.md` are the list (it holds
   hazards §4's short list omits, H8 today), so check every matched
   heading, not only §4's. List the ones it
   touches and the ones the description is silent on.

5. **If the verdict is Required, say which ladder stage gates the code.**
   Per `docs/rfcs/README.md` §Lifecycle: `drafted` → `specified` → `red` →
   `green` → `validated` → `accepted`. Implementation may begin only at
   `red` (stubs exist and fail). A change that depends on a criterion of an
   RFC still `drafted` waits; if part of the change needs no decision, split
   it off and ship that part now — the maintainer's standing preference is
   to split.

## Output

Write exactly this, filled in:

```
RFC check — <branch or plan name>

Files: <list, grouped by crate>
Triggers: <each matched trigger, one line, with the file:line that trips it — or the planned file and symbol when the input is a plan>
Amends: <one line per accepted RFC: RFC NNNN §X — "<quoted sentence>"> or "none"
Covered by: <one line per RFC the change implements: RFC NNNN (status) §X, criteria <ids as the RFC writes them: `RFC<NNNN>.<m>`, `H1.1` or `§3.4.2`, per `docs/rfcs/README.md` §Required sections>> or "none"
Coordinate: <one line per pre-accepted RFC the change touches without implementing: RFC NNNN (status) — overlap> or "none"   (a direct implementation of a `red`/`green`/`validated`/`accepted` RFC goes under Covered by, not here; an implementation of a `drafted`/`specified` RFC stays here until its red gate)
PR description: <invariants/hazards touched> / <silent on: …> or "no PR yet"

Verdict: Not required | Not required (covered by RFC NNNN[, RFC MMMM]) | Blocked (RFC NNNN[, RFC MMMM] not yet red) | Recommended | Required   (the question is "is a new RFC or an amendment required?"; a `Covered by` result overrides the trigger ranking, so a change fully covered by an RFC's §5 is Not required with that RFC's ladder as the gate; Required only when a matched trigger has no covering RFC or accepted text must change; otherwise Required outranks Recommended outranks Not required)
Because: <two sentences at most>
Gate: <one line per gate: "none" | "RFC NNNN: blocked until its `red` gate" | "issue #… naming the spec clause" | "new RFC required (no number yet): <one-line scope>; implementation waits for its `red` gate" | "RFC NNNN: `red` is the implementation gate (a red-stage branch is landable), `green` the acceptance-test gate (all §5 criteria plus unit, property and corpus tests pass), `validated` the thesis gate (`benchmarks.md` §7 on representative corpora), `validated → accepted` the maintainer sign-off" (`docs/verification.md` §3) | "RFC NNNN amends RFC MMMM §X: code gates on NNNN's `red`; MMMM's criterion X is flipped when NNNN is `accepted`" (no rule in `docs/rfcs/README.md` or `docs/verification.md` treats an amendment differently from any other RFC)>
Split: <"none" | "<part A> ships now; <part B> waits for the RFC">
```

## What it is not

It does not review the change and does not write the RFC. When the verdict
is Required, stop and say so; `docs/rfcs/README.md` has the template and
the maintainer flips the maturity.
