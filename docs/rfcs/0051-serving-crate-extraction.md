---
rfc: 0051
title: ourios-serving — shared serving infrastructure out of the ingest crate
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-27
supersedes: —
superseded-by: —
---

# RFC 0051 — `ourios-serving`: shared serving infrastructure out of the ingest crate

> **Status: `drafted` (2026-08-27).** Wave 3 of the 2026-08-27
> structural review (epic #745). One §3 fork is a maintainer
> decision: the crate shape (§3.2, single crate vs. an
> `ourios-authz` / `ourios-serving` pair). Touches no §2 pillar and
> no §3 invariant directly; §3.7 (multi-tenancy) constrains the
> extraction — every moved surface keeps its tenant parameter
> exactly. Prerequisites: RFC 0026/0027 (auth), RFC 0029 (OIDC),
> RFC 0030 (TLS), RFC 0046/0047 (out-of-band tenancy, ReBAC).

## 1. Motivation

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

## 2. Non-goals

- No behaviour change anywhere: auth decisions, TLS handshakes,
  header names, metric names, audit events and error text are all
  bit-identical. This is a placement RFC.
- The receiver's *pipeline* (`ingest_bound`, encode pool, WAL
  coupling) stays in `ourios-ingester` — only role-independent
  serving plumbing moves.
- No public-API redesign of the moved modules (the tower
  `AuthLayer` for HTTP and the `TenantDenied` error split are
  separate Wave 3 items, deliberately sequenced after this move).

## 3. Design

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

Each criterion is a test or a mechanically checkable assertion:

- **RFC0051.1** `ourios-server`, `ourios-querier` and their tests
  contain no `ourios_ingester::receiver::{auth,tls,tls_serve,propagation}`
  path (grep gate in CI or a compile-time re-export removal); the
  querier role builds with `ourios-serving` only.
- **RFC0051.2** `ourios-core/Cargo.toml` has no `reqwest`
  dependency and no `openfga`/`oidc` feature; `cargo tree -i
  reqwest` shows only `ourios-serving` (and dev-deps) as its
  workspace entry points.
- **RFC0051.3** Pure-move verification: the moved modules' test
  suites pass unchanged (auth resolver unit tests, TLS reload tests,
  propagation tests — renamed paths only), and the RFC 0026/0027
  (enforced tenancy + MCP binding), RFC 0029 (OIDC), RFC 0030 (TLS +
  mTLS + reload) and RFC 0039 (propagation) acceptance suites are
  green before and after on the same corpus of scenarios.
- **RFC0051.4** The collector-interop job (real otelcol → TLS+OIDC
  ingest) passes unchanged — the end-to-end proof that no serving
  behaviour moved.
- **RFC0051.5** Graph surfaces: RFC 0047's real-container suite
  (12/12) passes with the OpenFGA client in its new home; the graph
  emitter and erasure paths import it from there.
- **RFC0051.6** Compile-feedback measurement recorded in §6: warm
  `cargo check -p ourios-server` and `-p ourios-querier` after a
  one-line edit to `auth.rs`, before vs. after (expected: the
  querier-role path stops rebuilding `ourios-ingester`).
- **RFC0051.7** The deprecated re-exports exist in the release the
  move ships in and are deleted in the next breaking release
  (tracked by a follow-up issue at acceptance time).

## 6. Validation

Filled as the criteria land. RFC0051.6's numbers go here.

## 7. Open questions / decisions record

| # | Question | Decision |
| --- | --- | --- |
| 1 | §3.2 shape: Option A (one crate) or Option B (pair)? | **maintainer fork — open** |
| 2 | Does the OIDC *client* move with OpenFGA (recommended: yes, it is what empties `reqwest` out of core)? | open |
| 3 | Deprecation window for the `ourios_ingester::receiver::*` re-exports: one release (recommended) or none (pre-production precedent)? | open |
