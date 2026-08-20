---
rfc: 0047
title: ReBAC resolver (OpenFGA) and graph-fed visibility inside a tenant
status: green
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-17
supersedes: —
superseded-by: —
---

# RFC 0047 — ReBAC resolver and graph-fed visibility

> **Status: `green` (2026-08-18).** All twelve §5 criteria pass:
> RFC0047.1–.3 (the layer-1 resolver), .4–.8 (the planner two-step,
> masking, bounded enumeration), .9 (the MCP tool gate), .10–.11 (the graph
> emitter and erasure) on the served binary against a real OpenFGA container
> (`openfga-resolver` CI job) with the in-tree model, and .12 gating CI. The
> RFC0047.5 request-carried contextual-tuple arm is **struck** — rejected
> by RFC 0048 §3.5 (the carrier is sealed);
> the erasure request channel is the §3.6 slice-4 decision. Prerequisite: RFC 0046 (out-of-band tenancy, `green`) — the tenant is an
> opaque, coarse, credential-selected object, which is exactly the object
> type this RFC binds the authorization graph to. Grounded in the #688
> OpenFGA spike (resolver seam holds, p50 1.4 ms), two OpenFGA-assistant
> reviews (§8) and the agent-scale `ListObjects` spike recorded in §10, whose
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
can_read_content, tenant)` first, then `Check(can_read_metadata, tenant)`
(tenant-wide with content masked), and only for scoped principals a
bounded, streamed `ListObjects(conversation)` — filtered and counted per
tenant — that becomes an `IN (…)` predicate over the promoted column,
failing closed past an explicit bound or an incomplete stream. Content and metadata are separate relations, mapped to
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
`agent:<sub>`. It then issues `ListObjects(principal, can_query, tenant)`
and `ListObjects(principal, can_write, tenant)` (bounded, streamed —
tenant sets are small by construction) and produces the same
`AuthBinding { name, tenant-set }` RFC 0026 enforcement consumes; a
principal with no queryable and no writable tenant is unbound (401-class).
`can_query` is the **binding** capability, deliberately distinct from the
read capabilities: it holds for tenant-wide content readers, tenant-wide
metadata readers, *and* scoped principals (`scoped_reader` — a participant,
actor or delegate on some conversation in the tenant, §3.2/§3.3), so a
participant like `user:bob` or an agent like `agent:bot` binds the tenant
and reaches the planner, where the two-step (§3.4) decides *which rows*.
Binding never grants reading: `can_read_content` / `can_read_metadata` /
`can_write` stay separate relations, and the model tests assert that scoped
and metadata-only principals hold `can_query` without tenant-wide content.
The binding is cached per credential for `session_ttl_secs`, **fail-closed**:
an OpenFGA error or timeout during resolution is a `503` on the query side
and `UNAVAILABLE`/`503` on ingest, never an open door, and never a stale
grant past the TTL. Revocation latency ≡ TTL; `higher_consistency` bypasses
OpenFGA's own cache after writes. Ingest authorization is exactly RFC 0046
§3.5: the out-of-band selector ∈ the resolved write set.

**Composition with the credential's own tenant list (slice 1 decision).**
The graph is authoritative for *what* a principal may touch, and a
credential's own list — a static token's `tenants`, an OIDC
`tenant_claim` — can only narrow it, never widen it: the binding's read
set is `credential ∩ ListObjects(can_query)`, the write set
`credential ∩ ListObjects(can_write)`. With `auth.openfga` configured
the OIDC `tenant_claim` becomes optional (the graph binds; a token
without one binds exactly the graph's sets), and a static token
declares `tenants: ["*"]` to defer to the graph entirely. Every OpenFGA
round-trip is bounded by `auth.openfga.request_timeout_secs` (default
5 s) so fail-closed has a deadline. A token whose group claim exceeds
the contextual-tuple cap fails resolution with a **named** warning log
and an unauthenticated (401-class) session — a credential
defect, not an upstream outage.

**Token claims as contextual tuples.** An OIDC token's group claim
(`auth.oidc.groups_claim`, e.g. Dex `groups`) is passed to the resolver's
`ListObjects` calls as contextual tuples `team:<group>#member@<principal>`
— OpenFGA's documented pattern for claims-carried membership — so team
membership needs no synchronisation pipeline; only the stable edges
(`team#owner/reader` on tenants) are stored tuples written by operators.
Contextual tuples cap at 100 per request (a token with more groups than
that fails resolution closed, named error), are never persisted, and their
effect lives exactly as long as the session — the same
`session_ttl_secs` bound as everything else, so a group revocation in the
IdP is honoured on the next resolution.

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
    define scoped_reader: [user, agent]            # data-derived binding (§3.3)
    define can_write: writer or owner
    define can_read_metadata: metadata_reader or reader or owner
    define can_read_content: reader or owner
    define can_query: can_read_metadata or scoped_reader   # session binding (§3.1)

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
| every distinct `gen_ai.conversation.id` in tenant T | `conversation:T/<id>#parent@tenant:T` |
| (`gen_ai.conversation.id`, `user.hash` or `enduser.pseudo.id`) | `conversation:T/<id>#participant@user:<hash>` **and** `tenant:T#scoped_reader@user:<hash>` |
| (`gen_ai.conversation.id`, `gen_ai.agent.id`) | `conversation:T/<id>#actor@agent:<id>` **and** `tenant:T#scoped_reader@agent:<id>` |

Object ids are **tenant-prefixed** (`conversation:T/<id>`) in every tuple
the emitter writes and every id the planner reads: OpenFGA object ids are
opaque strings in one store, so the same raw conversation id in two tenants
must be two objects; the `parent` tuple carries the tenant edge and the
planner strips the prefix when it builds predicates (§3.4). A pure naming
rule, held in **one** place (`TenantObjects` in the core `openfga` module)
that both the emitter and the planner call. *Slice-2 decision:* the tenant
segment is percent-encoded for `/` and `%` (`conversation:<enc(T)>/<id>`),
so a tenant containing `/` can never alias another tenant's conversation
(`a` + `b/c-1` vs `a/b` + `c-1`); the raw conversation id follows verbatim.
A tenant that cannot itself be an object id (`:`, `#`, whitespace, > 256
bytes) has no graph objects at all — every graph question about it fails
closed (`403 tenant_unaddressable`, naming the rule).

The **binding tuple** (`tenant:T#scoped_reader@<principal>`) rides along
with every conversation grant so the principal can bind the tenant at
session establishment (§3.1). It is idempotent like the rest, and **stale
is safe by construction**: a `scoped_reader` whose last conversation grant
was erased (§3.6) still binds the tenant, but layer 2 then enumerates
nothing and — absent the self fast path — the query returns no rows. The
sweep garbage-collects such tuples best-effort; correctness never depends
on it. Ownership tuples (`tenant#owner/writer/reader/metadata_reader`,
`team#member`, `tool#caller`, `conversation#delegate`) are administrative:
written by operators through OpenFGA's own API/CLI, never by Ourios — and
a `delegate` grant on `conversation:T/<id>` MUST be paired by the operator
with `tenant:T#scoped_reader@agent:<id>` for the same reason (documented
next to the model; the `.fga.yaml` fixture shows the pair).

**Slice-4 decisions (implemented).** The emitter lives in the ingester
(`graph_emitter`), fed from **both** hooks: the compaction sweep observes
every input row it decodes (once, before any drop) and the receiver's
`PublishCoordinator` derives tuples from every batch it publishes and sends
them off the flush path once the batch is durable. The conversation key is
the column bound in `visibility.objects` (`attr.` or `resource.` stripped);
the user keys are `user.hash` and `enduser.pseudo.id`, the agent key
`gen_ai.agent.id`; a value that cannot be an object id is skipped. It also
writes the per-tenant `tool:T/<name>#parent@tenant:T` objects (§3.5), so
operators grant `caller` only. Writes are `on_duplicate = ignore`
(OpenFGA ≥ 1.10, per the OpenFGA assistant), so a resend is a no-op and
no read-then-diff pass exists; the sweep sends what it derived after the
blocking pass, in ≤ 100-tuple batches, counted on
`ourios.graph.tuples{ourios.graph.tuple.operation}`. Data stored before the
graph was configured is fed the next time its partition is rewritten
(compaction) — a backfill sweep is a follow-on (§9).

**Freshness.** A conversation is invisible to fine-grained principals until
its tuples land (seconds after the next sweep; the emitter is also invoked
on the receiver's flush cadence for the tenants it flushed). Two bridges,
both in the planner: (a) the **self fast path** — a `user:` principal (and
only a `user:` principal; agents and service accounts never get it) always
gets `<self_principal_column> == <subject>` as an additional OR-predicate
without consulting the graph, where `<subject>` is the principal id with
its `user:` prefix stripped: `user:bob` matches rows whose promoted
`attr.user.hash` is `bob`. The path presumes the deployment's
`self_principal_column` carries the same identity the OIDC subject does
(the RFC0047.5 fixture emits `user.hash = <sub>`); a deployment whose
hashes are not subjects leaves `self_principal_column` unset and the fast
path is disabled — never a mismatched comparison; (b) **contextual tuples** — a request may carry
`{conversation:T/<id>#participant@<principal>}` for ids it just created,
passed on `Check`/`ListObjects` and never persisted. Tenant-wide readers
(the FinOps/operator case) never wait: they resolve at layer 1.

**Bridge (b) is rejected (RFC 0048 §3.5; slice 2 had deferred it).** A contextual
tuple carried *by the request* is asserted by the very principal it
grants: any scoped caller could name any conversation id and read it —
a self-granted escalation the graph never checked. Contextual tuples are
an application-trusted input (the group claim, minted by the IdP, is one);
a caller-supplied one is not. Until a trusted carrier exists (a signed
claim from the producer, or the emitter's flush-cadence hook closing the
gap), freshness bridges are the self fast path (a) — verified against the
stored `user.hash` — and the emitter cadence. The RFC0047.5 contextual arm
was deferred with this question in §7; RFC 0048 §3.5 closes it by
rejection — the client API's `ContextualTuples` newtype has the group
claim as its only constructor, so the bridge is unrepresentable.

### 3.4 Layer 2 — query rewrite at plan time

For a query over tenant T by principal P, the planner runs the **two-step**:

1. `Check(P, can_read_content, tenant:T)` (cached with the session, TTL as
   §3.1). **Allowed ⇒ the tenant partition predicate only** — no
   enumeration, no masking, no change to today's plan.
2. Otherwise `Check(P, can_read_metadata, tenant:T)`. **Allowed ⇒ the tenant
   partition predicate plus column masking** — the tenant-wide metadata
   reader (`user:fin`, RFC0047.8) sees every row with the content columns
   masked, never an empty result: the configured content columns
   (`auth.openfga.visibility.content_columns`, default the GenAI content
   attributes `gen_ai.input.messages`, `gen_ai.output.messages`,
   `gen_ai.system_instructions`, `gen_ai.tool.call.arguments`,
   `gen_ai.tool.call.result` and `body`) are projected as NULL, and a query
   that filters or aggregates on a masked column is rejected (`403`, named
   column) rather than answered from data the principal may not read.
3. Otherwise the principal is scoped (it bound the tenant through
   `scoped_reader`, §3.1): `StreamedListObjects(P, can_read_content,
   conversation)` — the streamed variant, never plain `ListObjects`: the
   spike showed the plain call **returns HTTP 200 with 1000 objects and no
   truncation marker** for a principal over 100 000 conversations (13 ms) —
   a planner that enumerated with it would emit a *wrong* predicate
   silently. The stream is **global to the principal** (OpenFGA enumerates
   objects, not objects-within-a-tenant), so the planner filters each
   streamed id by the `T/` prefix and **counts only tenant-T ids** toward
   `auth.openfga.visibility.max_objects` (default 10 000): another tenant's
   grants can cost stream time but can never exhaust T's bound. The stream
   MUST be consumed to completion (or until T's bound is hit) within
   `auth.openfga.visibility.list_timeout` (default `2s`, below the server's
   own deadline — see the config note); reaching the
   bound **fails the query closed** with a named error ("visibility set
   exceeds N objects in tenant T; ask for tenant-wide read"), and hitting
   the timeout before the stream ends fails closed too ("visibility
   enumeration incomplete") — a partial tenant set is never accepted as a
   predicate. The surviving ids (prefix stripped) become
   `attr.gen_ai.conversation.id IN (…)` — over the promoted column, so
   RFC 0022/0042 pruning still applies — OR'd with the §3.3 self fast path
   and any contextual-tuple ids. A principal with an empty set and no fast
   path gets an empty result, not an error.
4. Scoped **metadata-only** grants do not exist in the v1 model (every
   conversation-level relation — `participant`, `actor`, `delegate` — grants
   content on that conversation), so there is no fourth branch today. If a
   future model adds one, the branch is: enumerate `can_read_metadata`
   conversations exactly as in step 3 and apply step 2's masking to the
   result — recorded here so the shape is settled before it is needed.

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
      content_columns: [body, attr.gen_ai.input.messages, attr.gen_ai.output.messages]  # replaces the default set
      max_objects: 10000        # tenant-T ids only (§3.4 step 3)
      list_timeout_ms: 2000     # MUST stay below server_list_objects_deadline_ms (3000)
    server_list_objects_deadline_ms: 3000   # the server's OPENFGA_LIST_OBJECTS_DEADLINE
```

`list_timeout_ms` is deliberately **below** OpenFGA's own
`OPENFGA_LIST_OBJECTS_DEADLINE` (server default 3 s, which bounds the
streamed call too): the client-side timeout must be the one that fires, so
an incomplete enumeration is always detected here and failed closed, never
ended quietly by the server. Startup validation rejects a `list_timeout_ms`
that is not below the configured server deadline
(`auth.openfga.server_list_objects_deadline_ms`, default 3000).

Per-record `Check` calls in the scan path are **never** performed (the
architectural line from the first spike, confirmed by both reviews).

**Slice-2 decisions (implemented).** Durations are milliseconds
(`list_timeout_ms`, `server_list_objects_deadline_ms`) like every other
knob; `objects[].type` accepts only `conversation` in v1 (the one bindable
type) and columns must be `attr.`/`resource.` promoted names; the two
`Check`s cache with the session TTL, the enumeration never does; masking
renders `body` as `{"kind":"masked"}` and a masked attribute as `"value":
null` (the OTLP unset value) — a reader can tell withheld from absent;
template-level surfaces (`drift`, `list_templates`, `template_drift`) need
tenant-wide content read (`403 visibility_scoped`) because templates are
mined from bodies; the branch taken is recorded on
`ourios.query.visibility{ourios.query.visibility.branch}` and the request
span (the MCP tool spans carry the same field), so RFC0047.4's "no
enumeration" is a counter assertion; an explicit `content_columns` list
replaces the default set and may not be empty (masking is never silently
disabled). The self
fast path is `user:` principals only, and principal ids are validated as
object ids (a `sub` with `:`/`#`/whitespace is a 401-class credential
defect, not a 503).

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

**Slice-3 decisions (implemented).** The gate runs after the RFC 0026
tenant binding and the §3.4 two-step: a principal on the *tenant-wide*
branch may call every tool without a round-trip (that is what the model's
`can_call: caller or can_read_content from parent` says, and it does not
depend on an operator having written the `tool#parent` tuple); every other
graph-bound principal needs an explicit `caller` grant, checked per call
with the session's contextual group tuples and never cached — a revoked
grant is honoured on the next call. The denial names the tool
(<code>permission denied: tool `template_drift` is not callable by this
principal in tenant `acme`</code>). Template-level tools additionally require
tenant-wide content read (§3.4 slice-2 rule), so a `caller` grant on
`list_templates` / `template_drift` for a scoped principal passes the gate
but not the content rule. The `tool:T/<name>#parent@tenant:T` tuples are
written by the emitter for every tenant it sweeps (slice 4), so operators
grant `caller` only.

### 3.6 Erasure (Q9)

Conversation-scoped erasure (RFC 0045 §9 / #688 Q9) rides the compaction
rewrite that removes the rows: the same pass reads the object's tuples
(`Read` by object) and deletes them in ≤100 chunks — no wildcard delete
exists — so a deleted conversation is unreachable *and* unlisted. Tuple
deletion follows the Parquet rewrite, never precedes it (a dangling tuple is
harmless; a dangling row is a leak).

**Slice-4 decisions (implemented).** The RFC left the *request* channel
open; the choice is a durable **marker object in the store** —
`erasure/tenant_id=<enc>/conversation=<enc>` (the partition
percent-encoding; body `{"phase":"rows"}`) — because the object store is
the source of truth (`CLAUDE.md` §3.6), it needs no new network surface
or credential, and an operator can write it with the tooling they already
have (`ourios_ingester::compactor::request_erasure` in-process). The sweep's
blocking pass rewrites every hour partition of the tenant through the same
compaction rewrite with the conversation's rows dropped (a single-file
partition is rewritten too), advances the marker to `{"phase":"tuples"}`
once every partition rewrote cleanly, and only then — in the async phase,
after the blocking pass — reads the object's tuples and deletes them in
≤ 100-tuple batches (`on_missing = ignore`), writes the new
`conversation_erased` audit event (RFC 0005 §3.7 kind 9, carrying
`partitions_rewritten` / `rows_dropped` / `tuples_deleted`) after the
sweep's compaction events, and removes the marker. A sweep interrupted
between the phases retries only the tuple deletion; an unreachable graph
leaves the marker and retries next sweep. Without a bound conversation
object a marker is recorded as a sweep error, never silently dropped.
The tuple deletion loops `Read → delete` until a `Read` returns empty —
at most **8** delete rounds plus one confirming read: a paginated `Read`
is not a snapshot (OpenFGA assistant, 2026-08-18), so a tuple the
flush-cadence feed writes concurrently is swept up by the next round; an
object still non-empty after the rounds is `EraseIncomplete` — the marker
stays in the `tuples` phase and the next sweep retries. Erasure covers the
rows durable at the rewrite; rows ingested for the same conversation id
afterwards are new data (with their tuples) — consistent by construction —
and the flush-cadence emit is a fire-and-forget within the same process
that completes in milliseconds and is never retried, so a batch published
before a later sweep's rewrite cannot land tuples after that sweep's
erasure; a dangling tuple is in any case harmless. The same review confirmed the object-naming
scheme has no documented pitfall; the served-binary test writes tenants
containing `/` and `%` to a real v1.11.1 and asserts `Read` returns the
object ids byte-for-byte and the encoded prefix filters the stream. The
server-side deadline behaviour on `streamed-list-objects` remains
undocumented (a source-level question); the client-side `list_timeout_ms`
below the server deadline stays the fail-closed pattern.

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
- **One OpenFGA store per tenant** (to make `ListObjects` tenant-constrained
  natively instead of the §3.4 prefix filter). Rejected: the multi-tenant
  guidance is one store with a `tenant` object (shared teams, one model
  version, one migration), and per-store provisioning would have to follow
  every RFC 0046 tenant's lifecycle. The prefix filter + per-tenant counting
  is the cost of the single store, paid in the planner.

## 5. Acceptance criteria

Scenario ids `RFC0047.<n>`. Integration tests run against a real OpenFGA
container (testcontainers, like Dex for RFC 0029) with the in-tree model.

> **RFC0047.1 — resolver binding.** Given `auth.openfga` configured and
> tuples granting `user:alice` reader on `tenant:acme` and
> `service_account:collector` writer on `tenant:acme`, When alice's OIDC
> bearer establishes a session, Then her binding's read set is `{acme}` and
> write set empty; And When the collector's token establishes one, Then its
> write set is `{acme}`; And Given `user:bob` with only a `participant`
> tuple on `conversation:acme/c-1` plus its paired
> `tenant:acme#scoped_reader` binding tuple, and `user:fin` with only
> `metadata_reader` on `tenant:acme`, When each establishes a session, Then
> each binding's read set is `{acme}` (they reach the planner) while
> `Check(can_read_content, tenant:acme)` is false for both; And Given a
> principal with no tuples, Then the session is unbound (401-class) — never
> an empty-but-open binding.

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
> conversations `acme/c-1` and `acme/c-2` only (tuples, with the binding
> tuple), When bob queries `true` on tenant `acme`, Then exactly the rows of
> `c-1`/`c-2` return; And Given a further conversation `c-9` whose rows
> carry `attr.user.hash = bob` (the principal's subject, prefix stripped)
> but no tuple yet, Then those rows also return (self fast path); And
> *(struck — rejected by RFC 0048 §3.5; the carrier is sealed)* ~~Given a
> contextual tuple for `c-10` on the request, Then `c-10`'s rows return
> too~~; And Given bob is
> also participant on `globex/c-1` (another tenant), Then that id never
> appears in the `acme` predicate.

> **RFC0047.6 — agent as principal.** Given `agent:bot` actor on 500
> conversations and `agent:other` actor on 500 different ones, When bot
> queries, Then exactly its 500 conversations' rows return and none of
> other's; And Given bot is also `delegate` on alice's conversation `c-7`,
> Then `c-7` returns for bot; And When the delegation tuple is deleted,
> Then (after the TTL) `c-7` no longer returns.

> **RFC0047.7 — bounded enumeration fails closed, per tenant.** Given
> `visibility.max_objects: 100` and an agent actor on 150 conversations in
> `acme`, When it queries `acme`, Then the query is rejected with the named
> bound error, no partial result is returned, and the plain (capped)
> `ListObjects` is never used — the streamed call is what the OpenFGA log
> shows; And Given instead an agent actor on 50 conversations in `acme` and
> 150 in `globex`, When it queries `acme`, Then the query **succeeds** with
> exactly the 50 (only tenant-`acme` ids count toward the bound); And Given
> `visibility.list_timeout` shorter than the stream (a stalled fake), Then
> the query fails closed with the incomplete-enumeration error, never a
> partial predicate.

> **RFC0047.8 — metadata without content.** Given `user:fin` with
> `metadata_reader` on `tenant:acme` only (no conversation tuples), When
> fin queries `sum(attr.cost_usd) by attr.model`, Then it succeeds over
> **every** row of the tenant (the §3.4 step-2 branch — the tenant
> predicate with masking, no enumeration issued); When fin selects or
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
- [x] **Request-carried contextual tuples (§3.3 bridge b)** — deferred in
      slice 2 (a caller-asserted `participant` tuple is a self-grant);
      **rejected by RFC 0048 §3.5**, which also names the only trusted
      carriers. RFC 0048 further takes over the tenant id grammar (removing
      the §3.3 percent-encoding), the identity keys as configuration, the
      erasure front door and a backfill pass.
- [ ] **OpenFGA MCP for design time** — community servers exist
      (`evansims/openfga-mcp`, read-only by default); adopt for authoring the
      `.fga.yaml` tests, never as a runtime dependency (assistant review 2 Q3).

## 8. References

- #688 — concept discussion (Q1–Q10 strawman) and the OpenFGA resolver
  spike report (seam holds, p50 1.4 ms).
- Two OpenFGA-assistant reviews (2026-08-17, maintainer-run, working notes
  not in-tree; every load-bearing answer is restated where it applies):
  model shape, ListObjects cap/deadline, two-step pattern, contextual tuples,
  ≤100 tuples/Write, no wildcard delete, single store, agents/delegation, MCP
  tool authorization (`tool` / `can_call`), ReBAC vs RBAC/ABAC.
- §10 — the agent-scale `ListObjects` spike: silent 1000 cap on plain
  ListObjects; streamed 5000 in 50 ms, 100k in 0.5 s; Check ~1 ms.
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

A backfill sweep that feeds the graph from data stored before the graph was
configured (today only a rewrite re-derives); an operator-facing erasure
surface over the store marker (a CLI verb / MCP tool); cluster/service
ownership as graph objects (from the reviewed model) once a scenario needs
them; time-boxed grants (`temporal_grant` CEL condition on
`tool#caller` / `conversation#delegate`); an upstream write-up for the OTel
community once this is validated in a real deployment (#688 Q7).

## 10. Evidence — agent-scale `ListObjects` spike (2026-08-17)

Throwaway `openfga/openfga run` (memory backend, defaults
`OPENFGA_LIST_OBJECTS_MAX_RESULTS=1000`, 3 s deadline); model = the §3.2
DSL. Seed: `tenant:acme`, 100 000 `conversation:acme/c-i` with
`parent tenant:acme`, agents `bot50` / `bot500` / `bot5000` as `actor` on
that many conversations, `user:alice` tenant reader, `user:bob` participant
of one conversation. Seeding via `/write` in 100-tuple chunks: 74.8 s
(~1 300 tuples/s single-threaded).

| Call | Result | Latency |
|---|---|---|
| Check(alice, `can_read_content`, tenant:acme) — the two-step gate | allowed | p50 1.1 ms / p95 1.4 ms |
| Check(agent, `can_read_content`, tenant:acme) | false | p50 0.8 ms |
| Check(agent, `can_read_content`, conversation:acme/c-3) | allowed | p50 2.1 ms |
| Check(collector, `can_write`, tenant:acme) — ingest binding | allowed | p50 1.0 ms |
| ListObjects(bot50, conversation) | 50 | 2.3 ms |
| ListObjects(bot500, conversation) | 500 | 2.5 ms |
| ListObjects(bot5000, conversation) | **1000 (silently capped, HTTP 200)** | 3.4 ms |
| ListObjects(bob, conversation) | 1 | 1.6 ms |
| ListObjects(alice tenant-wide, conversation) over 100k | **1000 (silently capped, HTTP 200)** | 13 ms |
| StreamedListObjects(bot5000) | 5000 | 0.05 s wall |
| StreamedListObjects(alice tenant-wide) | 100 000 | 0.53 s wall |

What this decided:

1. **The cap is silent** — 200 with 1000 objects and no truncation marker.
   A planner that enumerated for a tenant-wide reader would build a *wrong*
   `IN()` (1 % of the tenant) with no error, so the two-step in §3.4 is a
   correctness requirement, not an optimisation.
2. **Scoped sets are cheap** (2–3 ms for 50–500) but a principal with more
   than 1000 conversations still hits the cap → the scoped path uses
   `StreamedListObjects`, filters to the tenant, and fails closed at
   `max_objects` (§3.4 step 3) rather than emitting a giant `IN()`.
3. Session-time checks are ~1 ms → TTL-cached fail-closed resolution is free
   at the RFC 0029 seam (as the first spike found).
4. Write throughput at 100 tuples/Write is adequate for an emitter riding
   the compaction pass; a burst of new conversations lags seconds, which
   contextual tuples and the §3.3 self fast path cover.

The harness was a throwaway script; nothing from it lands in-tree.
