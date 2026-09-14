---
name: rfc-check
description: Decide whether a proposed change to Ourios needs an RFC before implementation, which accepted RFCs it would amend, and whether the PR description addresses the invariants and hazards it touches. Use when planning a change, before opening a PR, when splitting a PR, or when asked "does this need an RFC?".
allowed-tools: Read, Grep, Glob, Bash(git diff:*), Bash(git log:*), Bash(git status:*)
metadata:
  adapted-from: huggingface/openenv .claude/skills/rfc-check
---

# RFC check

`CLAUDE.md` §5.1 states the rule; this skill makes it a repeatable triage
with a written verdict. The rule: any change that touches an architectural
pillar (§2), an invariant (§3) or a hazard (§4) needs an RFC before code.
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
   before the PR exists) **and** `git status --porcelain` for untracked
   files — a new RFC or source file is usually still untracked at this
   point, and `git diff --no-index /dev/null <file>` shows its content —
   then the diffs themselves; for a plan, the files and functions it names.
   List every crate touched.

2. **Apply the triggers.** Grep the touched code for the surfaces below and
   mark each trigger that applies. A trigger is "touched" if the diff changes
   behaviour behind it, not merely if it compiles against it.

   | Trigger | Where it lives | Verdict |
   |---|---|---|
   | Pillar — Parquet on-disk format, Drain-derived miner, DataFusion as the engine (§2) | `ourios-parquet`, `ourios-miner`, `ourios-querier` | **Required** |
   | Invariant §3.1 template merges, §3.2 `params` cardinality, §3.3 bit-identical reconstruction | `ourios-miner`; for §3.3 also the body decode boundary `ourios-core/src/otlp.rs` and the read-path renderer `ourios-querier/src/log_row.rs`, which must return retained bodies for lossy rows | **Required** |
   | Invariant §3.4 WAL-before-ack: ack ordering, fsync, checkpoint, truncation, rotation, recovery | `ourios-wal`, `ourios-ingester` commit/recovery/publish paths, `ourios-server/src/receiver.rs` (startup recovery and the post-recovery, rotation and shutdown `flush_then_snapshot` barriers) | **Required** |
   | Invariant §3.5 Parquet schema, §3.6 object storage as truth, §3.7 tenancy | `ourios-parquet`, storage, every tenant-bearing path | **Required** |
   | Every hazard section of `docs/hazards.md` — enumerate the file, do not assume the count. Today: H1 miner correctness → `ourios-miner`; H2 params cardinality → `ourios-miner` params limit, `ourios-parquet` overflow column; H3 WAL durability → `ourios-wal`, `ourios-ingester/src/receiver/commit.rs`, `ourios-server/src/receiver.rs`; H4 small files → `ourios-ingester/src/record_sink.rs`, compaction in `ourios-core/src/alias.rs` and `ourios-bench/src/store.rs`; H5 template schema evolution → `ourios-miner/src/snapshot.rs`, `ourios-querier/src/{alias_store,drift}.rs`; H6 DSL vs SQL → `ourios-querier/src/{dsl,plan,exec.rs}`; H7 reconstruction → as §3.3; H8 replication dedup → no surface yet, any replication proposal trips it | **Required** |
   | Wire contract: OTLP receiver behaviour, error mapping, query DSL surface, HTTP and MCP query endpoints | `ourios-ingester/src/receiver/*`, `ourios-core/src/otlp.rs` (the OTLP decode boundary, RFC 0003/0043), `ourios-querier/src/dsl` and `api.rs`, `ourios-server/src/{querier,mcp,visibility}.rs` (`visibility::reject` picks the 401/403/503 the query surfaces return), `ourios-serving` | **Required** if it changes what a client observes; a conformance fix that only makes existing behaviour spec-correct is **Recommended** (open an issue naming the spec clause) |
   | New crate | `Cargo.toml`, `crates/` | **Required** |
   | New or changed persisted layout — WAL segment/checkpoint/snapshot format, Parquet partition or object layout | `ourios-wal/src`, `ourios-parquet/src` (`parquet_io::object_key`), `ourios-ingester/src/record_sink.rs` (`object_key`), `ourios-miner/src/snapshot.rs` (the snapshot byte format and its version dispatch; `snapshot_store.rs` only persists it) | **Required** |
   | New config field on the deployment surface | `ourios-server/src/config`, `ourios-config`, Helm | **Recommended** |
   | Telemetry: new metric, log event or attribute *name* | anywhere | Not an RFC trigger by itself, but the name goes through the shared `ourios-semconv` registry — say so in the verdict. Span names are not registry-backed here; they follow the OTel semantic conventions for their kind |
   | Span lifecycle: adding, removing, renaming or re-scoping a span | `ourios-server`, `ourios-ingester`, `ourios-df-otel`, `ourios-telemetry` | **Recommended**, and cross-reference RFC 0038 §3.5 and RFC 0040, which specify the spans; **Required** if it contradicts either |
   | Bug fix, dependency bump, refactor preserving every public signature and every on-disk byte, test-only change, non-normative docs (guides, talks, benchmarks records) | — | **Not required** |
   | Process and contract documents: `CLAUDE.md` (its footer requires a `meta:` RFC), an *accepted* RFC's text (an amendment, per `docs/rfcs/README.md` §Relationship), `docs/hazards.md` | those files | **Required** for `CLAUDE.md` and accepted RFCs; **Recommended** for `hazards.md` |

3. **Cross-reference the accepted RFCs.** For each trigger marked, grep
   `docs/rfcs/` for the section that specifies that surface — the RFC's
   *design* section and its §5 acceptance criteria, wherever the design
   lives (RFC 0003's is §6, not §3) — and quote the sentence the change
   would contradict or extend. Read the `status:` frontmatter first and
   route every status `docs/rfcs/README.md` defines: only `accepted` is
   binding, so a contradiction with an *accepted* RFC's criterion is
   **Required** whatever the table says and the verdict must name the RFC
   and section it amends ("amends RFC NNNN §X" belongs in the new RFC's
   status note and §8); an overlap with a `drafted`, `specified`, `red`,
   `green` or `validated` RFC is reported as "coordinate with RFC NNNN",
   never as an amendment; `superseded` text is never cited as live — follow
   `superseded-by:` to the current RFC instead (RFC 0045 → RFC 0046 is the
   case in the tree); and `rejected` text is simply ignored, since a
   rejected RFC has no replacement to follow. This is the step that finds
   the hidden amendment before review does.

4. **Check the PR description** (when there is one). `CLAUDE.md` §4 makes it
   review-blocking: for every §3 invariant or §4 hazard the change touches,
   the description must say how the change preserves it. List the ones it
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
Amends: <one line per accepted RFC: RFC NNNN §X — "<quoted sentence>"> or "none found in docs/rfcs/"
Coordinate: <one line per pre-accepted RFC: RFC NNNN (status) — overlap> or "none"
PR description: <invariants/hazards touched> / <silent on: …> or "no PR yet"

Verdict: Not required | Recommended | Required
Because: <two sentences at most>
Gate: <"none" | "issue #… naming the spec clause" | "RFC NNNN at `red` before code; ships after `accepted` if it contradicts an accepted criterion">
Split: <"none" | "<part A> ships now; <part B> waits for the RFC">
```

## What it is not

It does not review the change and does not write the RFC. When the verdict
is Required, stop and say so; `docs/rfcs/README.md` has the template and
the maintainer flips the maturity.
