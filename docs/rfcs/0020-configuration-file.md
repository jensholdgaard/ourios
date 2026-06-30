---
rfc: 0020
title: Server configuration file — YAML with environment-variable substitution
status: red
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-30
supersedes: —
superseded-by: —
---

# RFC 0020 — Server configuration file: YAML with environment-variable substitution

## 1. Summary

`ourios-server` gains a YAML configuration file, selected with
`--config <path>`, as the primary way to configure a deployment. The
file maps onto the same resolved `ServerConfig` the environment-variable
path produces today, and supports the OpenTelemetry Configuration
Working Group's environment-variable **substitution** model
(`${env:NAME}`, `${NAME}`, `${env:NAME:-default}`, `$$` escape;
scalar-only, non-recursive). The file is **authoritative**: when
`--config` is given, configuration comes from the file, and the
environment participates only through `${env:NAME}` / `${NAME}` references
inside it.
When `--config` is **absent**, the existing pure-`OURIOS_*`-env path is
used unchanged, so this is non-breaking.

## 2. Motivation

### 2.1 The env-only surface does not scale to a real deployment

Configuration today is ~15 `OURIOS_*` environment variables read in
`config_from_env()`. That is fine for a single container but awkward at
deployment scale: the Helm chart already wires a dozen env vars across
three workloads, there is no single artefact an operator can read, diff,
or version to see "how is this cluster configured", and adding a tunable
means threading another env var through every layer. A declarative file
is the artefact operators expect.

### 2.2 Match the ecosystem operators already know

Ourios is an OTLP-native backend; its operators run the OpenTelemetry
Collector, which is configured by a YAML file with `${env:…}`
substitution. Adopting the **same** file-plus-substitution data model
(rather than inventing one) means an operator's Collector instincts
transfer directly, and it keeps Ourios honest about dogfooding OTel
conventions. The Configuration WG has specified this substitution grammar
precisely, including the security-relevant edge cases (no YAML-structure
injection, no recursive expansion), so "mirror the spec" is a concrete,
testable target rather than a design space.

### 2.3 Why at this layer, and why now

This is the server's startup/config layer only — it changes how a
`ServerConfig` is *produced*, not what is configurable (that boundary is
RFC 0004) nor any data-path behaviour. It is self-contained: it has no
dependency on the storage, query, or miner subsystems and can land while
larger workstreams (e.g. the DataFusion/Arrow upgrade) are blocked. It
also unblocks a cleaner Helm chart (a `ConfigMap`-mounted file plus a
`Secret`-backed `${env:…}` for credentials) — the k8s-idiomatic shape.

## 3. Proposed design

### 3.1 Relationship to RFC 0004 and `ServerConfig`

RFC 0004 fixes *what* may be configured (the tunables-vs-invariants
boundary). This RFC fixes *how* that configuration is delivered. It adds
no new tunables and relaxes no invariant; it introduces a second
front-end that produces the **same** resolved `ServerConfig`
(`crates/ourios-server/src/main.rs`) the env path produces. There is one
config type and one set of validation rules downstream of resolution.

### 3.2 Selection and precedence

- A new CLI flag `--config <path>` names a YAML file.
- **`--config` present** → the file is the sole source of *Ourios's*
  configuration. Environment variables are consulted **only** where the
  file references them via `${env:NAME}` / `${NAME}` substitution (§3.3). A
  bare `OURIOS_*` env var does **not** override a value set in the file.
  (The standard `OTEL_*` SDK environment is a separate concern entirely —
  it configures Ourios's own telemetry SDK, never the data-plane config;
  see §3.8.)
- **`--config` absent** → the current `config_from_env()` path runs
  unchanged (reads `OURIOS_*` directly). This preserves today's
  behaviour exactly and keeps the change non-breaking.

The two modes are mutually exclusive by construction (the presence of
the flag selects the front-end); there is no per-key merge between a file
and direct env vars. This mirrors the Collector (the file is the
configuration; env is an injection mechanism, not an override layer) and
avoids a two-sources-of-truth precedence matrix.

### 3.3 Environment-variable substitution (mirrors the OTel Config WG)

Substitution follows the OpenTelemetry Configuration WG data model and
operates on the **parsed YAML scalar values**, not the raw text: the file
is parsed into a node tree first, then each scalar *value* has its text
substituted. Mapping keys are never candidates, and a substituted value is
never re-parsed into YAML structure (a mapping or sequence) — so
substitution can neither rewrite keys nor inject structure (rules 4–5
below are properties of this approach, not extra post-checks on a text
pass; the scalar's own type tag is still resolved, per rule 7). The
grammar for the subset this RFC supports (optional `env:` prefix +
optional `:-` default; self-contained, non-normative — the full ABNF is
the WG spec):

```text
REF      = "${" [ "env:" ] ENV-NAME [ ":-" DEFAULT ] "}"
ENV-NAME = [A-Za-z_][A-Za-z0-9_]*   ; the environment variable to resolve
DEFAULT  = any characters except "}", possibly empty ; used when ENV-NAME is unset or empty
```

Rules (each is an acceptance scenario in §5):

1. `${env:NAME}` and the prefix-less `${NAME}` are equivalent and both
   resolve `NAME` from the process environment.
2. `${env:NAME:-default}` / `${NAME:-default}` substitute `default` when
   `NAME` is unset **or empty**.
3. An undefined reference with no default resolves to the **empty
   string**. What that scalar then *is* follows rule 7: an unquoted empty
   scalar is read as YAML null, while a double-quoted one
   (`"${MISSING}"`) yields an empty string.
4. **Scalar-only**: substitution applies to scalar *values* only. A
   reference appearing in a mapping **key** position is left verbatim.
5. **Non-recursive**: a substituted value is used as-is and is **not**
   re-scanned — it can neither inject YAML structure (newlines/keys) nor
   trigger a second substitution. This is a security boundary, not a
   convenience limit.
6. `$$` is an escape for a literal `$`: `$${NAME}` yields the literal
   text `${NAME}` with no substitution.
7. **Type after substitution**: once a scalar's text is substituted, its
   type is resolved — a bare (unquoted) substituted scalar is
   re-interpreted by YAML's type rules and then deserialized into the
   target `ServerConfig` field, so `default_window_secs: ${env:W}` with
   `W=3600` yields the integer `3600`; a double-quoted scalar is forced to
   a string. Type interpretation therefore happens on the already-parsed
   scalar, after its value is substituted — never on a pre-parse text pass.
8. A `${…}` reference that does not conform to `REF` (e.g. `${1BAD}`,
   `${A$B}`), **encountered in a scalar value during substitution**, is a
   **whole-file parse error** — no partial
   resolution, no silent passthrough. Mapping keys are never substituted
   (rule 4), so a `${…}` in a key position is left verbatim whether or not
   it would conform.

The WG specification's worked input→output table (data-model §
*Environment variable substitution*) is adopted verbatim as the
conformance vector set (§6).

### 3.4 File schema

The YAML schema maps onto the resolved `ServerConfig`. Its top-level
grouping (`storage` / `receiver` / `querier` / `compaction`) deliberately
echoes the Helm chart's `values.yaml` for familiarity, though field names
follow the file's own snake_case convention rather than the chart's
camelCase:

```yaml
storage:
  backend: s3                       # local | s3
  s3:
    bucket: ${env:OURIOS_S3_BUCKET}
    endpoint: ${env:OURIOS_S3_ENDPOINT:-}   # empty → AWS regional endpoint
    region: us-east-1
    prefix: ""
    # Credentials are NEVER inline literals — only env references (§3.5).
    access_key_id: ${env:OURIOS_S3_ACCESS_KEY_ID:-}
    secret_access_key: ${env:OURIOS_S3_SECRET_ACCESS_KEY:-}
    session_token: ${env:OURIOS_S3_SESSION_TOKEN:-}
  local:
    bucket_root: /var/lib/ourios/data        # backend: local only

receiver:
  enabled: true
  grpc_addr: 0.0.0.0:4317
  http_addr: 0.0.0.0:4318
  wal_root: /var/lib/ourios/wal              # always local (RFC 0019 §3.1)

querier:
  enabled: true
  http_addr: 0.0.0.0:4319
  default_window_secs: 3600

compaction:
  enabled: true
  interval_secs: 300
```

Parsing is **strict**: unknown keys are a startup error (deny unknown
fields), matching RFC 0004's "small, deliberately bounded surface". The
same required/optional rules and value validation that
`build_store_config` / `build_*_config` enforce today apply unchanged to
the file-sourced values — there is exactly one validation path after
resolution (§3.1).

### 3.5 Secrets and hygiene (extends RFC 0019 §3.4)

Object-store credentials MUST NOT appear as inline literals in the file.
They are expressed only as env references (`${env:OURIOS_S3_SECRET_ACCESS_KEY}`),
which a deployment injects from a `Secret`. The existing invariant —
resolved credentials are never logged, and a config error names the
offending **key/path**, never a value (RFC 0019 §3.4, RFC0019.6) —
extends to the file path: substitution errors and schema errors report
the YAML key or env-var **name**, never the resolved secret text.

### 3.6 Crate placement

A new `config` module in `ourios-server` (no new crate; `ServerConfig`
already lives there). The substitution resolver is a pure
text→`Result<String, _>` submodule (`config/env_subst.rs`) with no
dependence on the schema, so it can be property-tested in isolation
against the WG vectors.

### 3.7 Helm chart follow-on (out of scope here)

Migrating the chart from a dozen env vars to a mounted `ConfigMap` +
`--config` + `Secret`-backed `${env:…}` is a follow-on tracked
separately; this RFC only adds the server capability. The chart change
is non-breaking-compatible because the env path remains.

### 3.8 Out of scope: the OTel SDK environment (`OTEL_*`)

The Ourios config file governs **Ourios's data-plane tunables only**. The
configuration of Ourios's *own* self-telemetry (its OpenTelemetry SDK —
RFC 0001 §6.8) is **not** modeled here: it is driven by the standard
`OTEL_*` environment variables, which the OTel SDK reads **directly** from
the process environment per the OpenTelemetry Environment Variable
Specification. There is no `otel:` section and no bespoke telemetry knob —
re-modeling those would duplicate (and drift from) a stable, language-
agnostic spec the SDK already implements. The relevant variables are, at
least:

- **General:** `OTEL_SDK_DISABLED`, `OTEL_SERVICE_NAME`,
  `OTEL_RESOURCE_ATTRIBUTES`, `OTEL_LOG_LEVEL`, `OTEL_PROPAGATORS`.
- **Exporter selection:** `OTEL_LOGS_EXPORTER` / `OTEL_METRICS_EXPORTER` /
  `OTEL_TRACES_EXPORTER`.
- **OTLP exporter:** `OTEL_EXPORTER_OTLP_ENDPOINT` (and per-signal
  variants), `…_PROTOCOL`, `…_HEADERS`, `…_TIMEOUT`, `…_COMPRESSION`,
  `…_CERTIFICATE` / `…_CLIENT_KEY`.

So `OTEL_*` is the one environment namespace that is deliberately *not*
absorbed into the file — it sits beside the file, consumed by the SDK.
(Consequence: the chart's current `otel.exporterEndpoint` value should
become a plain `OTEL_EXPORTER_OTLP_ENDPOINT` env passthrough — folded into
the §3.7 chart follow-on, not this RFC.)

## 4. Alternatives considered

### 4.1 Layered: env overrides file
A file as the base with direct `OURIOS_*` env vars overriding per key
(12-factor). Rejected: two ways to set every value and a precedence
matrix operators must keep in their heads; diverges from the Collector,
which our operators already know. The file-authoritative model with
`${env:…}` injection covers the same use cases (inject per-environment
values, keep secrets out of the file) without the ambiguity.

### 4.2 A bespoke substitution syntax (or none)
Inventing our own `{{VAR}}` templating, or only supporting whole-value
`$VAR`. Rejected: the OTel Config WG already specified this grammar
including the security edge cases (no structure injection, no recursion);
reusing it is less code, less surprise, and directly testable against a
published vector table. A bespoke syntax would re-litigate solved
problems and surprise Collector users.

### 4.3 TOML / JSON instead of YAML
Rejected: the Collector, the Helm `values.yaml`, and Kubernetes manifests
are all YAML; an operator configuring Ourios is already in YAML. JSON has
no comments; TOML is a third syntax in the stack.

### 4.4 A full Collector-style provider/URI scheme (`--config file:…|env:…|yaml:…`, multi-config merge)
The Collector accepts multiple `--config` URIs across providers and merges
them. Rejected as over-scoped for a single binary with a small bounded
surface: one `--config <path>` covers the need. The provider/merge model
can be revisited if a real multi-source requirement appears.

## 5. Acceptance criteria

Scenario ids `RFC0020.<m>`, referenced from the test code.

> **Scenario RFC0020.1 — a complete file resolves to the expected `ServerConfig`**
> Given a YAML file setting `storage.backend: s3` with a bucket, an
> enabled receiver with a `wal_root`, an enabled querier, and a compaction
> interval,
> When the server resolves configuration with `--config <that file>`,
> Then the resulting `ServerConfig` equals the one the equivalent
> `OURIOS_*` environment would produce, field for field.

> **Scenario RFC0020.2 — environment substitution follows the OTel Config WG model**
> Given a file whose scalar values use `${env:NAME}`, `${NAME}`,
> `${env:NAME:-default}`, a `$$`-escaped `$`, and a reference in a mapping
> **key** position,
> When the file is resolved with a known environment,
> Then `${env:NAME}`/`${NAME}` are replaced by the variable's value; the
> default is used when the variable is unset or empty; an undefined
> reference with no default becomes empty; `$$` yields a literal `$`; the
> key-position reference is left verbatim; and a substituted value is not
> re-scanned (no recursive expansion, no injected YAML structure).
> And the WG specification's published input→output vectors all hold.

> **Scenario RFC0020.3 — file is authoritative; bare env does not override**
> Given a file that sets `querier.default_window_secs: 1800`,
> When the server is started with `--config <that file>` and an
> environment that also sets `OURIOS_QUERIER_DEFAULT_WINDOW_SECS=3600`,
> Then the resolved value is `1800` (the file), and the bare env var has
> no effect.

> **Scenario RFC0020.4 — no `--config` preserves the env-only path**
> Given no `--config` flag,
> When the server resolves configuration from `OURIOS_*` variables,
> Then the resolved `ServerConfig` is identical to today's behaviour (the
> existing `config_from_env` scenarios continue to pass unchanged).

> **Scenario RFC0020.5 — invalid configuration fails fast**
> Given a file containing any of: a malformed substitution reference
> (`${1BAD}`), an unknown top-level key, or a value the existing
> validation rejects (e.g. `storage.backend: s3` with no bucket),
> When the server resolves it,
> Then startup fails with an error identifying the offending key or
> reference, and no partially-applied configuration is used.

> **Scenario RFC0020.6 — secret hygiene across the file path**
> Given a file referencing `secret_access_key: ${env:OURIOS_S3_SECRET_ACCESS_KEY}`
> with that variable set,
> When the configuration resolves and when a deliberately invalid sibling
> value triggers a config error,
> Then the resolved secret is never emitted to logs, and the error text
> names the YAML key / env-var **name** only — never the secret value
> (extends RFC 0019 §3.4 / RFC0019.6).

## 6. Testing strategy

Per `CLAUDE.md` §6.2.

- **Property tests** (`proptest`) for the substitution resolver
  (`config/env_subst.rs`, RFC0020.2): generate scalar text with arbitrary
  interleavings of literals, `${…}` refs, defaults, and `$$` escapes;
  assert the invariants (escape round-trips, non-recursion, scalar-only,
  undefined→empty). The OTel WG worked-example table is encoded as a
  fixed table test alongside the generators (the normative conformance
  vectors).
- **Unit tests** for schema mapping and validation (RFC0020.1/.3/.5):
  table of YAML inputs → expected `ServerConfig` or expected error; the
  file path and the env path are asserted to converge (RFC0020.1) and to
  diverge only as specified (RFC0020.3). Reuse the existing
  `build_store_config` / `build_*_config` validation tests as the shared
  oracle.
- **Regression** (RFC0020.4): the existing `config_from_env` unit tests
  run unchanged under "no `--config`".
- **Secret-hygiene test** (RFC0020.6): extends the RFC0019.6 redaction
  test to the file front-end (assert no secret substring in error/log
  output; the error names the key).
- No `criterion` benchmark — config resolution is a one-shot startup cost,
  not a hot path.

## 7. Open questions

- [ ] **`--config` vs `OURIOS_CONFIG`**: also accept an env var naming the
  config path (convenient for the chart), or flag-only? (Leaning
  flag-only to keep one selection mechanism; the chart passes the flag.)
- [ ] **Empty-vs-unset default semantics**: the WG model treats unset and
  empty identically for `:-default`. Confirm that matches our
  "trim, empty → unset" normalisation already used for `OURIOS_*`
  (it appears to; verify against `build_store_config`).
- [ ] **Strict unknown-key errors vs warn**: this RFC specifies error
  (deny unknown). Confirm no forward-compat need for tolerated-unknown
  keys (none expected pre-1.0).
- [ ] **Per-tenant overrides (RFC 0004 §3.4)**: out of scope here; the
  file configures the server globally. Note for a future RFC whether
  per-tenant tunables ever want a file representation.

## 8. References

- OpenTelemetry Configuration data model — Environment variable
  substitution:
  <https://opentelemetry.io/docs/specs/otel/configuration/data-model/#environment-variable-substitution>
- OpenTelemetry Collector configuration (env var expansion, `${env:…}`):
  <https://opentelemetry.io/docs/collector/configuration/>
- OpenTelemetry Environment Variable Specification (the `OTEL_*` SDK
  variables held out of the config per §3.8):
  <https://opentelemetry.io/docs/specs/otel/configuration/sdk-environment-variables/>
- Collector RFC, *Stabilizing environment variable resolution*:
  <https://github.com/open-telemetry/opentelemetry-collector/blob/main/docs/rfcs/env-vars.md>
- RFC 0004 — Configuration policy: tunables vs invariants (the *what*;
  this RFC is the *how*).
- RFC 0019 — Storage-backend selection (§3.4 credentials and secret
  hygiene; this RFC extends that invariant to the file path).
- `CLAUDE.md` §3.6 (object storage source of truth), §3.7 (tenancy),
  §5.1 (RFC process), §6.2 (testing).
