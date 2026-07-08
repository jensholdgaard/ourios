---
rfc: 0030
title: TLS/mTLS on the data-plane listeners
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-08
supersedes: —
superseded-by: —
---

# RFC 0030 — TLS/mTLS on the data-plane listeners

## 1. Summary

Ourios authenticates every data-plane surface (RFC 0026 static
bearers, RFC 0029 OIDC JWTs) but serves all of them over plaintext
TCP. Bearer credentials over plaintext are not auth — any on-path
observer can replay them. This RFC closes the gap identified as
"gates everything below" in the #331 epic:

1. **Server TLS on all three listeners** — OTLP gRPC (:4317), OTLP
   HTTP (:4318), and the querier HTTP surface (:4319, including the
   RFC 0027 `/mcp` route) — via **rustls** (already the workspace TLS
   stack; no OpenSSL link appears).
2. **Optional mTLS** per listener: a configured client CA turns on
   require-and-verify client-certificate authentication, as
   *transport* hardening. Identity stays with the RFC 0026/0029
   bearer layer — a client cert proves network admission, not tenant
   binding (deferred; §7.1).
3. **Certificate reload without restart**: cert/key pairs are re-read
   on a configurable interval so cert-manager-style rotation works
   with no dropped listener.
4. **Config mirrors the OTel Collector's `configtls` server model**
   (`cert_file`, `key_file`, `client_ca_file`, `min_version`,
   `reload_interval`) so operators configure Ourios exactly like the
   Collector in front of it.

TLS remains **opt-in per listener**: an unconfigured listener serves
plaintext, preserving the documented perimeter-trust deployment mode
(gateway/mesh terminates TLS) and every existing config. Enabling
auth on a plaintext listener logs a prominent startup warning (§3.4).

Touches hazard §4.6 adjacent surfaces (the listener layer in front of
the DSL) and the §3.7 tenancy perimeter indirectly (credential
confidentiality); no storage or query semantics change.

## 2. Motivation

- **Bearer tokens require confidentiality.** The OTel Collector's
  `bearertokenauth` extension "explicitly requires TLS" for exactly
  this reason; RFC 0026 §7 acknowledged the same and deferred the
  transport question to this RFC. Until it lands, the honest guidance
  for production is "put a TLS-terminating proxy in front" — workable
  but easy to skip silently.
- **The ecosystem default is native TLS.** Every Collector receiver
  takes a `tls:` block; operators pointing a Collector exporter at
  Ourios today must set `insecure: true`, which reads (correctly) as
  a warning sign.
- **mTLS is the fleet norm for collector→backend links.** Where an
  IdP is overkill (edge collectors with provisioned certs), a client
  CA is the established alternative; the Collector's server side
  supports `client_ca_file` for the same reason.
- **Rotation is not optional.** Kubernetes cert-manager renews
  certificates on a cadence; a listener that requires a restart to
  pick up a renewed cert turns rotation into an outage generator.

## 3. Design

### 3.1 Configuration (RFC 0020 amendment)

Each listener config grows an optional `tls` block with the
Collector's server-side field names and semantics:

```yaml
receiver:
  grpc:
    endpoint: 0.0.0.0:4317
    tls:
      cert_file: /etc/ourios/tls/server.crt     # required to enable TLS
      key_file: /etc/ourios/tls/server.key      # required alongside cert_file
      client_ca_file: /etc/ourios/tls/ca.crt    # optional: enables mTLS
      min_version: "1.2"                        # default; "1.3" allowed
      reload_interval: 5m                       # optional: never if unset
  http:
    endpoint: 0.0.0.0:4318
    tls: { ... }                                # same shape
querier:
  http:
    endpoint: 0.0.0.0:4319
    tls: { ... }                                # same shape, covers /mcp
```

Rules:

- `cert_file` and `key_file` come as a pair; one without the other is
  a config error at startup (named field in the message).
- `client_ca_file` without `cert_file`/`key_file` is a config error —
  mTLS presupposes server TLS.
- `min_version` accepts `"1.2"` (default) and `"1.3"` only. TLS 1.0
  and 1.1 are not implemented (rustls does not ship them; the
  Collector deprecates them).
- Paths may use `${env:VAR}` (RFC 0020 §3.5) like any other config
  value; the *file contents* are read at startup and on reload, never
  embedded in config.
- Unknown fields under `tls:` are rejected (RFC 0020 strict-mode
  parsing, unchanged).

### 3.2 Implementation shape

One shared `ourios-ingester`-side (receiver) and `ourios-server`-side
(querier wiring) seam:

- A `TlsSettings -> rustls::ServerConfig` builder in one place:
  certificate chain + key from the configured files, client CA into a
  `RootCertStore` + `WebPkiClientVerifier` when present, ALPN
  `h2`/`http/1.1` as appropriate per listener (gRPC requires `h2`).
- Both HTTP-family listeners (OTLP HTTP, querier axum router) accept
  through `tokio-rustls`'s `TlsAcceptor` in front of the existing
  hyper serve loop; the gRPC listener uses the same acceptor in front
  of tonic's `Server::serve_with_incoming` (tonic's own `tls` feature
  is not enabled — one rustls wiring for all three listeners instead
  of two).
- **Reload** (`reload_interval`): the acceptor holds an
  `ArcSwap<rustls::ServerConfig>`; a task re-reads the files on the
  interval and swaps on content change. In-flight connections keep
  their session; new handshakes see the new material. A reload
  failure (unreadable/invalid files) logs an error and keeps the last
  good config — it never takes the listener down.
- New dependencies: `tokio-rustls` + `arc-swap` (both already in the
  transitive tree via DataFusion/object_store; declared directly at
  the seam's home). `rcgen` as a dev-dependency to mint test CAs and
  leaf/client certs.

### 3.3 mTLS semantics

`client_ca_file` set ⇒ `RequireAndVerifyClientCert` (the Collector's
documented behavior for the same field): a handshake without a valid
client cert chain to that CA fails — the request never reaches the
auth layer. mTLS composes with, and does not replace, bearer auth:
the RFC 0026/0029 resolver still runs on every request that survives
the handshake. Client-cert identity extraction (SAN → tenant binding)
is deliberately out of scope (§7.1).

### 3.4 Plaintext + auth = warning

When any credential source (`auth.tokens` / `auth.oidc`) is enabled
and a listener has no `tls` block, startup logs one prominent warning
naming the listener ("bearer credentials over plaintext"). It is not
a hard error: TLS may legitimately terminate at a fronting
proxy/mesh. Whether a future major flips this to opt-out strictness
is an open question (§7.2).

### 3.5 What deliberately does not change

- The auth layer (RFC 0026/0029): resolvers, bindings, audit events,
  telemetry — untouched. TLS sits strictly below it.
- Open mode: a listener with neither `tls` nor credentials behaves
  exactly as today.
- Outbound TLS (object storage): already rustls via `object_store`;
  not this RFC.
- The Helm chart gains value plumbing (secret-mounted certs → `tls`
  blocks) in a follow-up chart release; the chart is not part of the
  acceptance gate here.

## 4. Alternatives considered

- **tonic's built-in `tls` feature for gRPC + separate axum-side
  wiring.** Two TLS stacks to configure and keep consistent; tonic's
  feature also pins its own rustls wiring. One `TlsAcceptor` in front
  of all three serve loops is smaller and uniform.
- **Terminate TLS only at the gateway, document, and skip native
  support.** The Loki model. Rejected: it leaves bearer tokens
  plaintext on every non-mesh deployment, contradicts the Collector
  norm our operators expect, and #331 explicitly scopes native TLS as
  the base everything else builds on.
- **SIGHUP-triggered reload instead of an interval.** Signals are
  awkward in containers (PID 1 handling) and unavailable on some
  targets; the Collector's `reload_interval` is the established
  shape. Interval it is.
- **Hard-fail auth-over-plaintext (§3.4 as an error).** Would break
  every current mesh-terminated deployment on upgrade; a warning
  preserves them while making the risk visible.

## 5. Acceptance criteria

Each criterion is a Given/When/Then that lands as a red test first
(RFC process §Red). Test CAs/certs are minted at test-time with
`rcgen` — no committed key material (house rule since the RFC 0029
fixture-key incident).

**RFC0030.1 — gRPC ingest over TLS.**
Given a receiver `grpc` listener with `cert_file`/`key_file` from a
test CA, When an OTLP gRPC client connects over TLS trusting that CA
and exports a batch, Then the export succeeds and the batch is
ingested; And When a plaintext gRPC client dials the same port, Then
the connection fails at the transport layer and nothing reaches the
auth layer or the WAL.

**RFC0030.2 — HTTP ingest over TLS.**
Same as RFC0030.1 for the `http` listener (`https://…:4318`),
plaintext `http://` request to the TLS port fails.

**RFC0030.3 — querier + MCP over TLS.**
Given a querier listener with TLS enabled, When a query request and
an MCP `initialize` arrive over TLS, Then both succeed; And a
plaintext request to the same port fails at the transport layer.

**RFC0030.4 — mTLS require-and-verify.**
Given a listener with `client_ca_file` set, When a client presents a
cert signed by that CA, Then the request proceeds (and still passes
bearer auth per RFC 0026); When a client presents no cert, Then the
handshake fails; When a client presents a cert from a different CA,
Then the handshake fails. Nothing about the three cases reaches the
request handler.

**RFC0030.5 — config validation.**
Given `cert_file` without `key_file`, or `client_ca_file` without a
server pair, or `min_version: "1.1"`, When the server starts, Then
startup fails with an error naming the exact offending field; Given
an unreadable or non-PEM `cert_file`, Then startup fails naming the
path.

**RFC0030.6 — certificate reload.**
Given a TLS listener with `reload_interval` set and an established
baseline connection, When the cert/key files are replaced with a new
pair (same CA) on disk and the interval elapses, Then new handshakes
serve the new certificate (observed via the peer certificate's serial)
without a process restart; And When the files are replaced with
garbage, Then new handshakes keep serving the last good certificate
and an error is logged.

**RFC0030.7 — plaintext-auth warning.**
Given `auth.tokens` configured and a listener without `tls`, When the
server starts, Then exactly one warning naming that listener is
emitted; Given the same listener with `tls` configured, Then no such
warning.

**RFC0030.8 — served end-to-end (Collector-shaped client).**
Given the served `ourios-server` binary with TLS on both receiver
listeners and mTLS on gRPC, When an OTLP exporter configured the
Collector way (`tls.ca_file` + client cert pair) exports over gRPC
and a second exporter over HTTPS, Then both batches land and are
queryable over the TLS querier — the full stack, no plaintext hop.

**RFC0030.9 — min_version enforcement.**
Given `min_version: "1.3"`, When a client attempts a TLS 1.2-only
handshake, Then the handshake is refused; a TLS 1.3 handshake
succeeds.

## 6. Testing strategy

- §5 arms live as integration tests in the owning crates
  (`ourios-ingester` for .1/.2/.4–.7/.9 receiver arms,
  `ourios-server` for .3/.8), joining the consolidated harnesses
  (RFC 0028) — no new test binaries.
- `rcgen` mints a CA + server/client leaves per test; nothing
  key-shaped is committed (RFC 0029 precedent).
- Reload (.6) drives a temp-dir cert swap and polls handshakes with a
  short interval — bounded, no wall-clock sleeps beyond the interval.
- TLS handshake overhead on the ingest hot path is measured
  indicatively on ci-runner (house bench rule) and recorded on the
  epic; it is a diagnostic, not a gate — TLS cost is a known,
  accepted tax.

## 7. Open questions

1. **Client-cert identity → tenant binding.** mTLS here is transport
   only. Mapping a client-cert SAN to an RFC 0026 `(name, tenants)`
   binding (the Envoy-style pattern) would make certs a third
   credential kind. Deferred until a deployment actually asks for it.
2. **Auth-over-plaintext as a hard error.** §3.4 warns. A future
   major could flip the default (opt-out via an explicit
   `allow_plaintext_credentials: true`), matching the Collector's
   bearertokenauth stance. Maintainer call, post-1.0 discussion.
3. **`cipher_suites` / `curve_preferences` exposure.** The Collector
   exposes both; rustls's defaults are deliberately safe and narrow.
   Left out until someone presents a compliance requirement.
4. **HTTP→HTTPS redirect / dual-listen.** Some operators expect the
   plaintext port to keep answering with a redirect during migration.
   Out of scope; a listener is either TLS or plaintext.

## 8. References

- #331 — the authn/transport epic this RFC advances ("TLS/mTLS on
  both listeners first (everything else depends on it)").
- RFC 0026 — authentication + tenant binding (accepted); RFC 0029 —
  OIDC bearer layer (green). The layers this RFC carries.
- OTel Collector `configtls` server settings — the config model
  mirrored here (cert_file/key_file/client_ca_file/min_version/
  reload_interval; `client_ca_file` ⇒ RequireAndVerifyClientCert).
- rustls / tokio-rustls — the TLS stack (already the workspace's via
  reqwest/object_store; no OpenSSL).
