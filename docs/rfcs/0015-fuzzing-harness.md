---
rfc: 0015
title: Fuzzing harness — cargo-fuzz targets & ClusterFuzzLite CI
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-19
supersedes: —
superseded-by: —
---

# RFC 0015 — Fuzzing harness: cargo-fuzz targets & ClusterFuzzLite CI

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

### 3.4 CI — phased

**Phase 1 (this RFC's `green`): `.github/workflows/fuzz.yml`.** A bounded
smoke-fuzz job on a pinned nightly toolchain, run as a matrix over **all
four** targets — the parser targets are cheap, so there is no reason to
gate on the miner alone. Each matrix job runs its target for a short PR
budget and a longer scheduled budget, e.g.:

```sh
# PR (per target): ~60 s. Daily schedule: ~300 s. --target forces the
# gnu host triple (cargo-fuzz otherwise picks musl, whose static libc
# is incompatible with the ASan sanitizer).
cargo +nightly-2026-06-01 fuzz run <target> --target "$host" -- -max_total_time=60
```

It triggers on PRs that touch `ourios-miner` / `ourios-ingester` /
`ourios-wal` / `fuzz/` and on a daily schedule. `fuzz run` builds before
it runs, so a target that stops compiling fails its job; because every
target is always in the matrix (`fail-fast: false`), all four are built
and run on every invocation. A crash fails that target's job and uploads
the reproducer as an artifact. Top-level `contents: read` (the
workflow-token least-privilege pattern the other workflows follow).

**Phase 2 (follow-up PR): ClusterFuzzLite.** `.clusterfuzzlite/`
(`Dockerfile` + `build.sh` building the same cargo-fuzz targets) plus
`cflite_pr.yml` (short PR fuzzing against the persisted corpus),
`cflite_batch.yml` (longer scheduled runs that grow and persist the
corpus), and a coverage job. ClusterFuzzLite is what Scorecard's
`Fuzzing` check detects (it cannot see a bare cargo-fuzz directory), so
Phase 2 is what moves that check 0 → positive. Corpus-persistence
backend is an open question (§7).

### 3.5 Regression discipline

When the fuzzer finds a crash, the workflow per `CLAUDE.md` §6.2 is:
minimise the reproducer (`cargo fuzz tmin`), commit it as a permanent
seed under the tracked `fuzz/seeds/<target>/` (the working
`fuzz/corpus/` is gitignored, so a reproducer parked there would not
persist — §3.3), then fix the bug. The seed stays forever, re-checked
on every run — a found bug becomes a standing specification, never
silently dropped.

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

> **Scenario RFC0015.6 — a found crash becomes a permanent regression seed**
> - **Given** the fuzzer has found and the team has fixed a crash
> - **When** the fix lands
> - **Then** the minimised reproducer is committed under
>   `fuzz/corpus/<target>/` (or its `regressions/` subdir) and is
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
  proves messy.

Still open, deferred to the implementation PRs:

- [ ] **A second miner target** driving *sequences* of records, to fuzz
  template **merge** behaviour (§3.1), not just single-line round-trip?
  Possible Phase 1.5.
- [ ] **A structured-body round-trip target** exercising the §6.1
  canonical encoding (`AnyValue ↔ stored bytes` determinism), separate
  from `miner_roundtrip`'s string-body scope? Possible Phase 1.5.
- [ ] **Phase 2 corpus-persistence backend**: GitHub Actions cache vs a
  dedicated storage branch/bucket for ClusterFuzzLite?
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
