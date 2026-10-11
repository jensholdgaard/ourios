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
  it exists only to host fuzz targets;
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

> **Amendment (2026-10-11, #951, maintainer-approved): shared harness code.**
> Code the targets share (the telemetry bootstrap, the miner oracles,
> the OTLP round-trip check) lives in `fuzz/common/*.rs` and is pulled
> into each target with a `#[path]` `mod` include. The package still
> has no `lib.rs`, so the no-library-API rule above holds.

### 3.2 The targets

Four targets, ranked by risk. Each is a `fuzz_target!(|data: &[u8]|)`
reaching a stable entry point with minimal glue.

| Target | Entry point | Crate | Oracle |
|---|---|---|---|
| `miner_roundtrip` ⭐ | `ingest` (with an observable `RecordSink`) → drain the `MinedRecord` → `templates_for` → `reconstruct::render`, on a **string-body** record | `ourios-miner` | **invariant**: the rendered bytes equal the original string body whether `render` reports `Reconstruction::Faithful` (rebuilt) or `Reconstruction::RetainedVerbatim` (retained) — §3.3 |
| `otlp_json` | `decode_json(&[u8])` | `ourios-ingester` | no panic; `Ok`/`Err` both fine |
| `otlp_protobuf` | `decode_protobuf(&[u8])` | `ourios-ingester` | no panic; `Ok`/`Err` both fine |
| `wal_frame` | `frame::read_frame(&mut Cursor::new(data))` — today `pub(crate)`, exposed to the target via the `fuzzing` feature (§3.2, §7) | `ourios-wal` | no panic; malformed input yields a typed `FrameError`, never UB |

**`miner_roundtrip` is the centerpiece.** Rather than feed the miner a
fixed string, the target uses the `arbitrary` crate to build an
`OtlpLogRecord` whose body is a **`String`** (the Drain template path —
the fuzz bytes become the log line; attributes are derived alongside).
`MinerCluster::ingest` returns only a `template_id`, so the harness
follows the miner's real read-back path (the one
`crates/ourios-miner/tests/invariants.rs` uses): the cluster is built
with an observable `RecordSink` (`SharedRecordSink`), `ingest` is called,
the emitted `MinedRecord` is drained from the sink, the leaf's template
tokens are looked up via `MinerCluster::templates_for(tenant)` matching
the record's `(template_id, template_version)`, and `reconstruct::render`
is called with that record and those tokens. It then asserts the §3.3
contract: the rendered bytes equal the original string body **in both
outcomes** — whether `render` reports `Reconstruction::Faithful` (rebuilt
from the template) or `Reconstruction::RetainedVerbatim` (the original
body surfaced verbatim, not rebuilt). §3.3 guarantees a string line is
either reconstructed exactly or has its original body retained, so
*either* a faithful-rebuild mismatch *or* a retention failure is a
violation — and makes the target panic, which libFuzzer reports as a
crash. That turns the fuzzer into a search for reconstruction bugs, not
just for `unwrap`s.

The target is deliberately scoped to **string** bodies: that is the
template-mining + line-reconstruction path the §3.3 invariant governs.
Structured (`kvlist`/array) bodies take the §6.1 canonical-encoding path
(`lossy_flag = false`, no template walk), whose round-trip is a distinct
property — a candidate for a separate target (§7), not folded into this
oracle.

The three parser targets are panic-oracles on untrusted-input
boundaries: a decoder must reject garbage with a typed error, never
panic, abort, or exhibit UB.

`frame::read_frame` is currently `pub(crate)`. Rather than widen the WAL
public API, expose it to the fuzz target through a `#[doc(hidden)]`
shim (or a `fuzzing` cargo feature) — resolved in §7.

> **Amendment (2026-10-11, #951, maintainer-approved): five targets, each an
> invariant oracle.** The table above lists four targets with mostly
> panic-only oracles. As amended there are five, and each asserts an
> invariant of what its entry point *accepts*:
>
> | Target | Entry point | Oracle |
> |---|---|---|
> | `miner_roundtrip` ⭐ | the input as length-prefixed bodies (little-endian `u16` length, capped at 4 KiB, at most 64 bodies, lossy UTF-8; a body may contain `\n`, which the tokenizer treats as a separator); every body a **string-body** record into one `MinerCluster` for one tenant, with observable record and audit sinks | §3.3 per row (RFC0015.1 as amended); §3.1 audit per ingest (RFC0015.1 as amended); §3.1 no silent merge (RFC0015.10) |
> | `otlp_json` | `decode_json(&[u8])` | no panic (RFC0015.2); an accepted request survives the protobuf round trip (RFC0015.9) |
> | `otlp_protobuf` | `decode_protobuf(&[u8])` | no panic (RFC0015.3); decode → encode is a byte fixpoint (RFC0015.9) |
> | `wal_frame` | `read_frame` on the raw input, and `read_frame(write_frame(kind, payload))` with the first byte picking the `FrameKind` | typed `FrameError`, never UB (RFC0015.4); the writer's frame reads back as `(kind, payload)`, fully consumed (RFC0015.9) |
> | `dsl_parse` | `parse_statement` and `parse_structured_statement` on the same text | no panic (RFC0015.8); an accepted statement round-trips its canonical form (RFC0002.7) |
>
> **`miner_roundtrip` drives a sequence.** One input now reaches leaf
> creation, clean reuse, widening and type expansion. Right after each
> ingest, the leaf the row was stamped with is snapshotted by
> `(template_id, template_version)`, the read path's registry key (RFC
> 0017 §3.5). A later widening bumps the leaf past the row's version,
> and a row stamped N must render against the N-version tokens. Every
> row is then checked against both halves of §3.3: it renders back to
> its own line whether `render` reports `Faithful` or
> `RetainedVerbatim`, and a *clean* row (not lossy, no §6.5 overflow)
> must be `Faithful`. A diverted row must be lossy-flagged with the
> line retained verbatim. `render` only retreats to the body on a
> template-shape mismatch, which a stored body would otherwise paper
> over.
>
> **§3.1 has two checks in this target.** First, after each ingest the
> tenant's leaves before and after are compared with what the audit
> sink received. "Every merge emits an audit event with old + new
> templates" (RFC 0001 H1.3) becomes: a new leaf has exactly one
> `Created` event, and a version that rose `v → w` has exactly the
> `Widened` / `TypeExpanded` chain `v → … → w`, its templates matching
> the tree before and after. Second, ported from the RFC 0024 property
> suite's `assert_no_silent_merge`: a row agrees with its template's
> literals at every non-wildcard position, and successive versions of
> one template only ever turn a literal into a wildcard.
>
> **The parser targets assert round trips.** The OTLP decoders' output
> is what the WAL stores re-encoded, so the re-encoding must be a byte
> fixpoint. The comparison is on bytes, not messages: a `NaN` double is
> a legal wire value unequal to itself. JSON re-encoding is not
> asserted, because proto3-JSON is not canonical (`1` vs `"1"` for
> 64-bit integers, enum names vs numbers). A DSL statement must
> re-parse from its canonical serialisation to the same IR, the
> RFC0002.7 contract the `dsl/structured.rs` proptest pins over
> generated trees. `frame::write_frame` joins `read_frame` behind the
> `fuzzing` feature, which makes the module `pub` and
> `#[doc(hidden)]`. The status stays `accepted`. Scenarios RFC0015.8–.10
> and the RFC0015.1 amendment are the criteria.

> **Amendment (2026-10-11, #955, maintainer-approved): a sixth target for
> the persisted artifacts.** The process reads back from object storage
> bytes it may not have written, or may have written in an older version;
> a decoder that panics or half-applies them turns every restart into a crash loop
> or a silently wrong state (#892: an unreadable self-written snapshot
> made every restart rebuild from the WAL). One target covers the small
> JSON artifacts, the first input byte picking the decoder:
>
> | Target | Entry point | Oracle |
> |---|---|---|
> | `artifact_decode` | `data[0] % 5`: 0 `snapshot::recover` then `MinerCluster::restore_tenant`; 1 `Manifest::from_json`; 2 the RFC 0059 high-water `parse`; 3 `TemplateMap::from_artifact_bytes` on the raw frame; 4 the same on the input zstd-compressed, so mutations reach the JSON body | an allowed outcome, never a panic (RFC0015.11); a rejected snapshot leaves no state (RFC0015.12); decode → encode → decode is a fixpoint, and a restored snapshot captures back what it restored (RFC0015.13) |
>
> One binary, not four: each decoder is a few functions of JSON over the
> same `serde_json`, coverage is still attributed per function, and the
> ClusterFuzzLite budget (3000 s shared, parallel) splits six ways
> instead of nine. The template-map frame is the only expanding decoder;
> its 64 MiB decompression bound classifies a bomb `Torn`, well inside
> `-rss_limit_mb=2048`. The high-water codec is private to
> `ourios-ingester`; a `fuzzing` feature exposes it as
> `template_ids::fuzzing` (`#[doc(hidden)]`), as `ourios-wal` does for
> its frames, so non-fuzz builds see no API change. The template-map
> reader is already public. Seeds are one valid artifact per decoder,
> each produced by its real encoder (`fuzz/README.md` has the recipe),
> with a dictionary of the artifacts' JSON keys
> (`fuzz/dict/artifact_decode.dict`).
>
> The audit-event Parquet reader (`AuditReader::open_bytes`) is not in
> this target: a Parquet decode is a far larger and slower coverage space
> than these, and it gets its own. The status stays `accepted`;
> scenarios RFC0015.11–.13 are the criteria.

> **Amendment (2026-10-11, #958, maintainer-approved): a seventh target
> for the audit-event Parquet files.** The template registry, the alias
> map and the drift query all fold audit events read back from object
> storage (RFC 0005 §3.7), and the upstream `parquet`/`arrow` decoders
> assert, index and unwrap on malformed pages and footers. Since #956
> every Parquet decode in `ourios-parquet` runs behind a panic boundary
> (`panic_boundary.rs`) that turns such a panic into a typed
> `DecodePanicked`:
>
> | Target | Entry point | Oracle |
> |---|---|---|
> | `audit_reader` | the input is the whole object: `AuditReader::open_bytes`, then `read_all` | `Ok` or an `AuditReaderError` other than `Io`/`PartitionMismatch`, a caught decoder panic being the expected `DecodePanicked`; a panic past the boundary crashes; decoded events re-encode through `audit_events_to_batch` and read back equal (RFC0015.14) |
>
> libfuzzer-sys installs a panic hook that aborts before any unwinding,
> so on its own the boundary's `catch_unwind` never runs under the
> fuzzer. The target swaps that hook for a recording one around the
> single decode call and restores it straight after; its own
> `catch_unwind` re-raises, under the restored hook, any panic the
> boundary let through. Per input and on the fuzzing thread only, so
> no other panic is ever masked. The seed is the audit object of the
> `artifact_decode` seed store; no dictionary (a binary format whose
> column names the seed already carries). While parquet 59's footer
> decoder can be driven into a multi-gigabyte allocation (#959, §6
> below), which the first run reached after ~145 s, the daily smoke run
> gives the target 30 s instead of 300 s and ClusterFuzzLite (batch,
> prune, coverage) does not run it: `.clusterfuzzlite/build.sh` skips
> staging it. Both the smoke override and the exclusion are deleted
> when #959 closes with the parquet 60 / DataFusion 56 upgrade (#846).
> The status stays
> `accepted`; scenario RFC0015.14 is the criterion.

> **Amendment (2026-10-11, #964, maintainer-approved): `dsl_parse`
> reaches the plan.** An accepted query used to stop at the parser.
> The querier then validates it (`plan::validate`), compiles it and
> lowers it to a `DataFusion` logical plan, and the engine's analyzer
> and optimizer run over that plan. Every one of those steps sees
> client text first. A query the DSL accepts but the engine rejects
> while planning comes back as `Storage` ("failed to read storage"):
> the engine, not the DSL, has decided what is a valid query (hazard
> `CLAUDE.md` §4.6). The target now goes on from the parser:
>
> | Target | Entry point | Oracle |
> |---|---|---|
> | `dsl_parse` | as before, then for an accepted log query the querier's `fuzzing::validate` and `fuzzing::lower`: `plan::compile`, `plan::apply` and the aggregation node over an empty in-memory scan, then the analyzer and optimizer, against the current writer's schema (one promoted key per RFC 0042 class) and a scan with every OPTIONAL column absent (RFC0007.4) | validation accepts or returns `InvalidQuery`; lowering returns exactly validation's rejection and lowers an accepted query to a plan or an `InvalidQuery`; the canonical re-parse validates and lowers to the same verdict and an equal plan (RFC0015.15) |
>
> One binary, not two. The plan oracle runs only on the queries a
> parser accepts, a share of the executions (a 300 s run with ASan
> still executes about 1 800 inputs a second, peak RSS about
> 800 MiB). It needs the same seeds and the same
> dictionary, so a second target would duplicate both and split the
> ClusterFuzzLite budget eight ways instead of seven. The binary keeps
> its name, so ClusterFuzzLite's stored corpus carries over. Nothing
> executes and no store is opened. The alias map and template registry
> are empty, the clock is fixed, and the session is built once per
> process. The seam lives behind an `ourios-querier` `fuzzing` feature
> (`#[doc(hidden)] pub mod fuzzing`, as `ourios-wal` and
> `ourios-ingester` do). The lowered plan crosses it as an opaque
> `LoweredPlan` that is only `PartialEq` and `Debug`, so no
> `DataFusion` type enters the crate's API. The aggregation lowering
> moved out of `Querier::execute_aggregate` into a function the
> execution path and the seam share, so the plan fuzzed is the plan
> run. The token denylist on error text (RFC0007.3) is not part of the
> oracle: an `InvalidQuery` detail echoes the query's own keys and
> literals, which can contain any token. The first runs found two
> planner errors that surfaced as `Storage`, each fixed in its own PR:
> the text functions and regex operators on the `Binary` `body`
> column (#960), and a malformed regex pattern (#961). The status
> stays `accepted`; scenario RFC0015.15 is the criterion.

### 3.3 Seed corpora

Committed seeds live under `fuzz/seeds/<target>/` (a tracked directory,
distinct from the gitignored working corpus `fuzz/corpus/<target>/`).
The CI job copies the seeds into the working corpus before each run, so
the committed inputs bootstrap coverage without the evolving corpus
churning the repo:

- `miner_roundtrip` seeds from a few real-shaped log lines;
- `otlp_json` seeds from a minimal `ExportLogsServiceRequest` (an empty
  `{"resourceLogs":[]}`);
- `otlp_protobuf` and `wal_frame` start from libFuzzer's generated
  inputs in Phase 1; binary seeds (valid protobuf encodings / valid
  frames) can be added later.

Committed seeds are kept minimal (enough to bootstrap coverage); the
grown corpus is persisted by ClusterFuzzLite in Phase 2, not committed.

> **Amendment (2026-10-11, #951, maintainer-approved): binary seeds and
> dictionaries.** The binary seeds the list above defers now exist:
>
> - `otlp_protobuf`: the prost encoding of a one-record request that
>   covers every `AnyValue` shape. The same request is also an
>   `otlp_json` seed.
> - `wal_frame`: one `write_frame` output per `FrameKind`.
> - `miner_roundtrip`: multi-line seeds (same shape, differing
>   parameters) start the fuzzer on widening and type expansion.
>
> `fuzz/README.md` records how the binary seeds are regenerated. The
> two text-grammar targets take a libFuzzer **dictionary**
> (`fuzz/dict/<target>.dict`):
>
> - `otlp_json`: the proto3-JSON keys and value shapes of
>   `ExportLogsServiceRequest`;
> - `dsl_parse`: the §7 grammar's keywords and operators, plus the
>   structured surface's tags.
>
> The smoke job passes it with `-dict=`, and ClusterFuzzLite through
> `dict =` in `<target>.options` (§3.4 as amended).

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
seed under the tracked `fuzz/seeds/<target>/` (the working
`fuzz/corpus/` is gitignored, so a reproducer parked there would not
persist — §3.3), then fix the bug. The seed stays forever, re-checked
on every run — a found bug becomes a standing specification, never
silently dropped.

> **Amendment (2026-10-11, #951, maintainer-approved): naming.** A committed
> reproducer is named `fuzz/seeds/<target>/regression_<issue>_<n>`.
> Every oracle's panic message names its invariant
> (`RFC0015.N / CLAUDE.md §3.x`), so a crash artefact says which
> contract broke.

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
>   **`String`** body from the arbitrary input, ingests it,
>   drains the emitted `MinedRecord` from the sink, looks up the
>   leaf tokens via `templates_for` for the record's
>   `(template_id, template_version)`, and calls `render`
> - **Then** the rendered bytes equal the original string body in
>   **both** outcomes — whether `render` reports
>   `Reconstruction::Faithful` (rebuilt from the template) or
>   `Reconstruction::RetainedVerbatim` (the original body returned
>   verbatim) — since §3.3 guarantees a string line is either
>   reconstructed exactly or has its original body retained
> - **And** the `Reconstruction` marker is asserted to be one of
>   those two variants, recording which path produced the bytes
> - **And** *any* input whose rendered bytes differ from the
>   original string body makes the target panic (a libFuzzer
>   crash) — a faithful-rebuild mismatch **and** a retention
>   failure are both §3.3 violations
> - **And** the assertion references the §3.3 invariant id so the
>   mapping back to `CLAUDE.md` is greppable

> **Amendment (2026-10-11, #951, maintainer-approved): RFC0015.1 as
> amended.** The **When** applies to *each* length-prefixed body of
> the input (a body may contain `\n`). Every body is ingested into one `MinerCluster` that has an observable audit
> sink as well as the record sink. Each row's tokens are looked up by
> its own `(template_id, template_version)` right after its ingest.
> Added clauses:
>
> - **And** a clean row (`lossy_flag` unset, no `Overflow` param)
>   renders with `Reconstruction::Faithful`;
> - **And** a diverted row (`NO_TEMPLATE`) has `lossy_flag` set and
>   `body` equal to the line;
> - **And** (`CLAUDE.md` §3.1, RFC 0001 H1.3) after each ingest,
>   every leaf the ingest created has exactly one `Created` audit
>   event carrying its tokens. Every leaf whose version rose `v → w`
>   has exactly the `Widened` / `TypeExpanded` chain `v → … → w`, its
>   old template equal to the leaf before the ingest and its new
>   template equal to the leaf after. No other leaf has a template
>   event;
> - **And** each violation panics with a message citing
>   `RFC0015.1 / CLAUDE.md §3.1` or `§3.3`.

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
>   `fuzz/corpus/<target>/` (or its `regressions/` subdir) and is
>   re-exercised on every subsequent run
> - **And** the seed is never removed to make a run pass
>   (`CLAUDE.md` §6.2)

> **Amendment (2026-10-11, #951, maintainer-approved): RFC0015.6 path.**
> `fuzz/corpus/` is the gitignored working corpus (§3.3), so the
> reproducer goes under the tracked `fuzz/seeds/<target>/` as
> `regression_<issue>_<n>`, as §3.5 already says.

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

> **Scenario RFC0015.8 — DSL parse never panics and round-trips its
> canonical form** (amendment 2026-10-11, #951)
> - **Given** the `dsl_parse` target
> - **When** `parse_statement` and `parse_structured_statement`
>   are run on arbitrary UTF-8 text
> - **Then** each returns `Ok(Statement)` or `Err(DslError)` and
>   the target never panics, aborts, or triggers a sanitizer error
>   on any input in a bounded run
> - **And** for every `Ok(s)`, `parse_statement(serialize_statement(&s))`
>   is `Ok` and equals `s` (RFC0002.7); a canonical form that fails
>   to parse or re-parses differently makes the target panic

> **Scenario RFC0015.9 — what a parser accepts survives its round
> trip** (amendment 2026-10-11, #951)
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

> **Scenario RFC0015.10 — the miner sequence target enforces §3.1 no
> silent merge** (amendment 2026-10-11, #951)
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
> - **And** the same holds for every `Widened` / `TypeExpanded`
>   audit event of an ingest, including one for an intermediate
>   version no row was stamped with: a widening changes only its
>   declared `positions_widened`, each to a wildcard, and a type
>   expansion changes no token
> - **And** the slot types follow the events (RFC 0001 §6.1/§6.4):
>   replayed from the leaf before the ingest, a widening opens one
>   slot per declared position, seeded `{Str, type of the line's
>   token there}`, and a type expansion's `slots_expanded` is
>   non-empty, names each existing slot at most once, and adds only
>   types new to it; the replayed slot types equal the leaf's after
>   the ingest
> - **And** any violation makes the target panic with a message
>   citing `CLAUDE.md` §3.1

> **Scenario RFC0015.11 — persisted-artifact decoders return an allowed
> outcome** (amendment 2026-10-11, #955)
> - **Given** the `artifact_decode` target
> - **When** a miner snapshot, a partition manifest, a template-id
>   high-water or a template-map frame (or its JSON body) is decoded
>   from arbitrary bytes
> - **Then** no decoder panics, aborts, or triggers a sanitizer error
> - **And** `recover(Some(_))` is `(Some, Restored)` only for a byte 0
>   of `SNAPSHOT_VERSION` that `load_snapshot` reads, and otherwise
>   `(None, UnknownOrCorruptDiscarded)`; `legacy_v1_mark` reads as
>   legacy exactly the byte-0 = 1 artefacts
> - **And** `Manifest::from_json` never reports an I/O error and
>   accepts only manifests `validate` accepts; the high-water parses
>   to an id within the template-id domain or is `Malformed`
> - **And** a template-map read is `Valid`, `Torn`, an
>   `UnknownVersion` other than the reader's own, or the loud
>   tenant-mismatch error

> **Scenario RFC0015.12 — a rejected snapshot leaves no partial
> state** (amendment 2026-10-11, #955)
> - **Given** a snapshot that decodes but `restore_tenant` rejects
> - **Then** the cluster holds no state for the tenant and its
>   template-id allocator has not moved

> **Scenario RFC0015.13 — what a persisted-artifact decoder accepts
> survives its writer** (amendment 2026-10-11, #955)
> - **When** a snapshot decodes, its payload re-encodes and decodes
>   equal; when it restores, the captured state (stamped with the
>   folded horizon) equals the decoded state in canonical form: every
>   field of every leaf, structured-template entry and adopted
>   template, and the horizon, with only the format's documented
>   rewrites (set-valued lists sorted and deduplicated, an empty
>   provenance read as its pre-RFC 0050 default, an owned adoption at
>   version 1, a tree-backed adoption carrying no provenance or
>   associations of its own); and that capture restores again to an
>   equal capture
> - **When** a manifest decodes, `to_json` decodes back equal and is a
>   byte fixpoint; when the high-water parses, `encode` parses back to
>   the same id (and so does every id in the domain)
> - **When** a template map reads `Valid`, `to_artifact_bytes` reads
>   back `Valid` with the same frontier, registry and alias classes,
>   and re-encodes to the same bytes
> - **And** any violation makes the target panic with a message
>   naming the scenario

> **Scenario RFC0015.14 — an audit-event Parquet file decodes to a
> typed outcome behind the panic boundary** (amendment 2026-10-11, #958)
> - **Given** the `audit_reader` target and arbitrary bytes as one
>   object from the store
> - **When** `AuditReader::open_bytes` and `read_all` decode them
> - **Then** the result is `Ok` or an `AuditReaderError`, never `Io` or
>   `PartitionMismatch` (bytes have no file system and no partition)
> - **And** a panic inside the `parquet`/`arrow` decoders surfaces as
>   `DecodePanicked`, an accepted outcome, while any panic outside that
>   boundary (in Ourios's own decode) still crashes the target
> - **And** the scenario is about panics, not allocation failures: an
>   abort from parquet 59's footer decoder reserving an untrusted list
>   count is a known exception no boundary can catch, tracked by #959
>   until parquet 60 removes it
> - **And** the decoded events re-encode through
>   `audit_events_to_batch`, write to Parquet and read back equal
> - **And** any violation makes the target panic with a message
>   naming the scenario

> **Scenario RFC0015.15 — an accepted query validates and lowers to
> the DSL's own outcome** (amendment 2026-10-11, #964)
> - **Given** the `dsl_parse` target and a log query either parser
>   accepts
> - **When** it is validated and lowered to its optimized logical plan
>   against the current writer's schema and against a scan with every
>   OPTIONAL column absent, with nothing executed
> - **Then** nothing panics, and validation accepts it or rejects it
>   with `InvalidQuery`
> - **And** lowering rejects exactly what validation rejects, with the
>   same error, and lowers what validation accepts to a plan, a
>   provably empty result, or an `InvalidQuery`, never a `Storage`
>   error carrying the engine's planning failure (`CLAUDE.md` §4.6)
> - **And** the canonical re-parse validates to the same verdict and
>   lowers to an equal plan or the same error
> - **And** any violation makes the target panic with a message
>   naming the scenario

## 6. Testing strategy

Per `CLAUDE.md` §6.2, the fuzz targets *are* the tests — coverage-guided
libFuzzer runs rather than fixed-input unit tests.

- **RFC0015.1** — the miner target's oracle is the same §3.3 invariant
  asserted by the existing property tests in
  `crates/ourios-miner/tests/invariants.rs` and the round-trip unit
  tests in `crates/ourios-miner/src/reconstruct.rs`; the fuzz target
  reuses that assertion under coverage guidance. Cross-referenced so the
  two layers stay in sync.
- **RFC0015.2 / .3 / .4** — panic-oracle targets. Verified by a bounded
  fuzz run (no crash) in CI; `cargo fuzz build` proves they compile even
  on runs where they are not executed.
- **RFC0015.5** — the `fuzz.yml` workflow itself; smoke budgets kept
  small enough to be non-flaky. The real coverage accrues from the
  Phase 2 continuous runs, not the per-PR smoke job.
- **RFC0015.6** — exercised the first time a crash is found; the
  committed reproducer is a standing corpus entry thereafter.

Each scenario id (`RFC0015.N`) is referenced from the corresponding
target source or workflow comment so the spec-to-test mapping is
greppable (`docs/verification.md` §2).

> **Amendment (2026-10-11, #951, maintainer-approved): testing the new
> oracles.**
>
> - **RFC0015.8** extends the panic-oracle bullet above. It mirrors
>   the `dsl/structured.rs` proptest and the `display.rs` corpus test
>   (`parse(serialize(q)) == q`), so the two layers stay in sync as the
>   grammar grows.
> - **RFC0015.9 / .10 and the RFC0015.1 amendment** run inside the
>   same bounded runs. Each oracle was shown to fire before landing:
>   the code under test was broken locally (a render mismatch, a
>   mislabelled marker, a dropped row, an absorbed literal, a rewritten
>   literal, a suppressed `Widened` audit event, a mis-encoded frame
>   kind, a non-round-tripping serialiser), a crafted input crashed
>   with the expected message, and the break was reverted. The seeds
>   in §3.3 keep every oracle on its happy path each run.
> - The RFC0015.5 bullet's "per-PR smoke job" predates §3.4's
>   schedule-only trigger. The smoke job runs on its schedule and on
>   dispatch, never per PR.

> **Amendment (2026-10-11, #955, maintainer-approved): testing
> RFC0015.11–.13.** Each oracle was shown to fire the same way before
> landing: a restore that moves the id allocator before rejecting, a
> snapshot writer that drops the wildcard route, a restore that drops
> the adopted templates, a manifest reader that skips `validate`, a
> high-water encoder off by one, a template-map
> writer that drops the alias classes, and `recover` reporting a corrupt
> snapshot as absent each crashed on a seed or a crafted input with the
> expected message, and were reverted.

> **Amendment (2026-10-11, #958, maintainer-approved): testing
> RFC0015.14.** The boundary was shown both ways before landing: the
> 147 `audit_reader` crash inputs found before #956 (eight upstream
> panic messages) and the `testdata/corrupt-parquet/` fixtures all run
> clean, and a panic injected into `AuditReader::read_all` crashed the
> seed with the scenario's "escaped the decode boundary" message, and
> was reverted. Four minimised inputs (an out-of-bounds offset, an
> empty slice, a negative column range, an unset dictionary decoder)
> stay as `regression_decoder_*` seeds, beside the #954 arrow-schema
> reproducer; `artifact_decode` gains the #957 unknown-slot-type
> reproducer. The first 300 s run found one out-of-memory the boundary
> cannot catch: parquet 59's footer decoder (`read_thrift_vec`)
> reserves a list's declared length before reading it, so a corrupt
> footer asks for gigabytes (#959). parquet 60 bounds that count by the
> remaining input. Until the DataFusion 56 upgrade (#846) brings it,
> the smoke run's 30 s budget for the target rarely reaches it (it can
> still report it as a crash), and ClusterFuzzLite does not stage the
> target (§3.2 as amended); the
> reproducer becomes a regression seed when #959 closes.

> **Amendment (2026-10-11, #964, maintainer-approved): testing
> RFC0015.15.** Each oracle was shown to fire before landing. A
> validation that returns `Storage` for an unsupported stage, a panic
> in the group-term lowering, a compile that lowers what validation
> rejected, and a default window read off the wall clock instead of
> the query clock each crashed a seed with the expected message, and
> were reverted. The `Storage` arm fired on real defects first: the
> seed `starts_with(body, "GET /")` (#960) and a run that mutated a
> seed into an invalid regex class (#961). Their minimized inputs
> become regression seeds once those fixes merge.

## 7. Open questions

Maintainer review (2026-06-19) gave direction on the following;
recorded here as the planned approach for the implementation PRs (to be
confirmed as the RFC advances toward `green`):

- [x] **Nightly pin** → **pin a dated `nightly-YYYY-MM-DD`** in the fuzz
  job (not a floating `nightly`), for reproducibility; Renovate bumps
  it like the other pinned toolchains.
- [x] **Smoke-fuzz budget** → **~60 s per target on PRs, ~300 s on the
  daily schedule** (see §3.4). Revisit if CI minutes or signal warrant. *(Amended
  2026-10-11, #951: the smoke job never runs per PR — ~300 s per target
  on its daily schedule, ~60 s on a manual dispatch, §3.4 as amended.)*
- [x] **`read_frame` exposure** → a **`fuzzing` cargo feature** on
  `ourios-wal` gating the `pub` export, rather than a `#[doc(hidden)]`
  shim — slightly cleaner and reusable for future non-fuzz tests.
- [x] **`OtlpLogRecord` construction** → expect a **hand-written
  `Arbitrary` impl** (or a thin newtype) for the string-body path,
  rather than relying on `derive` across the body variants, if a derive
  proves messy. *(Amended 2026-10-11, #951: neither — `miner_roundtrip` reads
  length-prefixed bodies off the raw bytes and builds each record with
  `..Default::default()`; `arbitrary` is no longer a direct dependency
  of the harness.)*

Still open, deferred to the implementation PRs:

- [ ] **A second miner target** driving *sequences* of records, to fuzz
  template **merge** behaviour (§3.1), not just single-line round-trip?
  Possible Phase 1.5. *(Amended 2026-10-11, #951: resolved inside
  `miner_roundtrip` rather than as a second binary — the input is a
  sequence of length-prefixed bodies into one tree, and the §3.1 checks run beside the §3.3
  one; §3.2 as amended, RFC0015.1 as amended, RFC0015.10.)*
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
