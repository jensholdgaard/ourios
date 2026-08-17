---
rfc: 0047
title: ReBAC resolver (OpenFGA) and graph-fed visibility inside a tenant
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-17
supersedes: —
superseded-by: —
---

# RFC 0047 — ReBAC resolver and graph-fed visibility

> **Status: `specified` (2026-08-17).** §5 criteria written and testable.
> Prerequisite: RFC 0046 (out-of-band tenancy, `green`) — the tenant is an
> opaque, coarse, credential-selected object, which is exactly the object
> type this RFC binds the authorization graph to. Grounded in the #688
> OpenFGA spike (resolver seam holds, p50 1.4 ms), the two OpenFGA-assistant
> reviews (`scratch/openfga-ai-review-2026-08-17.md`) and the agent-scale
> `ListObjects` spike (`scratch/openfga-spike-2-listobjects.md`) whose
> central finding — the 1000-object cap is **silent** — shapes §3.4.

## 1. Summary

Add OpenFGA as a third `AuthResolver` (beside static tokens and OIDC) and,
on top of the same coarse tenants, a **visibility layer inside a tenant**
driven by a relationship graph that is **fed from the telemetry itself**.
Layer 1 (unchanged from RFC 0046): the tenant is the storage partition, the
template-tree scope and the credential's blast radius, resolved once per
session into the existing `AuthBinding { name, tenant-set }`. Layer 2 (new):
the OpenTelemetry GenAI identifiers already stored as promoted columns —
`gen_ai.conversation.id`, `user.hash` / `enduser.pseudo.id`,
`gen_ai.agent.id`, `gen_ai.workflow.name` — are graph nodes; enforcement is
**query rewrite at plan time**, never per-record checks: `Check(principal,
can_read_content, tenant)` first, and only for principals *without*
tenant-wide read a bounded, streamed `ListObjects(conversation)` that becomes
an `IN (…)` predicate over the promoted column, failing closed past an
explicit bound. Content and metadata are separate relations, mapped to
column masking. Agents are first-class principals; MCP tools are graph
objects (`can_call`). Tuples are written asynchronously from stored data by a
tenant-scoped emitter riding the compaction pass; erasure removes them the
same way. Nobody in the OpenTelemetry ecosystem has connected these dots:
the GenAI semconv marks every content attribute as PII-laden and the OTel
platform blueprint marks multi-tenant compliance "help wanted" — coarse
out-of-band tenants plus graph-fed visibility over the GenAI node ids is the
answer to both.

## 2. Motivation

**The facts we need are relationships, and they are already in the data.**
"Alice participated in conversation C", "agent A acted in C", "team T owns
cluster K", "C belongs to tenant X": these are written when telemetry is
ingested (or when an admin grants ownership), not computed from request
attributes. Every question the product asks — may this principal read
these conversations, may this agent read only its own, may FinOps read the
spend metadata but no prompt content, may this collector write into this
tenant — is a graph traversal with hierarchy (tenant → conversation →
participant), per-resource sharing and reverse lookup ("which conversations
may I read"): the three canonical ReBAC indicators. RBAC-with-scopes cannot
express "only conversations I took part in"; ABAC/policy engines evaluate
attributes at request time and would need the graph reconstructed as
attributes on every request. ReBAC is the model; OpenFGA is the engine we
spiked and reviewed.

**Why now, and why on RFC 0046.** With in-band derivation (RFC 0045) the
tenant was a function of producer descriptors and corresponded to nothing in
an authority graph — two sources of truth for "what is a tenant". RFC 0046
made the tenant an opaque object chosen by the credential; that object *is*
the graph's `tenant` type, so layer 1 needs no translation and layer 2 can
hang everything off `parent: [tenant]`.

**Why the split matters.** Tenants must stay coarse (org/team/workspace):
they are Parquet partitions and template trees, and one per user or
conversation is hazard #4 (small files) plus RFC 0023 memory in one move.
Fine-grained visibility therefore cannot be more tenants; it must be
enforcement inside a tenant — which the graph gives us without touching
storage.

**The community gap.** GenAI logs are chat-history stores; the semconv says
so on every content attribute. Multi-tenant compliance is "help wanted" in
the OTel platform blueprint, and the ReBAC engines' agent-authorization
guidance stops at "authorize the tool call". Feeding an authorization graph
*from* the observability data it protects — conversation, user, agent
identifiers as nodes — closes the loop between "who may query this
telemetry" and "what the telemetry says happened", and is a contribution
avenue once validated here (RFC 0046 §2 / #688 Q7).

## 3. Proposed design

### 3.1 Layer 1 — the resolver

A third `AuthResolver`, `openfga`, behind the RFC 0029 seam. Configuration:

```yaml
auth:
  openfga:
    api_url: http://openfga.auth.svc:8080
    store_id: 01M07RYMXRDW4ND5M7XQV04W8R
    authorization_model_id: 01M07RZE9RHPVPTYCV22RX0TDA   # pinned; empty = latest
    api_token: ${env:OURIOS_OPENFGA_TOKEN}                 # ${env:…} only, RFC 0026 rule
    session_ttl_secs: 60
    consistency: minimize_latency    # or higher_consistency (bypasses OpenFGA's cache)
```

At session establishment (bearer resolved by the static or OIDC path first
— OpenFGA does not authenticate, it authorizes) the resolver maps the
credential to a **principal**: a static token → `service_account:<name>`,
an OIDC subject → `user:<sub>` unless the token carries the configured
agent claim (`auth.oidc.agent_claim`, e.g. `ourios_principal_type=agent`) →
`agent:<sub>`. It then issues `ListObjects(principal, can_read_content,
tenant)` and `ListObjects(principal, can_write, tenant)` (bounded, streamed
— tenant sets are small by construction) and produces the same
`AuthBinding { name, tenant-set }` RFC 0026 enforcement consumes; a
principal with no readable and no writable tenant is unbound (401-class).
The binding is cached per credential for `session_ttl_secs`, **fail-closed**:
an OpenFGA error or timeout during resolution is a `503` on the query side
and `UNAVAILABLE`/`503` on ingest, never an open door, and never a stale
grant past the TTL. Revocation latency ≡ TTL; `higher_consistency` bypasses
OpenFGA's own cache after writes. Ingest authorization is exactly RFC 0046
§3.5: the out-of-band selector ∈ the resolved write set.

### 3.2 The authorization model

Kept **in-tree** — `deploy/openfga/model.fga` and `deploy/openfga/
store.fga.yaml` land with this RFC, and CI's `openfga-model` job runs
`fga model validate` + `fga model test` on them (a required check, the way
`semconv/registry` is gated by weaver) — so the model is a tested artefact
from the day it is specified, not prose:

```fga
model
  schema 1.1

type user
type agent
type service_account

type team
  relations
    define member: [user, service_account, team#member]

type tenant
  relations
    define owner: [user, service_account, team#member]
    define writer: [service_account, team#member]
    define reader: [user, service_account, team#member]
    define metadata_reader: [user, service_account, team#member]
    define can_write: writer or owner
    define can_read_metadata: metadata_reader or reader or owner
    define can_read_content: reader or owner

type conversation
  relations
    define parent: [tenant]
    define participant: [user]
    define actor: [agent]
    define delegate: [agent]                       # acts on behalf of a participant
    define can_read_metadata: participant or actor or delegate or can_read_metadata from parent
    define can_read_content: participant or actor or delegate or can_read_content from parent

type tool
  relations
    define parent: [tenant]
    define caller: [user, agent, service_account, team#member]
    define can_call: caller or can_read_content from parent
```

Reviewed shape (OpenFGA assistant, both reviews): tenant as the object every
resource belongs to; `team#member` nesting; permissions as `can_*`;
`X from parent` inheritance; agents as first-class principals with explicit,
revocable delegation (`delegate`) rather than copied permissions; content vs
metadata as separate relations (not CEL conditions — those stay for
time-boxed grants if ever needed); one OpenFGA store per deployment. Cluster
and service ownership (`cluster`, `service` types from the review) are
**deferred** to a follow-up: with RFC 0046 they are producer descriptors
inside a tenant, and no v1 scenario needs them as objects.

### 3.3 Feeding the graph from the data

Tuples of the data-derived layer are written by an **emitter** in the
compaction sweep (RFC 0009 §3.2 — the one place that already walks every
tenant's Parquet, promoted columns included), tenant-scoped, idempotent and
batched (≤100 tuples per transactional `Write`, chunked non-transactionally;
duplicates ignored):

| From promoted column(s) | Tuple |
|---|---|
| every distinct `gen_ai.conversation.id` in tenant T | `conversation:<id>#parent@tenant:T` |
| (`gen_ai.conversation.id`, `user.hash` or `enduser.pseudo.id`) | `conversation:<id>#participant@user:<hash>` |
| (`gen_ai.conversation.id`, `gen_ai.agent.id`) | `conversation:<id>#actor@agent:<id>` |

Object ids are **tenant-prefixed** (`conversation:T/<id>`): OpenFGA object
ids are opaque strings in one store, so the same raw conversation id in two
tenants must be two objects; the `parent` tuple carries the tenant edge and
the planner strips the prefix when it builds predicates (§3.4). A pure
naming rule, mirrored in exactly two places (emitter, planner). Ownership
tuples (`tenant#owner/writer/reader`, `team#member`, `tool#caller`,
`conversation#delegate`) are administrative: written by operators through
OpenFGA's own API/CLI, never by Ourios.

**Freshness.** A conversation is invisible to fine-grained principals until
its tuples land (seconds after the next sweep; the emitter is also invoked
on the receiver's flush cadence for the tenants it flushed). Two bridges,
both in the planner: (a) the **self fast path** — a `user:` principal always
gets `user.hash == <principal>` as an additional OR-predicate without
consulting the graph; (b) **contextual tuples** — a request may carry
`{conversation:<id>#participant@<principal>}` for ids it just created,
passed on `Check`/`ListObjects` and never persisted. Tenant-wide readers
(the FinOps/operator case) never wait: they resolve at layer 1.

### 3.4 Layer 2 — query rewrite at plan time

For a query over tenant T by principal P, the planner runs the **two-step**:

1. `Check(P, can_read_content, tenant:T)` (cached with the session, TTL as
   §3.1). **Allowed ⇒ the tenant partition predicate only** — no
   enumeration, no change to today's plan.
2. Otherwise `StreamedListObjects(P, can_read_content, conversation)` — the
   streamed variant, never plain `ListObjects`: the spike showed the plain
   call **returns HTTP 200 with 1000 objects and no truncation marker** for a
   tenant-wide reader over 100 000 conversations (13 ms) — a planner that
   enumerated for such a principal would emit a *wrong* predicate silently.
   The stream is consumed up to `auth.openfga.visibility.max_objects`
   (default 10 000); reaching the bound **fails the query closed** with a
   named error ("visibility set exceeds N objects; ask for tenant-wide read")
   rather than emitting a giant `IN`. The ids (with the tenant prefix
   stripped) become `attr.gen_ai.conversation.id IN (…)` — over the promoted
   column, so RFC 0022/0042 pruning still applies — OR'd with the §3.3 self
   fast path and any contextual-tuple ids. A principal with an empty set and
   no fast path gets an empty result, not an error.
3. `can_read_metadata` without `can_read_content` ⇒ the same predicate plus
   **column masking**: the configured content columns
   (`auth.openfga.visibility.content_columns`, default the GenAI content
   attributes `gen_ai.input.messages`, `gen_ai.output.messages`,
   `gen_ai.system_instructions`, `gen_ai.tool.call.arguments`,
   `gen_ai.tool.call.result` and `body`) are projected as NULL, and a query
   that filters or aggregates on a masked column is rejected (`403`, named
   column) rather than answered from data the principal may not read.

Configuration binds object types to columns explicitly — nothing is
inferred:

```yaml
auth:
  openfga:
    visibility:
      objects:
        - type: conversation
          column: attr.gen_ai.conversation.id
      self_principal_column: attr.user.hash          # the §3.3 fast path
      content_columns: [body, attr.gen_ai.input.messages, attr.gen_ai.output.messages]
      max_objects: 10000
```

Per-record `Check` calls in the scan path are **never** performed (the
architectural line from the first spike, confirmed by both reviews).

### 3.5 MCP tools as objects

Every RFC 0027 tool (`query_logs`, `list_templates`, `template_drift`, the
FinOps trio) is a `tool:<name>` object per tenant (`tool:T/<name>#parent@
tenant:T`). Before dispatch the MCP server issues `Check(P, can_call,
tool:T/<name>)`; a tenant-wide reader can call everything, a narrowly scoped
principal only what it was granted (`caller`). Time-boxed grants are the
one place CEL conditions belong (a `temporal_grant` condition on `caller`) —
recorded, not built in v1. The tool call's own data access then goes through
§3.4 like any query, so an agent calling `query_logs` reads exactly its own
conversations.

### 3.6 Erasure (Q9)

Conversation-scoped erasure (RFC 0045 §9 / #688 Q9) rides the compaction
rewrite that removes the rows: the same pass reads the object's tuples
(`Read` by object) and deletes them in ≤100 chunks — no wildcard delete
exists — so a deleted conversation is unreachable *and* unlisted. Tuple
deletion follows the Parquet rewrite, never precedes it (a dangling tuple is
harmless; a dangling row is a leak).

### 3.7 Operational posture

Fail-closed everywhere OpenFGA is consulted (resolution, planner, tool
gate); one store per deployment; the model id pinned in config and bumped
by PR (the `.fga.yaml` tests run against the pinned model in CI); OpenFGA
availability is an operational cost the deployment opts into by configuring
`auth.openfga` at all — static tokens and OIDC keep working without it, and
a deployment that wants coarse tenants only never touches this RFC.

## 4. Alternatives considered

- **RBAC with scopes** (`tenant:read`, `tenant:content`). Covers layer 1 —
  it is what static tokens already are — but cannot express "only
  conversations I took part in" or "only my agent's runs" without a role
  per principal per conversation. Rejected for layer 2, kept as what the
  static/OIDC resolvers remain.
- **ABAC / policy engine (OPA, Cedar) inside the app.** Attribute-time
  evaluation; the relationships would have to be reconstructed as request
  attributes on every query, i.e. the graph rebuilt per call. Rejected as
  the primary model; OPA stays the right tool for infra-layer policy
  (admission, mesh) if ever needed — outside this RFC.
- **Per-record `Check` in the scan.** Millions of calls per query; the
  availability coupling RFC 0029 §4 rejected for introspection. Rejected —
  the planner is the only enforcement point.
- **Fine-grained tenants (one per conversation/user).** Breaks storage
  (hazard #4) and RFC 0023 memory; the multi-tenant guidance says keep
  tenants coarse. Rejected.
- **Plain `ListObjects` with a big cap.** Silent truncation makes it a
  correctness hole, not a tuning knob (spike 2). Rejected in favour of the
  two-step + streamed, bounded, fail-closed enumeration.
- **Deriving tuples at ingest (in the receiver hot path).** Adds an external
  write to the WAL-before-ack path. Rejected: the compaction pass already
  walks the data, and the freshness bridges (§3.3) cover the lag.
- **A second source of truth for tenants** (graph + derived). Removed by
  RFC 0046; this RFC depends on there being exactly one.

## 5. Acceptance criteria

Scenario ids `RFC0047.<n>`. Integration tests run against a real OpenFGA
container (testcontainers, like Dex for RFC 0029) with the in-tree model.

> **RFC0047.1 — resolver binding.** Given `auth.openfga` configured and
> tuples granting `user:alice` reader on `tenant:acme` and
> `service_account:collector` writer on `tenant:acme`, When alice's OIDC
> bearer establishes a session, Then her binding's read set is `{acme}` and
> write set empty; And When the collector's token establishes one, Then its
> write set is `{acme}`; And Given a principal with no tuples, Then the
> session is unbound (401-class) — never an empty-but-open binding.

> **RFC0047.2 — ingest binding through the resolver.** Given the collector
> above, When it exports with selector `acme`, Then it is accepted; with
> selector `globex`, Then `403`/`PERMISSION_DENIED` (RFC 0046 §3.5, RFC0026.7
> telemetry unchanged).

> **RFC0047.3 — fail closed.** Given the resolver configured and OpenFGA
> unreachable (or timing out), When any principal establishes a session or
> a cached session's TTL expires, Then queries answer `503` and ingest
> `503`/`UNAVAILABLE`, nothing is admitted, and `ourios.auth.resolutions`
> counts `error.type = upstream_unavailable`.

> **RFC0047.4 — two-step, tenant-wide reader.** Given alice with
> `can_read_content` on `tenant:acme` and 10 000 conversations in `acme`,
> When she queries, Then the plan carries only the tenant predicate, no
> `ListObjects`/stream call is issued (asserted on the OpenFGA request log
> / a counter), and every row is returned.

> **RFC0047.5 — two-step, participant.** Given `user:bob` participant of
> conversations `c-1` and `c-2` only (tuples), When bob queries `true`,
> Then exactly the rows of `c-1`/`c-2` return; And Given a further
> conversation `c-9` whose rows carry `user.hash = bob` but no tuple yet,
> Then those rows also return (self fast path); And Given a contextual tuple
> for `c-10` on the request, Then `c-10`'s rows return too.

> **RFC0047.6 — agent as principal.** Given `agent:bot` actor on 500
> conversations and `agent:other` actor on 500 different ones, When bot
> queries, Then exactly its 500 conversations' rows return and none of
> other's; And Given bot is also `delegate` on alice's conversation `c-7`,
> Then `c-7` returns for bot; And When the delegation tuple is deleted,
> Then (after the TTL) `c-7` no longer returns.

> **RFC0047.7 — bounded enumeration fails closed.** Given
> `visibility.max_objects: 100` and an agent actor on 150 conversations,
> When it queries, Then the query is rejected with the named bound error,
> no partial result is returned, and the plain (capped) `ListObjects` is
> never used — the streamed call is what the OpenFGA log shows.

> **RFC0047.8 — metadata without content.** Given `user:fin` with
> `metadata_reader` on `tenant:acme` only, When fin queries
> `sum(attr.cost_usd) by attr.model`, Then it succeeds; When fin selects or
> filters on `body` or `attr.gen_ai.input.messages`, Then rows carry NULL
> for those columns and a filter on them is `403` naming the column.

> **RFC0047.9 — tool gate.** Given `agent:bot` with `caller` on
> `tool:acme/query_logs` only, When it calls `query_logs`, Then the call
> proceeds (and §3.4 scopes its data); When it calls `template_drift`,
> Then the MCP error is a permission denial naming the tool.

> **RFC0047.10 — emitter.** Given a tenant whose Parquet holds records
> with `gen_ai.conversation.id`, `user.hash` and `gen_ai.agent.id`, When
> the compaction sweep runs, Then the `parent`, `participant` and `actor`
> tuples exist in OpenFGA with tenant-prefixed ids, a second sweep writes
> nothing new (idempotent), and tuple writes are chunked ≤100.

> **RFC0047.11 — erasure removes tuples after rows.** Given a conversation
> erased through the compaction rewrite, When the pass completes, Then no
> tuple for that object remains and the object is absent from every
> principal's `ListObjects`; And the tuple deletion is ordered after the
> Parquet rewrite (asserted on the sweep's audit events).

> **RFC0047.12 — model tests gate CI.** Given `deploy/openfga/model.fga` and
> `store.fga.yaml`, When CI runs `fga model validate` and `fga model test`,
> Then a model that breaks a documented assertion (e.g. an actor reading a
> conversation it did not act in) fails the job.

## 6. Testing strategy

Unit tests: principal mapping (token/OIDC/agent claim), the planner's
predicate composition (two-step branch, IN construction with prefix
stripping, self fast path OR, contextual tuples, masking projection, bound →
error) against a mocked resolver. Integration (`ourios-server` `it/`,
testcontainers `openfga/openfga`, CI-only like the Dex job): RFC0047.1–.9
through the served binary with the in-tree model; RFC0047.10/.11 through the
compaction sweep against the same container. `.fga.yaml` (RFC0047.12): one
`check`/`list_objects` block per §5 relationship claim, run in a `semconv`-
style CI job with the pinned CLI. Property test: for random tuple sets the
planner's returned row set equals the naive "rows whose conversation ∈
`ListObjects`" oracle (RFC 0024 style).

## 7. Open questions

- [ ] **Principal mapping for agents** — the `agent_claim` name/value
      convention; whether an agent's token may also carry a delegating
      user (`act` claim, RFC 8693) to mint `delegate` automatically.
- [ ] **Object id namespacing** — the `T/<id>` prefix vs a per-tenant
      store; one store is the reviewed default, prefixing is the cost.
- [ ] **`max_objects` default** (10 000) — spike 2 streamed 100k in 0.5 s,
      so the bound is about predicate size / plan cost, not OpenFGA.
- [ ] **Where the emitter runs** — compaction sweep only, or also the
      receiver flush cadence (§3.3 proposes both); the freshness bridges
      make the answer a tuning question.
- [ ] **OpenFGA MCP for design time** — community servers exist
      (`evansims/openfga-mcp`, read-only by default); adopt for authoring the
      `.fga.yaml` tests, never as a runtime dependency (assistant review 2 Q3).

## 8. References

- #688 — concept discussion; OpenFGA resolver spike (`scratch/openfga-spike.md`).
- `scratch/openfga-ai-review-2026-08-17.md` — two OpenFGA-assistant reviews:
  model shape, ListObjects cap/deadline, two-step pattern, contextual tuples,
  ≤100 tuples/Write, no wildcard delete, single store, agents/delegation, MCP
  tool authorization (`tool` / `can_call`), ReBAC vs RBAC/ABAC.
- `scratch/openfga-spike-2-listobjects.md` — agent-scale measurements: silent
  1000 cap on plain ListObjects; streamed 5000 in 50 ms, 100k in 0.5 s;
  Check ~1 ms.
- RFC 0046 — out-of-band tenancy (prerequisite); RFC 0026/0029 — the binding
  and resolver seam; RFC 0027/0032 — MCP tools and query schema; RFC 0022/
  0042 — promoted columns the predicates target; RFC 0009 — compaction sweep
  the emitter rides; RFC 0024 — the oracle-style property test.
- OpenFGA docs: Multi-Tenant SaaS, AI Agent Authorization, Agents as
  Principals, MCP Authorization, Task-Based Authorization, Relationship
  Queries (caveats), Contextual Tuples, Consistency; `openfga/agent-skills`
  (in-tree at `.claude/skills/openfga`).
- OpenTelemetry GenAI semantic conventions (content attributes flagged as
  sensitive; `gen_ai.conversation.id`); the OTel platform blueprint's
  multi-tenant "help wanted".
- `CLAUDE.md` §3.7 (multi-tenancy), §4 hazard #4 (small files — why tenants
  stay coarse).

## 9. Follow-ons (recorded, not built here)

Cluster/service ownership as graph objects (from the reviewed model) once a
scenario needs them; time-boxed grants (`temporal_grant` CEL condition on
`tool#caller` / `conversation#delegate`); an upstream write-up for the OTel
community once this is validated in a real deployment (#688 Q7).
