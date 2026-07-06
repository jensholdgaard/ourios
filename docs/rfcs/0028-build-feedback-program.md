---
rfc: 0028
title: Build-feedback program — test-harness consolidation and workspace decomposition
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-06
supersedes: —
superseded-by: —
---

# RFC 0028 — Build-feedback program: test-harness consolidation and workspace decomposition

## 1. Summary

Developer feedback latency is a first-order engineering constraint
("slow feedback is a development and velocity killer" — maintainer,
2026-07-06, with explicit precedence over feature work). This RFC
turns the measured build-cost profile (epic #382) into a program:

1. **Test-harness consolidation** — collapse the workspace's **104
   integration-test binaries** (ingester 31, querier 19, parquet 17,
   wal 11, server 9, miner 7, …) into ~1–3 harnesses per crate.
   Every binary links its crate's full dependency stack (DataFusion,
   tonic); link count dominates `cargo test` wall time, measured at
   57 s for `touch core → querier test binaries` before a single
   test runs. No new crates; test *names and assertions are
   preserved exactly* — files move under a harness root, nothing is
   weakened (CLAUDE.md §6.2).
2. **`ourios-core` decomposition** — split the fat hub along its
   fault line: pure data types (tenant, records, OTLP, audit,
   alias, confidence) stay in `ourios-core`; **`MinerConfig` and
   its validation move to a new `ourios-config`** crate (name
   bikesheddable). A `core` edit currently rechecks 9 crates
   (38 s); config churn — a frequent edit class — stops invalidating
   type-only consumers.
3. **Deferred-with-tripwire: `ourios-parquet` split**
   (reader/writer/compaction/store). Re-measure after (1); a
   parquet edit's 27 s / 5-crate fan-out may be acceptable once the
   link storm is gone. Splitting prematurely costs API churn across
   the RFC 0005 surface for unproven gain.
4. **cargo-nextest** for test execution (local + CI): per-test
   parallelism over the consolidated binaries, faster reruns,
   crisper failure output. Additive; `cargo test` keeps working.

Measured honestly: incremental *check* feedback is already fine
(17–38 s). The program targets the three verified sinks — link
count, branch-churn invalidation (worktrees are the practice;
documented in CONTRIBUTING), and hub fan-out — in that order.

## 2. Motivation

- **The numbers** (epic #382, 2026-07-06): 9 m 46 s warm-up after
  branch churn; ~10 min full-workspace `cargo test`; 57 s to relink
  querier tests after a core touch; `target/debug` hit 314 GiB
  before the #373 debuginfo trim. A single session repeatedly
  tripped 10-minute task budgets on rebuilds.
- **Every test file is a linker invocation.** The RFC-ladder
  discipline creates one integration-test file per scenario group —
  correct for clarity, quadratic-feeling for links. 31 binaries in
  `ourios-ingester` each link the tonic/tokio receiver stack.
- **sccache does not save the local loop** (measured: 37/199 hits,
  all C/C++ build scripts) — cargo's incremental dev builds bypass
  it by design. Its value is CI; local latency must come from
  structure.
- **The hub tax compounds.** Every future crate consuming core
  types inherits the config-churn invalidation unless the split
  happens while the workspace is still 11 crates.

## 3. Design

### 3.1 Test-harness consolidation (slices 1–2)

Per crate: a single `tests/it/main.rs` harness (Cargo's
one-binary idiom) with `mod` declarations per current file —
`tests/it/rfc0003_1_wal_before_ack.rs` etc. keep their content and
test names verbatim. Shared fixtures (`tests/common`,
`tests/ingest_support`) become harness modules, ending the
compile-per-binary duplication of helpers.

- Worst crate first (`ourios-ingester`, 31 → 2: one general harness
  plus keeping any test that *requires* process isolation — e.g.
  SIGKILL crash-recovery — as its own binary, explicitly annotated).
- Scenario-name greppability is preserved: `cargo test
  rfc0003_1` still works; CI invocations by `--test <name>` are
  updated in the same slice (the rfc0024 deep-run workflow names
  four).

### 3.2 `ourios-core` split (slice 3)

New crate `ourios-config` holding `MinerConfig`,
`MinerConfigError`, bound constants and builders. `ourios-core`
keeps pure data types and the canonical codec. Consumers move one
`use` path; no behavior change. The §7 layout table gains one row —
this RFC is the architectural commitment §7 requires.

Explicitly out: splitting audit/alias/otlp out of core — no
measurement implicates them, and every split multiplies version
lockstep costs.

### 3.3 Parquet split (slice 4, decision gate)

Re-run the #382 probe set after slices 1–2. Proceed with a
reader/writer split only if a parquet edit still costs > 30 s of
check fan-out or shows up in the top of `cargo build --timings`
critical path; otherwise record the decision and close.

### 3.4 nextest (slice 5)

`cargo nextest run` locally and in CI's test job; `cargo test`
remains supported (property suites' `proptest` integration is
runner-agnostic). CI keeps the exact same suite inventory.

## 4. Alternatives considered

- **Only crate splits (the original instinct).** The data says the
  link storm, not check fan-out, is the dominant cost; splits alone
  would leave 104 binaries linking.
- **One mega test binary per workspace.** Cross-crate harnesses
  can't exist (integration tests are per-crate), and a single
  binary per crate that force-includes isolation-sensitive tests
  (crash recovery) would serialize or destabilize them.
- **`CARGO_INCREMENTAL=0` + sccache locally.** Trades away
  incremental compilation (the thing that makes 17–38 s checks
  possible) to feed sccache; strictly worse for the edit loop.
- **Shared monolithic `tests/common` crate.** A dev-only fixtures
  crate would rebuild on every core change and re-couple the crates
  the split decouples; harness-local modules suffice.

## 5. Acceptance criteria

Written at the `specified` gate (docs/rfcs/README.md lifecycle).
Proposed scenarios accompany the drafting PR for sign-off.

## 6. Testing strategy

Follows §5 at the `specified` gate.

## 7. Open questions

1. **Crash-recovery isolation inventory.** Which tests genuinely
   need their own process/binary (SIGKILL, env-mutating,
   `#[ignore]`d hardware gates)? Slice 1 produces the list.
2. **Per-branch target dirs.** Worktrees already give this
   implicitly; whether to document `CARGO_TARGET_DIR` conventions
   for branch-heavy local work, or leave it to worktree practice.
3. **`ourios-config` naming and scope** — config only, or does the
   RFC 0020 file-config layer's schema (currently in
   `ourios-server`) eventually belong beside it?

## 8. References

- Epic #382 (measurements, 2026-07-06), maintainer precedence
  instruction (same date), #373 (debuginfo trim), CLAUDE.md §6.2
  (tests are specifications — consolidation moves, never weakens),
  §7 (new crates are RFC-level), §8.2 (worktrees for parallel
  work), cargo book (integration-test harness layout), cargo-nextest.
