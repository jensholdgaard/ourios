---
name: rfc-check
description: Decide whether a proposed change to Ourios needs an RFC before implementation, which accepted RFCs it would amend, and whether the PR description addresses the invariants and hazards it touches. Use when planning a change, before opening a PR, when splitting a PR, or when asked "does this need an RFC?".
allowed-tools: Read, Grep, Glob, Bash(git diff:*), Bash(git log:*)
metadata:
  adapted-from: huggingface/openenv .claude/skills/rfc-check
---

# RFC check

`CLAUDE.md` §5.1 states the rule; this skill makes it a repeatable triage
with a written verdict. The rule: any change that touches an architectural
pillar (§2), an invariant (§3) or a hazard (§4) needs an RFC before code.
Bug fixes, dependency bumps and internal refactors do not. **If unsure,
assume RFC.**

Run it on a diff, a plan, or a PR. Never on nothing: the verdict has to
name files and sections.

## Steps

1. **Establish the change.** For a branch, `git diff --stat main...HEAD`
   and the diff itself; for a plan, the files and functions it names. List
   every crate touched.

2. **Apply the triggers.** Grep the touched code for the surfaces below and
   mark each trigger that applies. A trigger is "touched" if the diff changes
   behaviour behind it, not merely if it compiles against it.

   | Trigger | Where it lives | Verdict |
   |---|---|---|
   | Pillar — Parquet on-disk format, Drain-derived miner, DataFusion as the engine (§2) | `ourios-parquet`, `ourios-miner`, `ourios-querier` | **Required** |
   | Invariant §3.1 template merges, §3.2 `params` cardinality, §3.3 bit-identical reconstruction | `ourios-miner` | **Required** |
   | Invariant §3.4 WAL-before-ack: ack ordering, fsync, checkpoint, truncation, rotation, recovery | `ourios-wal`, `ourios-ingester` commit/recovery/publish paths | **Required** |
   | Invariant §3.5 Parquet schema, §3.6 object storage as truth, §3.7 tenancy | `ourios-parquet`, storage, every tenant-bearing path | **Required** |
   | Hazard #1–#7 (`docs/hazards.md`) | as listed there | **Required** |
   | Wire contract: OTLP receiver behaviour, error mapping, query DSL surface | `receiver/*`, `ourios-dsl`, `ourios-querier` HTTP/MCP | **Required** if it changes what a client observes; a conformance fix that only makes existing behaviour spec-correct is **Recommended** (open an issue naming the spec clause) |
   | New crate, new persisted layout, new config field on the deployment surface | `Cargo.toml`, `ourios-server/src/config`, Helm | **Required** (crate, layout); **Recommended** (config) |
   | Telemetry: new metric, span, log event or attribute *name* | anywhere | Not an RFC trigger by itself, but the name goes through the shared `ourios-semconv` registry — say so in the verdict |
   | Bug fix, dependency bump, refactor preserving every public signature and every on-disk byte, test-only change, docs | — | **Not required** |

3. **Cross-reference the accepted RFCs.** For each trigger marked, grep
   `docs/rfcs/` for the section that specifies that surface (the RFC's §3
   and its §5 criteria) and quote the sentence the change would contradict
   or extend. A change that contradicts an *accepted* RFC's criterion is
   **Required** whatever the table says, and the verdict must name the RFC
   and section it amends; "amends RFC NNNN §X" belongs in the new RFC's
   status note and §8. This is the step that finds the hidden amendment
   before review does.

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
Triggers: <each matched trigger, one line, with the file:line that trips it>
Amends: <RFC NNNN §X — "<quoted sentence>"> or "none found in docs/rfcs/"
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
