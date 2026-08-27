---
rfc: 0051
title: ourios-serving — shared serving infrastructure out of the ingest crate
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-27
supersedes: —
superseded-by: —
---

# RFC 0051 — `ourios-serving`: shared serving infrastructure out of the ingest crate

> **Status: `specified` (2026-08-27, maintainer sign-off on the §7
> decisions).** §3.2 resolved to **Option A** — one `ourios-serving`
> crate; the OIDC client moves with the OpenFGA client (core ends
> `reqwest`-free); the `ourios_ingester::receiver::*` re-export
> shims get one deprecation release. §5 criteria are written and
> testable; implementation may begin.
>
> *(`drafted`, 2026-08-27: Wave 3 of the structural review, epic
> #745.)* Touches no §2 pillar and
> no §3 invariant directly; §3.7 (multi-tenancy) constrains the
> extraction — every moved surface keeps its tenant parameter
> exactly. Prerequisites: RFC 0026/0027 (auth), RFC 0029 (OIDC),
> RFC 0030 (TLS), RFC 0046/0047 (out-of-band tenancy, ReBAC).

## 1. Summary

A new crate `ourios-serving` takes the role-independent serving
plumbing — the auth resolver, TLS settings and reloading acceptors,
and trace-context propagation — out of `ourios-ingester`, and takes
the reqwest-backed OpenFGA and OIDC clients out of `ourios-core`.
The move is placement-only: every auth decision, TLS handshake,
header name, metric, audit event and error text stays bit-identical.
After it, the querier role no longer compiles the ingest pipeline to
get an auth check, and the foundational-types crate carries no HTTP
stack.

## 2. Motivation

Two placements survived from the era when the receiver was the only
serving surface:

1. **Serving plumbing lives in the ingest crate.** `AuthResolver` /
   `AuthBinding` (`receiver/auth.rs`, 660 lines), `TlsSettings`
   (`receiver/tls.rs`, 226), the reloading TLS acceptors
   (`receiver/tls_serve.rs`, 456) and trace-context propagation
   (`receiver/propagation.rs`, 219) — 1 561 lines of
   role-independent serving infrastructure — sit under
   `ourios-ingester`. The querier role consumes all of it
   (`ourios-server`'s `querier.rs`, `mcp.rs`, `visibility.rs` import
   from `ourios_ingester::receiver::*`), so a querier-only deployment
   compiles and links the entire ingest pipeline to get an auth
   check and a TLS acceptor. `LISTENER_QUERIER` being defined inside
   the *receiver's* TLS module is the one-line picture of the
   problem.

2. **HTTP clients live in the foundational-types crate.**
   `ourios-core` carries the OpenFGA HTTP client
   (`auth/openfga/client.rs`, 1 628 lines) and the OIDC verifier
   (`auth/oidc.rs`, 1 078 lines), both `reqwest`-backed behind the
   `openfga` / `oidc` features. Core is depended on by every crate
   in the workspace; an HTTP client with TLS stack is the heaviest
   possible payload to put there, and feature-unification means most
   builds pay for it. "Shared types, tenant, IDs, errors" (§7 of
   `CLAUDE.md`) was never meant to include a REST client.

Both smells have a compile-feedback cost (RFC 0028's thesis: slow
feedback is a velocity killer) and a conceptual one: the dependency
arrows point *across* roles instead of *down* to shared
infrastructure.

### Non-goals

- No behaviour change anywhere. This is a placement RFC.
- The receiver's *pipeline* (`ingest_bound`, encode pool, WAL
  coupling) stays in `ourios-ingester` — only role-independent
  serving plumbing moves.
- No public-API redesign of the moved modules (the tower
  `AuthLayer` for HTTP and the `TenantDenied` error split are
  separate Wave 3 items, deliberately sequenced after this move).

## 3. Proposed design

### 3.1 What moves

A new crate `crates/ourios-serving` receives, as pure moves:

| From | To | Contents |
| --- | --- | --- |
| `ourios-ingester/src/receiver/auth.rs` | `ourios-serving/src/auth.rs` | `AuthResolver`, `AuthBinding`, `AuthError`, token/OIDC resolution |
| `ourios-ingester/src/receiver/tls.rs` | `ourios-serving/src/tls.rs` | `TlsSettings`, ALPN constants |
| `ourios-ingester/src/receiver/tls_serve.rs` | `ourios-serving/src/tls_serve.rs` | reloading acceptors, `LISTENER_*` labels, handshake metrics |
| `ourios-ingester/src/receiver/propagation.rs` | `ourios-serving/src/propagation.rs` | W3C trace-context extraction (RFC 0039) |
| `ourios-core/src/auth/openfga/client.rs` | `ourios-serving/src/openfga.rs` | the OpenFGA HTTP client |
| `ourios-core/src/auth/oidc.rs` (client half) | `ourios-serving/src/oidc.rs` | JWKS fetch + verification |

`ourios-ingester` re-exports the moved receiver modules for one
release (`pub use ourios_serving::… as …`) so downstream paths keep
compiling; the re-exports carry `#[deprecated]` and are removed in
the following breaking release (pre-production posture — see the
"break persisted layouts" precedent, but source-level paths get one
deprecation cycle because the Helm chart's users may pin git deps).

### 3.2 What stays, and the shape fork

**Stays in `ourios-core`:** the *config types* —
`OpenFgaConfig`, OIDC issuer/audience config, `AuthConfig` — which
`ourios-server`'s resolver and `ourios-config` need without any I/O.
After the move `ourios-core` has **no `reqwest` dependency and no
`openfga`/`oidc` cargo features**; the features migrate to
`ourios-serving`.

**The fork (maintainer decision):**

- **Option A (recommended): one crate `ourios-serving`** with
  modules `auth`, `oidc`, `openfga`, `tls`, `tls_serve`,
  `propagation`. One new crate in §7's layout; the dependency
  diamond is `ingester → serving`, `server → serving`,
  (`querier` stays serving-free — its role wiring lives in
  `ourios-server`). The name stretches slightly over the OpenFGA
  client (the *graph emitter* in the ingester writes tuples through
  it), but one crate is the smaller architectural commitment, and a
  later `ourios-authz` split remains cheap because the module
  boundaries land clean now.
- **Option B: two crates** — `ourios-authz` (auth, oidc, openfga)
  and `ourios-serving` (tls, tls_serve, propagation). Honest names,
  two arrows per consumer, two new crates in §7. Choose this only
  if the naming stretch of Option A is judged to matter more than
  the crate-count budget.

New-crate rule (§7 of `CLAUDE.md`): this RFC is the required
justification. The §7 layout list gains the chosen crate(s) when the
RFC is accepted — recorded here so the CLAUDE.md line edit rides the
acceptance rather than a separate waiver.

### 3.3 Dependency rules after the move

- `ourios-serving` depends on `ourios-core` (types),
  `ourios-config`, `ourios-telemetry`, `ourios-semconv` — never on
  `ourios-ingester`, `ourios-querier` or `ourios-parquet`.
- `ourios-ingester` and `ourios-server` depend on
  `ourios-serving`. `ourios-querier` does not (its role wiring in
  `ourios-server` does).
- §3.7 tenancy: every moved function keeps its tenant parameter and
  semantics byte-for-byte; the move is `git`-verifiable as pure
  relocation (function bodies unchanged).

## 4. Alternatives considered

- **Leave it**: the querier role keeps linking the ingest pipeline;
  every auth/TLS touch keeps recompiling `ourios-ingester` and
  everything above it. Rejected by the review's evidence (receiver.rs
  is the #3 churn file; worst-case warm check 24.5 s).
- **Move serving plumbing into `ourios-server`**: makes the binary
  crate a library for the ingester (inverted again) and defeats
  role-scoped compilation. Rejected.
- **Feature-gate the receiver modules inside `ourios-ingester`**:
  features don't fix dependency direction and multiply the CI
  matrix. Rejected.

## 5. Acceptance criteria

Scenario ids `RFC0051.<n>`, RFC0051.1–.7.

> **RFC0051.1 — the querier role sheds the ingest crate.** Given the
> workspace after the move, When `ourios-server`, `ourios-querier`
> and their tests are grepped for
> `ourios_ingester::receiver::{auth,tls,tls_serve,propagation}`
> paths, Then no match exists, And the querier role's modules build
> against `ourios-serving` only.
>
> **RFC0051.2 — core carries no HTTP stack.** Given
> `ourios-core/Cargo.toml` after the move, When inspected, Then it
> declares no `reqwest` dependency and no `openfga`/`oidc` feature,
> And `cargo tree -i reqwest` lists only the chosen serving crate
> (and dev-dependencies) as its workspace entry points.
>
> **RFC0051.3 — pure move, proven by the standing suites.** Given
> the moved modules, When their own test suites (auth resolver, TLS
> reload, propagation) and the RFC 0026/0027 (enforced tenancy + MCP
> binding), RFC 0029 (OIDC), RFC 0030 (TLS + mTLS + reload) and
> RFC 0039 (propagation) acceptance suites run, Then every scenario
> that passed before the move passes after it, with only import
> paths changed in the test code.
>
> **RFC0051.4 — end-to-end interop unchanged.** Given the
> collector-interop CI job (a real otelcol-contrib exporting over
> TLS + OIDC), When it runs against the moved crate layout, Then it
> passes unchanged.
>
> **RFC0051.5 — graph surfaces follow the client.** Given the
> OpenFGA client in its new home, When RFC 0047's real-container
> suite (12 scenarios) and the graph emitter / erasure paths run,
> Then all pass, And both paths import the client from the serving
> crate, not from `ourios-core`.
>
> **RFC0051.6 — compile feedback improves measurably.** Given a
> one-line edit to the moved `auth.rs`, When warm
> `cargo check -p ourios-server` and `-p ourios-querier` are timed
> before and after the move, Then the after-times are recorded in
> §9, And the querier-role path no longer rebuilds
> `ourios-ingester`.
>
> **RFC0051.7 — the shims die on schedule.** Given the deprecated
> `ourios_ingester::receiver::*` re-exports in the release the move
> ships in, When the next breaking release is cut, Then the
> re-exports are deleted, And a follow-up issue created at
> acceptance time tracks that removal.

## 6. Testing strategy

Per `CLAUDE.md` §6.2, mapped to the §5 ids:

- **RFC0051.1/.2** — mechanical gates: a grep assertion (CI step or
  a test over the source tree) and a `cargo tree -i reqwest` check;
  no new test code.
- **RFC0051.3** — the existing unit + integration suites of the
  moved modules, renamed paths only; the RFC 0026/0027/0029/0030/0039
  scenario tests run unmodified. No test is weakened or deleted
  (§6.2 "tests are specifications").
- **RFC0051.4/.5** — the existing testcontainers CI jobs
  (`collector-interop`, the RFC 0047 OpenFGA container suite) —
  end-to-end behaviour pins.
- **RFC0051.6** — a recorded measurement (script + numbers into §9),
  not a CI gate: wall-clock is machine-dependent; the *dependency*
  claim (querier path not rebuilding the ingester) is the assertable
  half, via `cargo build --timings` unit lists.
- **RFC0051.7** — release-process checklist item plus the tracking
  issue; not automatable before the release exists.

## 7. Open questions

All resolved at `specified` (2026-08-27, maintainer sign-off):

- [x] §3.2 shape — **Option A**: one `ourios-serving` crate. A later
      `ourios-authz` split stays cheap because the module boundaries
      land clean now.
- [x] The OIDC *client* moves with OpenFGA — it is what empties
      `reqwest` out of core (RFC0051.2).
- [x] Deprecation window for the `ourios_ingester::receiver::*`
      re-exports — **one release**, then deleted in the next breaking
      release (RFC0051.7).

## 8. References

- Epic #745 — the 2026-08-27 structural review (method + line-level
  evidence; Wave 3 item 1 is this RFC).
- RFC 0026 (auth & tenant binding), RFC 0027 (MCP surface),
  RFC 0029 (OIDC bearer layer), RFC 0030 (TLS/mTLS listeners),
  RFC 0039 (trace-context propagation) — the behaviour contracts the
  moved modules implement; their §5 suites are this RFC's
  no-regression pins.
- RFC 0046 / RFC 0047 — out-of-band tenancy and the ReBAC resolver;
  the OpenFGA client's consumers on both the ingest (graph emitter,
  erasure) and serving (visibility) sides.
- RFC 0028 — the build-feedback program; the compile-cost argument
  and the "modules change zero compilation units" rule this RFC's
  crate split deliberately escapes.
- `CLAUDE.md` §3.7 (multi-tenancy constraint on every moved
  surface), §7 (crate layout — gains the new crate on acceptance).

## 9. Validation

Filled as the criteria land. RFC0051.6's timing numbers go here.
