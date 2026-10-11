---
rfc: 0015
title: Fuzzing harness — cargo-fuzz targets & ClusterFuzzLite CI
status: accepted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-19
supersedes: —
superseded-by: —
---

# RFC 0015 — Fuzzing harness: cargo-fuzz targets & ClusterFuzzLite CI

> **Status: `accepted` (2026-08-25, maintainer sign-off).** Terminal —
> the completed-backlog batch flip. Where a thesis-gate applies it
> stands passing in `docs/benchmarks.md` §7; elsewhere `validated` is
> vacuous for this surface (RFC 0008 precedent).

## 1. Summary

Add a coverage-guided fuzzing harness: a `fuzz/` cargo-fuzz workspace
member with libFuzzer targets on the project's highest-risk surfaces —
the template miner and the untrusted-input parsers (OTLP protobuf,
OTLP/JSON, WAL frame). The miner target does not merely check for
panics: it asserts the §3.3 bit-identical-reconstruction invariant, so
the fuzzer actively hunts inputs that round-trip wrong. CI is phased —
Phase 1 (this RFC) lands the targets plus a bounded smoke-fuzz job that
gates on crashes; Phase 2 (a follow-up) layers ClusterFuzzLite for
continuous fuzzing, corpus persistence, and OpenSSF Scorecard detection
of the `Fuzzing` check. The new `fuzz/` member is the architectural
commitment this RFC exists to authorise (`CLAUDE.md` §7).

## 2. Motivation

The template miner is named in `CLAUDE.md` §2 as *"the single biggest
engineering risk in the project,"* and §3.1 / §3.3 make its merge
correctness and bit-identical reconstruction load-bearing invariants.
The OTLP decoders (RFC 0003) and the WAL frame reader (RFC 0008) parse
**adversarial bytes** off the wire and off disk — exactly the boundary
fuzzing is built for.

`proptest` already guards these invariants (e.g.
`crates/ourios-miner/tests/invariants.rs`), but property tests explore
only the input space a hand-written `Strategy` describes. Coverage-guided
fuzzing instruments the binary and mutates toward unexplored branches,
reaching malformed-but-structurally-valid inputs — truncated protobuf,
non-UTF-8 bodies, CRC-valid-but-semantically-broken frames — that a
generator rarely synthesises. The two techniques are complementary:
proptest pins the invariants we can describe; the fuzzer finds the ones
we did not think to.

Why now: the ingest and query stack is built and tested behind RFC
gates, so the parsing and mining surfaces are stable enough that fuzz
findings reflect real bugs rather than churn. Fuzzing was previously
parked in the "deferred to the shipping milestone" set alongside
Signed-Releases; the maintainer has opted to pull it forward (it finds
bugs cheaply, before they calcify into the on-disk contract). Closing
Scorecard's `Fuzzing` check (currently 0) is a secondary benefit of
Phase 2, not the primary driver.

## 3. Proposed design

### 3.1 The `fuzz/` workspace member

A single new workspace member at the repo root, `fuzz/`, following the
cargo-fuzz convention (`cargo fuzz init`). It is:

- **not published** (`publish = false`) and carries no library API —
  it exists only to host fuzz targets. Code the targets share (the
  telemetry bootstrap, the miner oracles, the OTLP round-trip check)
  lives in `fuzz/common/*.rs` and is pulled into each target with a
  `#[path]` `mod` include, so the package still has no `lib.rs`;
- **built with nightly Rust.** libFuzzer requires sanitizer/`-Z`
  support absent from stable. `rust-toolchain.toml` stays `stable`
  (the source of truth for every shipping crate per `CLAUDE.md` §6.1);
  the nightly toolchain is requested **only** by the fuzz CI job and by
  developers running fuzz locally. This is a contained, documented
  deviation from the §6.1 stable pin — it never touches the binaries we
  ship;
- **opts out of the workspace `unsafe_code = "deny"` lint** (root
  `Cargo.toml` `[workspace.lints.rust]`; every shipping crate inherits
  it via `[lints] workspace = true`), because the
  `libfuzzer_sys::fuzz_target!` macro (the `libfuzzer-sys` crate) expands
  to `unsafe` glue. `CLAUDE.md`
  §6.1 permits a per-crate waiver where an RFC justifies one (it cites a
  possible `ourios-parquet` zero-copy need as the example; no crate
  carries such a waiver today — every crate root, `ourios-parquet`
  included, is `#![deny(unsafe_code)]`). This RFC is that justification,
  scoped to `fuzz/` alone — the member simply does not inherit the
  workspace lint; our harness bodies stay safe.

Adding this member is a `CLAUDE.md` §7 new-crate decision; this RFC is
that decision's record.

### 3.2 The targets

Five targets, ranked by risk. Each is a `fuzz_target!` reaching a stable
entry point with minimal glue; each asserts an invariant of what the
entry point *accepts*, not only that it does not panic.

| Target | Entry point | Crate | Oracle |
|---|---|---|---|
| `miner_roundtrip` ⭐ | the input split on `\n`; every line a **string-body** `OtlpLogRecord` into one `MinerCluster` (observable `RecordSink`) for one tenant; the leaf snapshotted via `templates_for` by `(template_id, template_version)` after each `ingest`; then `reconstruct::render` per row | `ourios-miner` | **invariants**: the rendered bytes equal the row's own line whether `render` reports `Reconstruction::Faithful` or `RetainedVerbatim`, a clean row is `Faithful` — §3.3 (RFC0015.1); every row agrees with its template's literals outside the wildcards, versions only widen, one row per ingest — §3.1 (RFC0015.10) |
| `otlp_json` | `decode_json(&[u8])` | `ourios-ingester` | no panic (RFC0015.2); an accepted request survives the protobuf round trip (RFC0015.9) |
| `otlp_protobuf` | `decode_protobuf(&[u8])` | `ourios-ingester` | no panic (RFC0015.3); decode → encode is a fixpoint (RFC0015.9) |
| `wal_frame` | `frame::read_frame(&mut Cursor::new(data))` on the raw input, and `read_frame(write_frame(kind, payload))` with the first byte picking the `FrameKind` — both `pub` only under the `fuzzing` feature (§7) | `ourios-wal` | no panic; malformed input yields a typed `FrameError`, never UB (RFC0015.4); the writer's frame reads back as `(kind, payload)`, fully consumed (RFC0015.9) |
| `dsl_parse` | `parse_statement(&str)` and `parse_structured_statement(&str)` on the same text | `ourios-querier` | no panic (RFC0015.8); an accepted statement round-trips its canonical form, `parse_statement(serialize_statement(s)) == s` (RFC0002.7) |

**`miner_roundtrip` is the centerpiece.** The target feeds the miner a
*sequence*: the input is split on `\n` (each line capped at 4 KiB, at
most 64 lines, lossy UTF-8 — nothing is discarded) and every line is
ingested as an `OtlpLogRecord` whose body is a **`String`** (the Drain
template path) into one `MinerCluster` for one tenant, so one input
drives leaf creation, clean reuse, widening and type expansion.
`MinerCluster::ingest` returns only a `template_id`, so the harness
follows the miner's real read-back path (the one
`crates/ourios-miner/tests/invariants.rs` uses): the cluster is built
with an observable `RecordSink` (`SharedRecordSink`), `ingest` is called,
the emitted `MinedRecord` is drained from the sink, and the leaf's
template tokens are looked up via `MinerCluster::templates_for(tenant)`
matching the record's `(template_id, template_version)` — right after
that ingest, because a later widening bumps the leaf past the record's
version. That is the read path's registry key (RFC 0017 §3.5): a row
stamped version N renders against the N-version tokens. Once every line
is in, `reconstruct::render` runs per row with its own version's tokens
and the target asserts the §3.3 contract: the rendered bytes equal the
row's own line **in both outcomes** — whether `render` reports
`Reconstruction::Faithful` (rebuilt from the template) or
`Reconstruction::RetainedVerbatim` (the original body surfaced verbatim,
not rebuilt) — and a *clean* row (not lossy, no §6.5 overflow) must be
`Faithful`, since `render` only retreats to the body on a template-shape
mismatch, which a stored body would otherwise paper over. §3.3
guarantees a string line is either reconstructed exactly or has its
original body retained, so *either* a faithful-rebuild mismatch *or* a
retention failure is a violation — and makes the target panic, which
libFuzzer reports as a crash. That turns the fuzzer into a search for
reconstruction bugs, not just for `unwrap`s.

The same pass asserts §3.1 (`fuzz/common/oracles.rs`, ported from the
RFC 0024 property suite's `assert_no_silent_merge`): a row attached to a
template agrees with that template's literal tokens at every
non-wildcard position (so two rows on one template agree everywhere a
merge across a semantic boundary would differ), successive versions of
one template only ever turn a literal into a wildcard — never a literal
into a different literal, never a wildcard back, never a length change
— and each string-body ingest emits exactly one row.

The target is deliberately scoped to **string** bodies: that is the
template-mining + line-reconstruction path the §3.3 invariant governs.
Structured (`kvlist`/array) bodies take the §6.1 canonical-encoding path
(`lossy_flag = false`, no template walk), whose round-trip is a distinct
property — a candidate for a separate target (§7), not folded into this
oracle.

The four parser targets are panic-oracles on untrusted-input
boundaries — a decoder must reject garbage with a typed error, never
panic, abort, or exhibit UB — and round-trip oracles on what they
accept. The OTLP decoders' output is what the WAL stores re-encoded, so
`encode(decode(encode(request)))` must reproduce `encode(request)`
byte for byte (bytes, not messages: a `NaN` double is a legal wire
value unequal to itself). A WAL frame the writer produces must read
back as exactly `(kind, payload)`, with the reader consuming exactly
the frame. A DSL statement either parser accepts must re-parse from its
canonical serialisation to the same IR — the RFC0002.7 contract the
`dsl/structured.rs` proptest pins over generated trees, here under
coverage guidance. JSON re-encoding of OTLP is deliberately *not*
asserted: proto3-JSON is not canonical (`1` vs `"1"` for 64-bit
integers, enum names vs numbers), so equal JSON is not a contract.

`frame::read_frame` and `frame::write_frame` are `pub(crate)` in the
shipping build; the `fuzzing` cargo feature makes the module `pub`
(`#[doc(hidden)]`, so rustdoc stays honest) for the target — §7.

### 3.3 Seed corpora

Committed seeds live under `fuzz/seeds/<target>/` (a tracked directory,
distinct from the gitignored working corpus `fuzz/corpus/<target>/`).
The CI job copies the seeds into the working corpus before each run, so
the committed inputs bootstrap coverage without the evolving corpus
churning the repo:

- `miner_roundtrip` seeds from a few real-shaped log lines, single-line
  and multi-line (same shape with differing parameters, so the first
  mutations already exercise widening and type expansion);
- `otlp_json` seeds from a minimal `ExportLogsServiceRequest` (an empty
  `{"resourceLogs":[]}`) and a one-record request covering every
  `AnyValue` shape;
- `otlp_protobuf` seeds from the prost encoding of that one-record
  request; `wal_frame` from one `write_frame` output per `FrameKind`
  (`fuzz/README.md` records how both are regenerated);
- `dsl_parse` seeds from the RFC 0002 cookbook queries, structured-JSON
  examples, and the regression inputs of fixed crashes (§3.5).

The two text-grammar targets also take a libFuzzer **dictionary**
(`fuzz/dict/<target>.dict`: the proto3-JSON keys and value shapes of
`ExportLogsServiceRequest`; the §7 grammar's keywords and operators plus
the structured surface's tags), passed with `-dict=`, so mutations reach
the parsers' productions rather than the lexer's error arm.

Committed seeds are kept minimal (enough to bootstrap coverage); the
grown corpus is persisted by ClusterFuzzLite in Phase 2, not committed.

### 3.4 CI — phased

**Phase 1 (this RFC's `green`): `.github/workflows/fuzz.yml`.** A bounded
smoke-fuzz job on a pinned nightly toolchain, run as a matrix over **all
four** targets — the parser targets are cheap, so there is no reason to
gate on the miner alone. Each matrix job runs its target for the budget
of the triggering event, e.g.:

```sh
# Daily schedule: ~300 s per target. Manual dispatch: ~60 s. --target
# forces the gnu host triple (cargo-fuzz otherwise picks musl, whose
# static libc is incompatible with the ASan sanitizer).
cargo +nightly-2026-06-01 fuzz run <target> --target "$host" -- -max_total_time=300
```

It runs on a **daily `schedule` and on `workflow_dispatch`** —
deliberately *not* on `pull_request`: the sanitizer build is too heavy
for per-change CI, and continuous per-change fuzzing is Phase 2's job
(ClusterFuzzLite). `fuzz run` builds before it runs, so a target that
stops compiling fails its job; because every target is always in the
matrix (`fail-fast: false`), all four are built and run on every
invocation. A crash fails that target's job and uploads the reproducer
as an artifact. Top-level `contents: read` (the workflow-token
least-privilege pattern the other workflows follow).

**Phase 2: ClusterFuzzLite.** `.clusterfuzzlite/` (`Dockerfile` on the
OSS-Fuzz `base-builder-rust` image + `build.sh` that `cargo fuzz build`s
the same targets and stages them with their seed corpora) plus
`cflite_batch.yml` (scheduled continuous fuzzing that grows and persists
the corpus) and `cflite_coverage.yml` (weekly corpus line-coverage).
Both run on `schedule` + `workflow_dispatch` only — **no PR-fuzzing
workflow**, consistent with the per-PR rule above. The corpus persists
in the **GitHub Actions cache** (no external storage backend), resolving
the §7 open question. ClusterFuzzLite is what Scorecard's `Fuzzing`
check detects (via `.clusterfuzzlite/Dockerfile`; it cannot see a bare
cargo-fuzz directory), so Phase 2 is what moves that check 0 → positive
(RFC0015.7). The cflite container build is verified by dispatching
`cflite_batch` after merge — it cannot run on the introducing PR with no
PR trigger.

> **Amendment (2026-10-11, #950, maintainer-approved): one `Fuzz`
> workflow, four jobs.** The three workflows above are one
> `.github/workflows/fuzz.yml` with four jobs, each on its own
> `schedule` entry (gated by `github.event.schedule`) or on
> `workflow_dispatch` with a `job` input naming one job (`smoke |
> batch | coverage | prune`); still no `pull_request` trigger.
>
> - `smoke` (daily 04:17) is no longer a matrix: one `cargo fuzz build
>   --target "$host"` of every target, then a loop over the targets,
>   each run for its budget (300 s on the schedule, 60 s on dispatch)
>   with `-timeout=30 -rss_limit_mb=2048`. A crash is recorded and the
>   loop continues, so a crash in one target does not stop the others;
>   the job fails at the end and uploads `fuzz/artifacts/`.
> - `batch` (daily 02:00) is ClusterFuzzLite `mode: batch`, 3000 s
>   split across the targets, `parallel-fuzzing: true`.
> - `prune` (daily 01:00, new) is `mode: prune`. The ClusterFuzzLite
>   GitHub Actions guide requires it of any project that batch-fuzzes
>   ("If batch fuzzing is running, you must also run corpus pruning").
>   `batch` and `prune` share a no-cancel concurrency group and never
>   run in the same workflow run: the corpus artifact is their
>   handoff, and ClusterFuzzLite cannot hand it over within one run.
> - `coverage` (Mondays 03:00) is `mode: coverage` and appends
>   per-target line coverage to the step summary.
>
> Every job has a `timeout-minutes` (smoke 60, batch 90, prune 60,
> coverage 30). The ClusterFuzzLite jobs take the same `timeout` /
> `rss_limit_mb` from `fuzz/<target>.options`, staged next to each
> binary by `build.sh`. The corpus persists as the
> `cifuzz-corpus-<target>` workflow artifacts (90-day expiry, rolled by
> each run) — not the Actions cache as written above and in §7; a
> storage repo remains an open maintainer decision. The status stays
> `accepted`; RFC0015.5 and RFC0015.7 as amended are the criteria.

### 3.5 Regression discipline

When the fuzzer finds a crash, the workflow per `CLAUDE.md` §6.2 is:
minimise the reproducer (`cargo fuzz tmin`), commit it as a permanent
seed under the tracked `fuzz/seeds/<target>/` as
`regression_<issue>_<n>` (the working `fuzz/corpus/` is gitignored, so
a reproducer parked there would not persist — §3.3), then fix the bug.
The seed stays forever, re-checked on every run — a found bug becomes a
standing specification, never silently dropped. The oracles' panic
messages name the invariant (`RFC0015.N / CLAUDE.md §3.x`), so a crash
artefact says which contract broke before anyone opens a debugger.

## 4. Alternatives considered

**`afl.rs` (AFL++) instead of cargo-fuzz/libFuzzer.** AFL++ is a capable
fuzzer, but cargo-fuzz/libFuzzer is the de-facto Rust default, has the
smoothest `cargo` integration, and is the engine ClusterFuzzLite and
OSS-Fuzz drive for Rust. Choosing it keeps Phase 1 and Phase 2 on one
engine.

**Just extend proptest, no coverage-guided fuzzing.** The obvious
cheaper move is to widen the existing `proptest` suites rather than add a
fuzz toolchain. We keep and value proptest, but it cannot replace
fuzzing here: its inputs come from hand-authored `Strategy` generators
that sample a distribution *we* describe, with no feedback from the code
under test. A coverage-guided fuzzer instruments the binary and mutates
toward unexecuted branches, reaching the malformed-but-structurally-valid
inputs (truncated protobuf, CRC-valid-but-broken frames, non-UTF-8 body
bytes) that a generator only hits by luck. proptest pins the invariants
we can describe; the fuzzer finds the ones we did not think to write a
strategy for. They are complementary layers, not substitutes — which is
also why the `miner_roundtrip` oracle deliberately reuses the same §3.3
assertion the proptest suite already encodes.

**OSS-Fuzz from day one instead of ClusterFuzzLite.** OSS-Fuzz is the
richer option — Google-hosted compute, long-running campaigns, automatic
bug filing — and remains the goal once Ourios ships. But acceptance
requires a project to be widely used or critical to the ecosystem, which
a pre-release backend is not, and onboarding adds an external dependency
and review loop we do not control. ClusterFuzzLite is the same engine
(libFuzzer) running in our own CI with our own corpus, available today
and detected by Scorecard; it is the pragmatic Phase 2, with OSS-Fuzz
held as a post-ship upgrade.

**A `fuzzing` feature inside each crate instead of a separate `fuzz/`
member.** Folding targets into the shipping crates would drag the
nightly/sanitizer toolchain and the `unsafe` macro expansion into code
we ship. The cargo-fuzz convention isolates all of that in `fuzz/`.

**Keep fuzzing deferred to the shipping milestone.** Rejected by the
maintainer: the surfaces are stable now, fuzzing is cheap, and bugs
found pre-release never reach the on-disk contract. Deferral only delays
the find.

## 5. Acceptance criteria

> **Scenario RFC0015.1 — miner round-trip target enforces the §3.3 invariant**
> - **Given** the `miner_roundtrip` target and a `MinerCluster`
>   built from `MinerConfig::default()` with an observable
>   `RecordSink` attached
> - **When** the target builds an `OtlpLogRecord` with a
>   **`String`** body from each line of the arbitrary input,
>   ingests it, drains the emitted `MinedRecord` from the sink,
>   looks up the leaf tokens via `templates_for` for the record's
>   `(template_id, template_version)`, and calls `render`
> - **Then** the rendered bytes equal the original string body in
>   **both** outcomes — whether `render` reports
>   `Reconstruction::Faithful` (rebuilt from the template) or
>   `Reconstruction::RetainedVerbatim` (the original body returned
>   verbatim) — since §3.3 guarantees a string line is either
>   reconstructed exactly or has its original body retained
> - **And** the `Reconstruction` marker is asserted to be one of
>   those two variants, recording which path produced the bytes;
>   for a clean row (`lossy_flag` unset, no `Overflow` param) it
>   is asserted to be `Faithful`, and for a diverted row
>   (`NO_TEMPLATE`) `lossy_flag` is set and `body` carries the
>   line verbatim
> - **And** *any* input whose rendered bytes differ from the
>   original string body makes the target panic (a libFuzzer
>   crash) — a faithful-rebuild mismatch **and** a retention
>   failure are both §3.3 violations
> - **And** the assertion references the §3.3 invariant id so the
>   mapping back to `CLAUDE.md` is greppable

> **Scenario RFC0015.2 — OTLP/JSON decode never panics**
> - **Given** the `otlp_json` target
> - **When** it is run on arbitrary bytes
> - **Then** `decode_json` returns `Ok(_)` or `Err(DecodeError)`
> - **And** the target never panics, aborts, or triggers a
>   sanitizer error on any input in a bounded run

> **Scenario RFC0015.3 — OTLP/protobuf decode never panics**
> - **Given** the `otlp_protobuf` target
> - **When** it is run on arbitrary bytes
> - **Then** `decode_protobuf` returns `Ok(_)` or
>   `Err(DecodeError)`
> - **And** the target never panics, aborts, or triggers a
>   sanitizer error on any input in a bounded run

> **Scenario RFC0015.4 — WAL frame decode yields a typed error, never UB**
> - **Given** the `wal_frame` target wrapping the input in a
>   `Cursor`
> - **When** `read_frame` is run on arbitrary bytes
> - **Then** it returns `Ok((kind, payload))` or a `FrameError`
>   (bad CRC, length over `MAX_FRAME_BYTES`, unknown kind, or
>   non-zero pad)
> - **And** the target never panics or exhibits undefined
>   behaviour, including on truncated headers and length fields
>   that overrun the buffer

> **Scenario RFC0015.5 — CI smoke-fuzz is bounded and gates on crashes**
> - **Given** `.github/workflows/fuzz.yml` on the nightly
>   toolchain
> - **When** a PR touches `ourios-miner`, `ourios-ingester`, or
>   `ourios-wal`, or the daily schedule fires
> - **Then** each target is built (`cargo fuzz build`) and run for
>   its configured bounded budget
> - **And** a crash fails the job and uploads the crashing input
>   as an artifact
> - **And** the job uses top-level `contents: read` (least
>   privilege, matching the other workflows)

> **Amendment (2026-10-11, #950): RFC0015.5 as amended.** The
> scenario reads:
> - **Given** `.github/workflows/fuzz.yml` on the nightly toolchain
> - **When** the daily schedule fires or the workflow is dispatched
>   (there is no `pull_request` trigger — §3.4)
> - **Then** every target is built once (`cargo fuzz build`) and each
>   is run for its configured bounded budget with libFuzzer
>   `-timeout` and `-rss_limit_mb` set
> - **And** each target's crash fails the job and uploads its
>   reproducer; a crash in one target does not stop the others
> - **And** the workflow uses top-level `contents: read` only, and
>   every job carries a `timeout-minutes`

> **Scenario RFC0015.6 — a found crash becomes a permanent regression seed**
> - **Given** the fuzzer has found and the team has fixed a crash
> - **When** the fix lands
> - **Then** the minimised reproducer is committed under
>   `fuzz/seeds/<target>/` as `regression_<issue>_<n>` and is
>   re-exercised on every subsequent run
> - **And** the seed is never removed to make a run pass
>   (`CLAUDE.md` §6.2)

> **Scenario RFC0015.7 — ClusterFuzzLite is detected by Scorecard (Phase 2)**
> - **Given** the Phase 2 follow-up has landed `.clusterfuzzlite/`
>   and the `cflite_*` workflows
> - **When** the OpenSSF Scorecard workflow runs
> - **Then** the `Fuzzing` check detects ClusterFuzzLite and
>   scores greater than 0
> - **Note**: this scenario is **out of scope for this RFC's
>   `green`** and gates the Phase 2 PR; it is recorded here so the
>   phasing is explicit.

> **Amendment (2026-10-11, #950): Phase 2 done.** `.clusterfuzzlite/`
> landed with the `cflite_*` workflows, since folded into `fuzz.yml`
> as the `batch`, `coverage` and `prune` jobs (§3.4 as amended), so
> the **Given** reads "`.clusterfuzzlite/` and the ClusterFuzzLite
> jobs of `fuzz.yml`". Scorecard's `Fuzzing` check scores 10/10
> ("project is fuzzed"; Scorecard API, 2026-08-21 run).

> **Scenario RFC0015.8 — DSL parse never panics and round-trips its canonical form**
> - **Given** the `dsl_parse` target
> - **When** `parse_statement` and `parse_structured_statement`
>   are run on arbitrary UTF-8 text
> - **Then** each returns `Ok(Statement)` or `Err(DslError)` and
>   the target never panics, aborts, or triggers a sanitizer error
>   on any input in a bounded run
> - **And** for every `Ok(s)`, `parse_statement(serialize_statement(&s))`
>   is `Ok` and equals `s` (RFC0002.7); a canonical form that fails
>   to parse or re-parses differently makes the target panic

> **Scenario RFC0015.9 — what a parser accepts survives its round trip**
> - **Given** the `otlp_protobuf`, `otlp_json` and `wal_frame`
>   targets
> - **When** `decode_protobuf` or `decode_json` returns `Ok(request)`
> - **Then** `request.encode_to_vec()` decodes, and re-encodes to
>   the same bytes (compared as bytes so a `NaN` double cannot
>   raise a false positive)
> - **When** the `wal_frame` target writes `write_frame(kind,
>   payload)` for the `FrameKind` its first input byte selects and
>   the remaining bytes as the payload
> - **Then** `read_frame` on those bytes returns `Ok((kind,
>   payload))` and consumes exactly the frame
> - **And** any violation makes the target panic with a message
>   naming this scenario

> **Scenario RFC0015.10 — the miner sequence target enforces §3.1 no silent merge**
> - **Given** the `miner_roundtrip` target with every line of the
>   input ingested into one `MinerCluster` for one tenant, and each
>   row's template snapshotted by `(template_id, template_version)`
>   right after its ingest
> - **When** the rows are checked after the last ingest
> - **Then** every string-body ingest emitted exactly one row
> - **And** every row with a template has the template's token
>   count and matches its literal tokens at every non-wildcard
>   position
> - **And** for every `template_id`, each later version keeps the
>   token count and changes tokens only from a literal to a
>   wildcard — never a literal to a different literal, never a
>   wildcard back to a literal
> - **And** any violation makes the target panic with a message
>   citing `CLAUDE.md` §3.1

## 6. Testing strategy

Per `CLAUDE.md` §6.2, the fuzz targets *are* the tests — coverage-guided
libFuzzer runs rather than fixed-input unit tests.

- **RFC0015.1** — the miner target's oracle is the same §3.3 invariant
  asserted by the existing property tests in
  `crates/ourios-miner/tests/invariants.rs` and the round-trip unit
  tests in `crates/ourios-miner/src/reconstruct.rs`; the fuzz target
  reuses that assertion under coverage guidance. Cross-referenced so the
  two layers stay in sync.
- **RFC0015.2 / .3 / .4 / .8** — panic-oracle targets. Verified by a
  bounded fuzz run (no crash) in CI; `cargo fuzz build` proves they
  compile even on runs where they are not executed.
- **RFC0015.9 / .10** — round-trip and no-silent-merge oracles inside
  the same targets. Each was proven to fire before landing by breaking
  the code under test locally (a mis-encoded frame kind, a dropped
  literal) and watching the bounded run crash on the first seed; the
  seeds in §3.3 keep the oracles on the happy path every run.
- **RFC0015.8** mirrors the `dsl/structured.rs` proptest and the
  `display.rs` corpus test (`parse(serialize(q)) == q`), so the two
  layers stay in sync as the grammar grows.
- **RFC0015.5** — the `fuzz.yml` workflow itself; smoke budgets kept
  small enough to be non-flaky. The real coverage accrues from the
  Phase 2 continuous runs, not the bounded smoke job (which runs on a
  schedule and on dispatch, never per PR — §3.4).
- **RFC0015.6** — exercised the first time a crash is found; the
  committed reproducer is a standing corpus entry thereafter.

Each scenario id (`RFC0015.N`) is referenced from the corresponding
target source or workflow comment so the spec-to-test mapping is
greppable (`docs/verification.md` §2).

## 7. Open questions

Maintainer review (2026-06-19) gave direction on the following;
recorded here as the planned approach for the implementation PRs (to be
confirmed as the RFC advances toward `green`):

- [x] **Nightly pin** → **pin a dated `nightly-YYYY-MM-DD`** in the fuzz
  job (not a floating `nightly`), for reproducibility; Renovate bumps
  it like the other pinned toolchains.
- [x] **Smoke-fuzz budget** → **~60 s per target on PRs, ~300 s on the
  daily schedule** (see §3.4). Revisit if CI minutes or signal warrant.
- [x] **`read_frame` exposure** → a **`fuzzing` cargo feature** on
  `ourios-wal` gating the `pub` export, rather than a `#[doc(hidden)]`
  shim — slightly cleaner and reusable for future non-fuzz tests.
- [x] **`OtlpLogRecord` construction** → expect a **hand-written
  `Arbitrary` impl** (or a thin newtype) for the string-body path,
  rather than relying on `derive` across the body variants, if a derive
  proves messy. *Outcome:* neither — the target splits the raw bytes on
  `\n` and builds each record with `..Default::default()`; `arbitrary`
  is not a direct dependency of the harness.

Still open, deferred to the implementation PRs:

- [x] **A second miner target** driving *sequences* of records, to fuzz
  template **merge** behaviour (§3.1), not just single-line round-trip?
  → Folded into `miner_roundtrip` itself rather than a second binary:
  the input is a line sequence into one tree, and the §3.1 oracle runs
  beside the §3.3 one (§3.2, RFC0015.10).
- [ ] **A structured-body round-trip target** exercising the §6.1
  canonical encoding (`AnyValue ↔ stored bytes` determinism), separate
  from `miner_roundtrip`'s string-body scope? Possible Phase 1.5.
- [x] **Phase 2 corpus-persistence backend** → **GitHub Actions cache**
  (ephemeral, in-repo, no external storage or secrets), chosen over a
  storage branch/bucket. Implemented in `cflite_batch.yml`. *(Amended
  2026-10-11, #950: with no storage repo ClusterFuzzLite in fact keeps
  the corpus as `cifuzz-corpus-<target>` workflow artifacts — 90-day
  expiry, rolled by each run — now the `batch` / `prune` jobs of
  `fuzz.yml`, §3.4 as amended.)*
- [ ] **`unsafe` waiver** for `fuzz/`: confirm that having the fuzz
  member opt out of the workspace `unsafe_code = "deny"` lint (the first
  such waiver in the repo) is acceptable, given the `fuzz_target!` macro
  requires it.

## 8. References

- cargo-fuzz book: <https://rust-fuzz.github.io/book/>
- libFuzzer: <https://llvm.org/docs/LibFuzzer.html>
- ClusterFuzzLite: <https://google.github.io/clusterfuzzlite/>
- OSS-Fuzz: <https://google.github.io/oss-fuzz/>
- `arbitrary` crate: <https://docs.rs/arbitrary/>
- OpenSSF Scorecard `Fuzzing` check:
  <https://github.com/ossf/scorecard/blob/main/docs/checks.md#fuzzing>
- Related RFCs: RFC 0001 (template miner), RFC 0003 (OTLP receiver),
  RFC 0008 (write-ahead log).
- `CLAUDE.md` §2 (pillar #2, miner risk), §3.1 (no silent merges),
  §3.3 (bit-identical reconstruction), §3.4 (WAL), §4 (hazards),
  §6.1 (stable-toolchain pin — contained deviation), §6.2 (testing
  discipline), §7 (new-crate commitment); `docs/hazards.md`.
