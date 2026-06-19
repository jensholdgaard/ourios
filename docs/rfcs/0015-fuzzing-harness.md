---
rfc: 0015
title: Fuzzing harness — cargo-fuzz targets & ClusterFuzzLite CI
status: specified
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
  `libfuzzer-sys::fuzz_target!` macro expands to `unsafe` glue. `CLAUDE.md`
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
| `miner_roundtrip` ⭐ | `MinerCluster::ingest` → `reconstruct::render` on a **string-body** record | `ourios-miner` | **invariant**: when `render` reports `Reconstruction::Faithful` the bytes equal the original string body; otherwise it reports `Reconstruction::RetainedVerbatim` (the body was retained, not reconstructed — §3.3's escape hatch) |
| `otlp_json` | `decode_json(&[u8])` | `ourios-ingester` | no panic; `Ok`/`Err` both fine |
| `otlp_protobuf` | `decode_protobuf(&[u8])` | `ourios-ingester` | no panic; `Ok`/`Err` both fine |
| `wal_frame` | `frame::read_frame(&mut Cursor::new(data))` | `ourios-wal` | no panic; malformed input yields a typed `FrameError`, never UB |

**`miner_roundtrip` is the centerpiece.** Rather than feed the miner a
fixed string, the target uses the `arbitrary` crate to build an
`OtlpLogRecord` whose body is a **`String`** (the Drain template path —
the fuzz bytes become the log line; attributes are derived alongside),
calls `MinerCluster::ingest`, then `render`s the mined record back and
asserts the §3.3 contract: when `render` returns
`Reconstruction::Faithful` the bytes are byte-identical to the original
string body; otherwise it returns `Reconstruction::RetainedVerbatim`
(the body was surfaced verbatim, not rebuilt — §3.3's "retain the
original body" path). A `Faithful` result whose bytes differ from the
original body makes the target panic, which libFuzzer reports as a crash
— turning the fuzzer into a search for reconstruction bugs, not just for
`unwrap`s.

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

Each target gets a seed corpus under `fuzz/corpus/<target>/`:

- `miner_roundtrip` and `otlp_json` seed from a small committed slice of
  the anonymised lines already in `testdata/corpus/`;
- `otlp_protobuf` seeds from a handful of valid `ExportLogsServiceRequest`
  encodings emitted by an existing ingester test;
- `wal_frame` seeds from valid frames written by the WAL writer.

Committed seeds are kept minimal (enough to bootstrap coverage); the
grown corpus is persisted by ClusterFuzzLite in Phase 2, not committed.

### 3.4 CI — phased

**Phase 1 (this RFC's `green`): `.github/workflows/fuzz.yml`.** A bounded
smoke-fuzz job, nightly toolchain, that for each target runs
`cargo +nightly fuzz run <target> -- -max_total_time=<N>`. It triggers
on PRs that touch `ourios-miner` / `ourios-ingester` / `ourios-wal` and
on a daily schedule. A crash fails the job; the crashing input is
uploaded as an artifact. The job also runs `cargo +nightly fuzz build`
unconditionally so a target that stops compiling is caught even when not
run. Top-level `contents: read`, job-scoped escalation only if needed
(the workflow-token least-privilege pattern the other workflows follow).

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
seed under `fuzz/corpus/<target>/` (or a dedicated `regressions/`
subdir), then fix the bug. The seed stays forever, re-checked on every
run — a found bug becomes a standing specification, never silently
dropped.

## 4. Alternatives considered

**`afl.rs` (AFL++) instead of cargo-fuzz/libFuzzer.** AFL++ is a capable
fuzzer, but cargo-fuzz/libFuzzer is the de-facto Rust default, has the
smoothest `cargo` integration, and is the engine ClusterFuzzLite and
OSS-Fuzz drive for Rust. Choosing it keeps Phase 1 and Phase 2 on one
engine.

**proptest only, no coverage-guided fuzzing.** proptest is retained and
valued, but its generators are hand-authored and do not instrument the
binary to steer toward new branches. It cannot substitute for
coverage-guided exploration of the parser/miner input space; the two are
kept as complementary layers.

**OSS-Fuzz instead of ClusterFuzzLite.** OSS-Fuzz requires a project to
be widely used or critical to the ecosystem; a pre-release backend will
not be accepted. ClusterFuzzLite is the self-hosted, runs-in-our-own-CI
equivalent and is available today. OSS-Fuzz remains a future option once
Ourios ships and has users.

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
>   built from `MinerConfig::default()`
> - **When** the target builds an `OtlpLogRecord` with a
>   **`String`** body from the arbitrary input, ingests it, and
>   renders the mined record back
> - **Then** the target asserts that when `render` reports
>   `Reconstruction::Faithful` the rendered bytes equal the
>   original string body, **and** that any other case reports
>   `Reconstruction::RetainedVerbatim` (the retained body, not a
>   reconstruction)
> - **And** an input for which a `Faithful` record renders to
>   bytes unequal to its string body makes the target panic (a
>   libFuzzer crash), surfacing the reconstruction bug
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

- [ ] **Nightly pin**: float `nightly`, or pin a dated
  `nightly-YYYY-MM-DD` for reproducibility and let Renovate bump it?
  (Lean: pin dated.)
- [ ] **Smoke-fuzz budget**: `-max_total_time` per target in PR CI
  (e.g. 60 s) vs the daily schedule (e.g. 5–10 min)? Balance signal
  against CI minutes.
- [ ] **`OtlpLogRecord` construction**: can `arbitrary` be `derive`d on
  the relevant `ourios-core` types, or is a hand-written `Arbitrary`
  impl (or a local newtype) needed given its field types?
- [ ] **`read_frame` exposure**: `#[doc(hidden)] pub` shim vs a
  `fuzzing` cargo feature on `ourios-wal`? (Lean: doc-hidden shim, to
  avoid feature-flag sprawl — confirm no clippy/API-surface concern.)
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
