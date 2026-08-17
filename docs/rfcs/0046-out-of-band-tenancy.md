---
rfc: 0046
title: Out-of-band tenancy — the credential names the tenant, the data never does
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-17
supersedes: RFC 0045 (§3.1–§3.4); RFC 0003 §6.3 and RFC0003.3/.4; RFC 0001 §6.1 *Tenant derivation*
superseded-by: —
---

# RFC 0046 — Out-of-band tenancy

> **Status: `specified` (2026-08-17).** §5 criteria written and testable.
> Premise settled by the maintainer the same day: tenancy does not reside
> in OTLP data — no resource attribute (`service.name` included) is ever a
> tenancy input. That is also the #688 OTel-docs finding (OTel's own
> multi-tenancy is out-of-band: collector metadata routing + an auth
> extension, `headers_setter` → `X-Scope-OrgID`), which the #688 strawman
> and RFC 0045 drifted from for a zero-config default. RFC 0045 stays
> `green` as an implemented mechanism and is superseded by this RFC.

## 1. Summary

The tenant an export lands in is chosen **out of band**, on the ingest
request, never derived from the payload: an `X-Ourios-Tenant` header (HTTP)
or `x-ourios-tenant` metadata entry (gRPC), required on every export, that
must fall inside the credential's tenant set — the exact contract the querier
has enforced since RFC 0016/0026. One export = one tenant. Resource
attributes describe the *producer* (service, cluster, agent) and become
promoted columns and filters inside a tenant; they never partition storage.
The WAL frame carries the tenant it was acknowledged under, so replay needs
no derivation and RFC 0045's rule-epoch log disappears. `TenantId` stays an
opaque, coarse (org/team/workspace-scale) string — the storage partition,
the template-tree scope and the credential's blast radius remain one
concept (#688 Q1) — and it is the object type a ReBAC resolver (RFC 0047)
will bind to.

## 2. Motivation

**Deriving the tenant from the data was the wrong model, not a bad rule.**
RFC 0045 solved the `service.name` collision with a better key, but every
derived tenant is still a *function of producer descriptors*: `k8s.cluster.name`
and `service.name` say what emitted the record, not who owns it. That has
three costs the composite rule cannot remove:

- **Two sources of truth for "what is a tenant."** With an authority model
  (RFC 0026 tokens today; a relationship graph tomorrow — RFC 0047) the tenant
  is an object with owners; a derivation rule mints tenants from whatever the
  producer chose to send. Every mismatch between the two is a silent
  isolation gap, and the derived id corresponds to nothing in the graph.
- **The producer controls its own tenancy.** A misconfigured or hostile
  emitter picks its tenant by choosing attribute values, bounded only by the
  token set — the credential should pick, the data should not.
- **OTel says so.** Resource attributes are the entity model's description
  of the producer; multi-tenancy in the Collector is metadata (`X-Scope-OrgID`,
  `headers_setter`, batch-by-tenant metadata + auth extension) — never an
  attribute. Aligning keeps Ourios a drop-in target for the
  Collector's existing tenant routing.

The maintainer's ruling (2026-08-17) makes this the model: `service.name` is
to be totally unrelated to tenancy. This RFC applies it and retires the
derivation machinery.

## 3. Proposed design

### 3.1 The tenant selector

Every OTLP export names its tenant out of band:

| Transport | Carrier | Absent / empty | Not in the credential's set |
|---|---|---|---|
| OTLP/HTTP | `X-Ourios-Tenant` request header | `400 Bad Request` | `403 Forbidden` |
| OTLP/gRPC | `x-ourios-tenant` request metadata | `INVALID_ARGUMENT` | `PERMISSION_DENIED` |

The rule is the querier's (RFC 0016 §3.3, RFC 0026 §3.3), applied verbatim
to ingest: the header is **required on every export**, in open mode too
(there is no default tenant — RFC0003.4's "never invent a tenant" posture,
kept), and when auth is on the value must be a member of the resolved
binding's tenant set (a wildcard `*` set admits any non-empty value). A
single-tenant credential does not make the header optional: one rule for
both roles, nothing implicit; a Collector sets it once
(`otlphttp.headers` / `headers_setter`).

The value is an opaque `TenantId` — the same string the querier's header,
the MCP `tenant` argument and the token set already use — trimmed of
surrounding whitespace, non-empty, at most 256 bytes. No validation beyond
that; storage path-safety is `percent_encode_tenant` (RFC 0005 §3.4).

### 3.2 One export, one tenant

The per-`ResourceLogs` fan-out (RFC 0003 §6.3, RFC0003.3) is retired: every
record in the export carries the selected tenant. Missing `service.name` is
no longer a rejection — it is an absent (NULL) promoted column, exactly as
any other absent promoted attribute (RFC 0022; the "always promoted" rule
means the column exists, not that the value must). RFC0003.4's *shape*
survives as "no tenant selector ⇒ whole export rejected"; its *trigger* moves
from the payload to the request.

`service.name`, `k8s.cluster.name`, `gen_ai.*` and every other resource or
log attribute are what OTel says they are: descriptions of the producer and
the event, queryable (`service == "fluxcd"`), promotable (RFC 0022/0042),
sortable inside the tenant (RFC 0036) — never a partition key.

### 3.3 The WAL frame carries the tenant

Replay today re-derives tenants from the raw request; with no derivation
there is nothing to re-derive from, so the acknowledged frame must record
its tenant. RFC 0008 §6.2 reserves `kind > 0x02` for exactly this: a new
frame kind

```
kind = 0x03  TenantOtlpBatch
payload = u16 (little-endian) tenant byte length
        ‖ tenant bytes (UTF-8, as validated in §3.1)
        ‖ ExportLogsServiceRequest protobuf bytes (as OtlpBatch)
```

Everything else about the frame (header, CRC, `_pad`, torn-tail rules,
group-commit fsync, checkpoint/retain semantics) is unchanged; RFC0008.x
criteria hold as written because they are payload-agnostic. Replay decodes
the tenant prefix, materialises the request under it and feeds the miner as
before; the RFC 0045 rule-epoch log is deleted, not migrated.

**Legacy frames.** A `kind = 0x01` (`OtlpBatch`) frame has no recorded
tenant. Per the maintainer's persisted-layout ruling (nothing pre-production
is preserved) replay does not guess: encountering one aborts startup with an
error naming the frame's offset and the remedy (drain the WAL under the
previous version, or delete it). `0x01` stays a valid kind byte on the wire
— rejected as *unsupported for replay*, never as corruption — so RFC0008.5's
corruption classification is untouched.

### 3.4 What RFC 0045 leaves behind

| RFC 0045 piece | Fate |
|---|---|
| `TenantRule` / composite derivation, `receiver.tenant.rule` | **Removed.** No derivation exists. |
| Rule-epoch log `tenant_rule_epochs.json` | **Removed** (the frame carries the tenant). An existing file is ignored and may be deleted. |
| Divergence detector + `receiver.tenant.watch{,_capacity}`, `ourios.receiver.tenant.divergences`, the two events, `ourios.tenant.watch.*` | **Removed.** "One tenant spans several clusters" is a legitimate ownership shape once the credential picks the tenant; the ownership topology belongs to the graph (RFC 0047), not to a heuristic. Registry entries are **deprecated**, not deleted (semconv rule). |
| `Store::resolve` parse fix (RFC 0045 §3.2) | **Kept.** Layout-correctness, independent of the tenancy model. |
| `TenantId` opacity, `percent_encode_tenant`, per-tenant partitioning and template trees | **Kept.** |
| Helm `receiver.tenant` passthrough | **Removed** (the section is gone). |

### 3.5 Auth interaction

RFC 0026's whole-batch binding check becomes a header check: selector ∉
binding set ⇒ `403`/`PERMISSION_DENIED`, `ourios.ingest.batches` with
`error.type = permission_denied` and the `ingest_denied` audit event, all
unchanged; the per-`ResourceLogs` walk that produced the same result from
derived tenants is retired. RFC 0029 (OIDC) and RFC 0027 (MCP binding) are
untouched — they resolve the *set*; this RFC changes only where the
*selection* comes from on ingest. RFC 0047 will add a third resolver
(OpenFGA) behind the same seam, which is why the selector must be an opaque
`TenantId` and nothing more.

### 3.6 Collector interop

The reference Collector pipeline sets the header statically
(`exporters.otlphttp.headers.X-Ourios-Tenant`) or per-request via the
`headers_setter` extension from inbound context — the same shape as Loki's
`X-Scope-OrgID`. The interop test moves from "tenant derived from
`service.name`" to "tenant set by the exporter"; a pipeline that omits the
header gets a `400` it can see.

## 4. Alternatives considered

- **Keep derivation, add out-of-band as an override** (header wins when
  present, else derive). Rejected: two sources of truth is the problem, not a
  feature; every "else" branch is a silent path.
- **Header optional for single-tenant credentials.** Rejected for
  uniformity: the querier requires it, a Collector sets it once, and "the
  token implies the tenant" is one more implicit rule to document and test.
- **A trusted in-band tenant attribute** (`ourios.tenant` set by the
  Collector). Rejected as in #688 Q2: OTLP has no tenant field; routing
  metadata smuggled into the data model, and any producer can set it.
- **Tenant in the WAL segment header instead of per frame.** Rejected: a
  segment interleaves exports from many tenants under group commit; per-frame
  is the only correct granularity, and it costs 2 + |tenant| bytes.
- **Replay legacy `0x01` frames under `[service.name]`.** Rejected per the
  persisted-layout ruling; keeping the derivation code alive only for a
  replay path nobody in production has would preserve the model this RFC
  retires.

## 5. Acceptance criteria

Scenario ids `RFC0046.<n>`.

> **RFC0046.1 — selector required, both transports.** Given open mode and an
> export without a tenant selector, When it is sent over OTLP/HTTP (over
> OTLP/gRPC), Then it is rejected with `400` (`INVALID_ARGUMENT`) naming the
> header, and nothing reaches the WAL; And Given the same export with
> `X-Ourios-Tenant: acme` (`x-ourios-tenant` metadata), Then it is accepted
> and every record lands in tenant `acme`.

> **RFC0046.2 — binding check.** Given auth enabled with a token bound to
> `[acme]`, When an export selects `acme`, Then it is accepted; When it
> selects `globex`, Then the whole export is rejected `403`
> (`PERMISSION_DENIED`), `ourios.ingest.batches{error.type=permission_denied}`
> increments and the `ingest_denied` audit event names the token and tenant
> (the RFC0026.7 surface, unchanged); Given a `*` token, When an export
> selects any non-empty tenant, Then it is accepted.

> **RFC0046.3 — one export, one tenant; `service.name` is just an
> attribute.** Given an export whose `ResourceLogs` carry `service.name`
> `fluxcd`, `checkout`, and none at all, When it is sent with selector
> `acme`, Then all records are queryable under `acme` only, `service ==
> "fluxcd"` returns exactly the first group, the record without
> `service.name` has a NULL promoted service column and is returned by a
> tenant-wide query, and no other tenant exists.

> **RFC0046.4 — WAL frame carries the tenant.** Given exports acknowledged
> under selectors `acme` and `globex` whose records lack `service.name`
> entirely, When the receiver is `SIGKILL`ed before any flush and restarted,
> Then replay lands every record in the tenant it was acknowledged under, no
> record is lost, no record moves tenant, and every replayed frame is
> `kind = 0x03`.

> **RFC0046.5 — legacy frames abort loudly.** Given a WAL holding a
> `kind = 0x01` frame, When the receiver starts, Then startup aborts naming
> the offset and the remedy, and the frame is not classified as corruption
> (RFC0008.5's classification is unchanged).

> **RFC0046.6 — RFC 0008 invariants hold for the new kind.** Given the
> RFC0008.4/.5/.7/.8/.10 suites, When they run with `0x03` frames, Then they
> pass unchanged (torn tail, corruption, checkpoint/retain, group commit,
> recovery driver are payload-agnostic).

> **RFC0046.7 — selector hygiene.** Given selectors ` acme ` (whitespace),
> `` (empty), and one of 257 bytes, When exported, Then the first is accepted
> as `acme`, the other two are rejected `400`; And Given a selector
> containing `/`, `%` and a space, Then it round-trips: the export is
> accepted, stored under `tenant_id=<percent_encode_tenant>` and queryable
> under the same header value.

> **RFC0046.8 — Collector interop.** Given the reference `otelcol-contrib`
> pipeline exporting over TLS + OIDC with `X-Ourios-Tenant` set on the
> exporter, When it ships a batch, Then the records are queryable under that
> tenant; And Given the header removed from the pipeline, Then the Collector
> logs the receiver's `400`.

> **RFC0046.9 — derivation is gone.** Given the codebase, Then no
> `TenantRule`, `fan_out`, `RuleEpochs`, `DivergenceWatch` or
> `receiver.tenant.*` config remains; `ourios.receiver.tenant.divergences`,
> the two events and `ourios.tenant.watch.*` are `deprecated` in the
> registry; and the RFC 0003 / RFC 0026 / RFC 0045 tests that asserted the
> retired behaviour are replaced by the criteria above, each replacement
> named in the PR (CLAUDE.md §6.2 — a contract change made explicit).

> **RFC0046.10 — querier and MCP unchanged.** Given the RFC 0016 / 0026 / 0027
> query and MCP suites, When they run, Then they pass unchanged — the read
> side already was out-of-band.

## 6. Testing strategy

Unit tests in `ourios-ingester` for selector extraction (header/metadata,
trim, empty, length) and for the `0x03` frame codec (round-trip, tenant
prefix bounds, decode of a `0x01` frame → the unsupported-for-replay error).
RFC0046.1/.2/.3/.7 as `ourios-server` served-binary tests through both
transports and the querier (reusing the RFC 0045 harness shape); RFC0046.2's
telemetry half in the RFC0026.7 harness-exempt binary. RFC0046.4 on the
RFC0014.5 crash fixture with two tenants and no `service.name`. RFC0046.5/.6
in `ourios-wal` (`it/`), the existing RFC 0008 suites parameterised over the
frame kind. RFC0046.8 in the CI-only collector interop job. RFC0046.9 is a
`git grep` in the PR description plus the compile.

## 7. Open questions

- [ ] **Selector length bound (256 B)** — generous for opaque ids; is a
      shorter bound wanted so headers stay log-friendly?
- [ ] **Deprecation window for the RFC 0045 registry entries** — deprecate
      now and delete at the next minor, or keep indefinitely per semconv
      practice? (Leaning: keep deprecated; they were never released.)
- [ ] **RFC 0003 §6.3 text** — amend in place with a "superseded by RFC 0046"
      note, or leave and rely on this RFC's `supersedes:` line? (Leaning:
      amend in place; readers start from 0003.)

## 8. References

- #688 — the tenancy concept discussion; the OTel-docs finding that
  multi-tenancy is out-of-band (comment 1, point 2), and the OpenFGA
  resolver spike (`scratch/openfga-spike.md`).
- OpenFGA assistant review of the two-layer model
  (`scratch/openfga-ai-review-2026-08-17.md`) — tenant as coarse object,
  never per-conversation; the 2-step planner pattern RFC 0047 adopts.
- RFC 0045 — the in-band composite rule this RFC supersedes; its `Store`
  fix survives.
- RFC 0003 §6.3, RFC0003.3/.4 — the fan-out and rejection this RFC replaces.
- RFC 0008 §6.2 — frame kinds, reserved range, RFC0008.4/.5 classification.
- RFC 0016 §3.3, RFC 0026 §3.2–3.4, RFC 0027, RFC 0029 — the query-side
  contract ingest now mirrors; the resolver seam RFC 0047 extends.
- RFC 0022 — `service.name` always promoted (column exists; value may be
  NULL).
- OpenTelemetry Collector `headers_setter` extension and `otlphttp`
  exporter `headers`; Loki `X-Scope-OrgID` as prior art.
- `CLAUDE.md` §3.4 (WAL-before-ack), §3.7 (multi-tenancy), §6.2 (tests are
  specifications).

## 9. Follow-on (recorded, not built here)

**RFC 0047 — ReBAC resolver and graph-fed visibility.** OpenFGA as a third
`AuthResolver` producing the same `(name, tenant-set)` binding; the
relationship graph fed asynchronously from stored GenAI columns
(`gen_ai.conversation.id`, `user.hash`, `gen_ai.agent.id`); enforcement
inside a tenant by query rewrite at plan time (Check tenant-wide first,
bounded `ListObjects` otherwise, never per-record); content-vs-metadata
classes as separate relations mapped to column masking; erasure via
read-then-delete tuples riding the compaction rewrite. Nobody in the OTel
ecosystem has connected these dots yet: the GenAI semconv marks every
content attribute as PII-laden and the platform blueprint marks
multi-tenant compliance "help wanted" — the coarse-tenant + graph-visibility
split is the answer to both, and this RFC is its prerequisite.
