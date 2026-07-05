---
rfc: 0027
title: MCP query surface (agent-facing read tools over the querier)
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-05
supersedes: —
superseded-by: —
---

# RFC 0027 — MCP query surface (agent-facing read tools over the querier)

## 1. Summary

Expose the querier's read surface as a **Model Context Protocol
(MCP) server**, so LLM agents can query logs, inspect templates, and
run drift analysis as typed, discoverable tools instead of
hand-rolled HTTP calls. The surface is a thin adapter over what
already exists — the RFC 0002 DSL through the RFC 0016 endpoint
machinery, the RFC 0017 template registry, the RFC 0010 drift query
— hosted on the querier role's existing HTTP listener (streamable
HTTP transport) at `/mcp`. Read-only by design: no ingest, no
administration, no state mutation reachable through it.

Implementation is **gated on RFC 0026**: an MCP endpoint is exactly
the thing agents reach from laptops and CI over shared networks, and
it must not ship ahead of query-side authentication.

## 2. Motivation

- **The product story.** Ourios is an OTLP-native backend built in
  the open with agents; "point your agent at your logs" is the
  natural demo and the sharpest differentiator available to a
  pre-1.0 backend courting testers. Template mining is unusually
  agent-friendly: `list_templates` gives an agent the *shape* of a
  corpus in a few hundred rows — something raw-log backends cannot
  offer without a scan.
- **Tool typing beats API docs.** Agents can already hit the
  RFC 0016 JSON API, but every consumer must be taught the DSL, the
  tenant header, and the response shape by prompt. MCP moves that
  contract into the protocol: schemas are discovered, the DSL
  grammar ships as a resource, and errors are structured.
- **Cheap by construction.** The querier already owns the DSL
  parse → compile → run path and its HTTP hosting; the adapter adds
  tool plumbing, not query machinery. Hazard §4.6 (don't leak
  DataFusion through user surfaces) is inherited, already-solved
  behavior, not new work.

## 3. Design

### 3.1 Placement and transport

- A module in `ourios-server`'s querier role — **no new crate** (§7
  layout untouched; the adapter is small and shares the querier's
  types). The MCP SDK dependency (`rmcp`, the official Rust SDK)
  lives in `ourios-server` only.
- Transport: **streamable HTTP** on the existing querier listener at
  `/mcp`, enabled by a `querier.mcp.enabled` config flag (RFC 0020
  section; default **off**). No stdio transport in v1 — the querier
  is a deployed server, not a spawned subprocess; a local stdio
  bridge can be a later convenience (§7).
- Authentication: the RFC 0026 bearer scheme, identically to the
  JSON API. The token's tenant set bounds every tool call; the
  tenant is an explicit tool argument validated against that set.

### 3.2 Tool set (v1)

| Tool | Backs onto | Notes |
| --- | --- | --- |
| `query_logs` | RFC 0002 DSL via the RFC 0016 path | args: `tenant`, `query` (DSL string), optional `limit`; returns count + up to `limit` rendered rows + pruning stats |
| `list_templates` | RFC 0017 registry | args: `tenant`; returns `(template_id, rendered_template, version)` rows — the corpus's shape at a glance |
| `template_drift` | RFC 0010 drift surface | args: `tenant`, `from`, `to`; the audit-stream drift analysis over the half-open window `[from, to)` (RFC0010.2's boundary rule, inherited verbatim) |

Plus one **resource**: the DSL grammar/reference doc, served
verbatim so agents learn the query language from the protocol
rather than from prompt engineering.

Deliberately absent: any write, any admin (compaction, snapshots),
any raw-SQL escape hatch (hazard §4.6), and any cross-tenant
enumeration — there is no `list_tenants` tool; a token knows its
tenants out of band.

### 3.3 Output discipline

- Tool results are the RFC 0016 JSON shapes re-encoded as MCP
  content — one serialization boundary, no new response schema to
  drift.
- `query_logs` defaults to a conservative `limit` (rows are LLM
  context, not a data export); the full count always accompanies
  the rows so agents know what they're not seeing.
- **Returned log bodies are untrusted text.** A log line is
  attacker-influenceable input that will be placed into an LLM
  context; the server cannot sanitize meaning away, but the tool
  descriptions MUST carry the standard treat-as-data warning so
  well-behaved clients render results as content, not instructions.
  (This is a consumer-side hazard the RFC documents rather than
  solves; see §7.)

## 4. Alternatives considered

- **No MCP; agents use the JSON API directly.** Works today, loses
  discovery, typing, and the grammar-as-resource; every integration
  re-teaches the DSL by prompt. The adapter is small enough that
  "just use HTTP" saves little.
- **A separate `ourios-mcp` sidecar binary/crate.** Another artifact
  to version, deploy, and secure, wrapping an API that lives one
  process away. A module behind a config flag delivers the same
  surface with none of the operational spread. Revisit only if the
  MCP dependency tree bloats the server build measurably.
- **stdio-first transport.** Natural for laptop-local tools, wrong
  for a deployed backend: it would couple agent hosts to process
  lifecycle on the server host. Streamable HTTP is MCP's remote
  story and matches the existing listener.
- **Exposing SQL instead of the DSL.** Directly violates hazard
  §4.6 (DataFusion specifics leaking through a user surface) and
  widens the authz analysis from three tools to a query planner.

## 5. Acceptance criteria

Written at the `specified` gate (docs/rfcs/README.md lifecycle).
Proposed scenarios await maintainer sign-off in the drafting PR.

## 6. Testing strategy

Follows §5 at the `specified` gate.

## 7. Open questions

1. **Result pagination.** Whether `query_logs` grows a cursor for
   result sets past the row limit, or agents are expected to refine
   predicates instead (the DSL makes refinement cheap).
2. **stdio bridge.** A `ourios mcp-stdio --endpoint <url>` local
   proxy for clients that only speak stdio — convenience, not
   architecture; demand-driven.
3. **Prompt-injection posture.** Whether to offer an opt-in
   result-wrapping mode (e.g. explicit content fencing) beyond the
   tool-description warning, once client conventions settle.
4. **Aggregation tools.** `count by template` style pre-shaped
   tools vs teaching agents the DSL's pipe stages; start with the
   grammar resource and observe.

## 8. References

- RFC 0026 (authentication — the implementation gate), RFC 0002
  (the DSL surface exposed), RFC 0016 (the querier HTTP role this
  co-hosts on, incl. the `x-ourios-tenant` scoping), RFC 0017
  (template registry behind `list_templates`), RFC 0010 (drift),
  CLAUDE.md §4.6 (DSL vs engine leakage hazard), §3.7 (tenancy),
  §1 (scope — this stays a query surface, not a new product line);
  Model Context Protocol spec (streamable HTTP transport).
