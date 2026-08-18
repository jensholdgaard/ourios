---
rfc: 0048
title: Graph operational surfaces — tenant id grammar, identity keys, erasure and backfill
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-18
supersedes: —
superseded-by: —
---

# RFC 0048 — Graph operational surfaces

> **Status: `specified` (2026-08-18).** §5 criteria written and testable.
> Prerequisites: RFC 0046 (`green`), RFC 0047 (`green`). This RFC closes
> the operational gaps the RFC 0047 implementation (#705–#710) had to fill
> on its own — each of them a decision that belongs in a spec, with a
> criterion, rather than in a slice's commit message.

## 1. Summary

RFC 0047 specified the authorization *model* well and left four
operational surfaces unspecified; the implementation chose for each and
recorded the choice as a "slice decision". This RFC promotes those choices
into contract — or replaces them where the implementation's choice was a
workaround. Four changes: (1) a **tenant id grammar** shared by every
boundary (selector, token config, OIDC claim, graph object ids) that makes
the graph's percent-encoding unnecessary; (2) the graph's **identity keys**
(user, agent) become configuration next to the conversation column, with
today's semconv keys as defaults; (3) an **operator-facing erasure
surface** over the RFC 0047 store marker (a CLI verb to request and to
list, plus a completion signal), and the marker made the *only* channel;
(4) a **backfill** pass that feeds the graph from data stored before the
graph was configured. It also formally **rejects** the request-carried
contextual-tuple bridge RFC 0047 §3.3(b) deferred, and pins the
`list_timeout_ms` / server-deadline coupling as observable at startup.

## 2. Motivation

The RFC 0047 slices surfaced five things a reviewer of the spec could have
asked and did not (the retrospective is in the #710 discussion):

- **Two grammars for one identifier.** RFC 0046 made the tenant an opaque
  string (visible text, ≤ 256 bytes, no control characters); RFC 0047 then
  put it inside OpenFGA object ids, where `:`, `#` and whitespace are
  illegal and `/` is the conversation separator. The implementation
  reconciled them with percent-encoding of the tenant segment
  (`conversation:<enc(T)>/<id>`) plus a per-tenant "cannot be a graph
  object → fail closed" branch. Encoding is a smell that says the grammar
  should have been constrained upstream: a tenant that cannot name a graph
  object is not a tenant this system can authorize.
- **Asymmetric configuration.** The conversation column is explicit
  (`visibility.objects[].column`, "nothing is inferred") while the user and
  agent keys (`user.hash` / `enduser.pseudo.id`, `gen_ai.agent.id`) are
  constants in the emitter. A deployment whose producers carry identity
  under `enduser.id` or a bespoke key cannot use the graph.
- **No operator surface for erasure.** RFC 0047 §3.6 says how an erasure
  *runs* but not how it is *requested*; the implementation invented a
  durable marker object (`erasure/tenant_id=<enc>/conversation=<enc>`)
  reachable only from inside the process (`request_erasure`). An operator
  today writes an object into the bucket by hand. That is a workable
  primitive and the wrong front door.
- **No backfill.** The emitter feeds the graph from the flush cadence and
  from every row compaction rewrites; data stored before the graph was
  configured is fed only if something rewrites its partition. A deployment
  that turns the graph on over existing history has scoped principals who
  see nothing until an unrelated compaction happens by.
- **A bridge that was a hole.** RFC 0047 §3.3(b) let *the request* carry
  `conversation:T/<id>#participant@<principal>` as a contextual tuple — a
  self-grant. Slice 2 deferred it; this RFC rejects it and names the only
  trusted carriers.

None of these change the model or the two-step; they are the surfaces
around them.

## 3. Proposed design

### 3.1 Tenant id grammar (amends RFC 0046 §3.1)

A tenant id is **1–128 bytes of ASCII graphic characters (`0x21`–`0x7E`,
i.e. printable ASCII excluding space) with `:`, `#` and `/` further
excluded** — Rust's `char::is_ascii_graphic` minus three characters. Every
boundary applies the same rule, once, at
extraction: the OTLP selector (`X-Ourios-Tenant` / metadata, RFC 0046),
the querier header and MCP `tenant` argument, `auth.tokens[].tenants`,
the OIDC `tenant_claim` values, and the OpenFGA `tenant:<T>` object. A
value outside the grammar is `400` / `INVALID_ARGUMENT` at the request
boundaries and a startup error in configuration.

Consequences, in order:

- The RFC 0047 percent-encoding of the tenant segment goes away:
  `conversation:<T>/<id>` and `tool:<T>/<name>` with `T` verbatim; the
  `/` separator is unambiguous because `T` cannot contain it, and the raw
  conversation id follows (it may contain `/`). `TenantObjects` keeps the
  one-place naming rule and drops `encode_tenant_segment`; the
  `InvalidTenant` branch becomes unreachable from the request boundaries
  and stays only as the library's own guard.
- RFC 0046's "non-ASCII reachable over HTTP but not gRPC" caveat
  disappears — the grammar is ASCII everywhere.
- The 256-byte selector bound tightens to 128: OpenFGA caps the **full
  object string** (`conversation:<T>/<id>`) at 256 — the proto constraint
  is `^[^\s]{2,256}$` on the whole `type:id` string (openfga/openfga
  discussion #302; the docs AI confirms the cap includes the type prefix
  and the colon) — so a 128-byte tenant leaves 256 − 13 (`conversation:`)
  − 128 − 1 (`/`) = **114 bytes** for the conversation id. Whether the
  cap counts bytes or characters is not documented; the grammar here is
  ASCII-only, so the two coincide and the question cannot bite. Pre-production, this is a `!` change with no
  dual-read (`feedback: break persisted layouts pre-production`); the
  percent-encoding of the *storage path* (`data/tenant_id=<enc>`, RFC 0005
  §3.4) is untouched — it is a path rule, not a grammar.
- **Conversation ids** keep the *object-id* grammar, not the tenant
  grammar: 1 byte or more of ASCII graphic characters excluding `:` and
  `#` (so `/` is allowed — a raw `gen_ai.conversation.id` may contain it),
  and the full `conversation:<T>/<id>` string ≤ 256 bytes. One function
  (`TenantObjects::conversation_fits`) is the rule; the emitter skips a row
  whose id does not fit, the planner never sees such an id (the graph
  cannot hold it), the erasure path erases its rows and deletes zero
  tuples, and the CLI (§3.3) applies the *tenant* grammar to `--tenant`
  and this object-id grammar to `--conversation`.

### 3.2 Identity keys as configuration (amends RFC 0047 §3.3)

```yaml
auth:
  openfga:
    visibility:
      objects:
        - type: conversation
          column: attr.gen_ai.conversation.id
      identities:                       # RFC 0048 — who is in the conversation
        user_columns: [attr.user.hash, attr.enduser.pseudo.id]   # default
        agent_columns: [attr.gen_ai.agent.id]                    # default
      self_principal_column: attr.user.hash
```

`identities.user_columns` / `agent_columns` name the promoted columns
(`attr.` / `resource.`) whose values become `user:<v>` and `agent:<v>`
principals in the emitter's tuples (`participant` + binding, `actor` +
binding, exactly as RFC 0047 §3.3). Defaults are today's constants — the
OpenTelemetry semantic-convention keys — so a deployment that says nothing
gets the same graph. Every listed column must be a promoted column
(startup error otherwise, the RFC 0047 §3.4 rule); a value that cannot be
an object id is skipped as today. `self_principal_column` must be one of
`user_columns` (the fast path compares the subject to a column that also
mints `participant`, or it compares nothing).

### 3.3 Erasure surface (amends RFC 0047 §3.6)

The RFC 0047 store marker stays the **durable primitive and the only
channel** — the compactor acts on markers and nothing else — and gains an
operator front door:

```text
ourios-server graph erase   --tenant acme --conversation c-7      # writes the marker
ourios-server graph erasures [--tenant acme]                       # lists pending markers + phase
```

Both are `ourios-server` subcommands (clap, RFC 0004 style) that resolve
the same `storage` config as the daemon, so they work against local and
S3 stores alike, and both refuse a tenant or conversation id outside the
§3.1 grammar. `erase` is idempotent (create-if-absent, RFC 0047 §3.6). No
HTTP or MCP surface: an erasure is an operator action against the store
of record, not a tenant-facing request; an admin API is a later RFC if a
scenario needs one. Completion is observable three ways, all existing: the
marker disappears (`graph erasures`), the `conversation_erased` audit
event lands (RFC 0005 §3.7 kind 9), and
`ourios.graph.tuples{ourios.graph.tuple.operation="delete"}` counts. The
compactor's sweep additionally logs one structured
event per completed erasure naming tenant, conversation, rows dropped and
tuples deleted.

### 3.4 Backfill (amends RFC 0047 §3.3)

```text
ourios-server graph backfill --tenant acme [--from 2026-08-01T00:00:00Z]  # one-off, resumable
```

Reads every data partition of the tenant — `--from` (RFC 3339, UTC)
selects partitions whose **hour start ≥ `from`**, a half-open
`[from, ∞)` on the partition key, so a whole hour is either in or out —
offers every row to the emitter, and writes the derived tuples in ≤ 100-
tuple idempotent batches — the same code path as the sweep's observer,
driven over all partitions instead of the ones being rewritten. It never
rewrites Parquet. Resumable by construction (every write is idempotent);
progress is one structured event per partition and `ourios.graph.tuples`.
Runs as a subcommand, not a daemon mode, so it cannot be left on by
accident.

**Backfill and erasure exclude each other.** Idempotent writes alone do
not make backfill safe beside an erasure: a partition read before the
erasure and written after it would recreate the erased conversation's
tuples. So the two hold each other off through the store: backfill first
checks for pending erasure markers and refuses to start when any exists
for the tenant ("erasures pending for `acme`; run again after the next
sweep") — leaving **no** lock behind — then creates (create-if-absent) a
**lock marker** `backfill/tenant_id=<enc>` (the RFC 0005 §3.4 path
encoding, like the erasure marker) and re-checks the erasure markers once
more under the lock, removing the lock and refusing if one appeared in
between; the sweep's erasure pass skips a tenant whose backfill lock exists (recorded
in the sweep report, retried next sweep); backfill removes its lock on
completion, and `graph backfill --unlock --tenant T` clears a lock a
crashed run left behind (the operator's call, logged). Both markers are
listed by `graph erasures`.

### 3.5 Contextual tuples — the trusted carrier (amends RFC 0047 §3.3)

The request-carried bridge is **rejected**. Contextual tuples reach the
graph from exactly one carrier in v1, trusted by construction: the OIDC
group claim (`team:<group>#member@<principal>`, minted by the identity
provider — RFC 0047 §3.1). The client API enforces it: `check` and
`streamed_list_objects` take a `ContextualTuples` newtype whose only
constructor is the group-claim path (`OpenFgaResolver::group_tuples`,
which validates every group as an object id and applies the 100 cap) —
no caller can hand the client an arbitrary tuple. Freshness for a
conversation whose tuples have not landed is the self fast path (data-
verified) and the flush-cadence emit (seconds). RFC 0047 §3.3(b) and the
corresponding RFC0047.5 arm are struck; RFC 0047 §7's open item closes.

### 3.6 Deadline coupling made visible (amends RFC 0047 §3.4)

`list_timeout_ms` must stay below OpenFGA's `OPENFGA_LIST_OBJECTS_DEADLINE`.
The server cannot observe that setting; it can make its own assumption
loud. At startup, when `auth.openfga` is configured, the server logs one
structured event (`ourios.server.graph.list_deadline`) carrying the client
`list_timeout_ms` and the declared `server_list_objects_deadline_ms`, and
the RFC 0047 startup rejection stays. Operators who change the server's
deadline have one line to grep for.

## 4. Alternatives considered

- **Keep percent-encoding, leave tenants opaque.** Works (proven against
  v1.11.1, #710), but every future object type pays the encoding and every
  reader of a tuple sees `%2F`. A grammar is one rule; an encoding is a
  rule plus a decoder in every consumer. Rejected.
- **Constrain tenants only where the graph is configured.** Two grammars
  again, chosen at runtime — exactly the confusion this RFC removes.
- **An admin HTTP endpoint for erasure.** A new authenticated surface with
  its own authorization question ("who may erase?") for one operator verb;
  the marker + CLI keeps erasure an act against the store of record. If a
  self-service tenant erasure is ever needed, that is a scenario for its
  own RFC.
- **Backfill as a compactor mode.** A long-running flag that must be
  turned off is the wrong shape for a one-off; a subcommand is.
- **Signed request-carried contextual tuples.** Would restore bridge (b)
  safely, but needs a producer-side signer and a key distribution story
  for a bridge whose window the flush-cadence emit already covers.
  Rejected for v1; noted in §9.

## 5. Acceptance criteria

Scenario ids `RFC0048.<n>`.

> **RFC0048.1 — one tenant grammar, every boundary.** Given tenant ids
> `acme`, `a/b`, `a:b`, `a b`, `a#b`, a 129-byte id and a non-ASCII id, When
> each is presented as the OTLP selector (HTTP and gRPC), the querier
> header, the MCP `tenant` argument, an `auth.tokens[].tenants` entry and an
> OIDC `tenant_claim` value, Then `acme` is accepted everywhere and every
> other value is rejected everywhere with the same named reason (`400` /
> `INVALID_ARGUMENT` at request boundaries, a startup error in config, an
> unverifiable token for the claim).

> **RFC0048.2 — no encoding, one byte budget.** Given the grammar, When
> the emitter and the planner name a conversation, Then the object is
> `conversation:<T>/<id>` with `T` verbatim (asserted against a real
> OpenFGA: write, `Read` byte-for-byte, streamed prefix filter), and
> `TenantObjects` has no encoding step; And Given a 128-byte tenant, Then a
> 114-byte conversation id fits and a 115-byte one is skipped by the
> emitter (never sent), and an id containing `/` fits.

> **RFC0048.3 — identity keys are configuration.** Given
> `identities.user_columns: [attr.enduser.id]` and
> `identities.agent_columns: [attr.bot.name]`, When rows carrying those
> attributes are swept, Then the graph holds `participant`/`actor` tuples
> for those values and none for `user.hash` / `gen_ai.agent.id`; And Given
> no `identities` block, Then today's defaults apply unchanged
> (RFC0047.10 still passes); And Given a non-promoted column or a
> `self_principal_column` outside `user_columns`, Then startup fails
> naming the key.

> **RFC0048.4 — erasure has a front door.** Given the daemon's storage
> config, When `ourios-server graph erase --tenant acme --conversation
> c-7` runs, Then the marker exists (`graph erasures` lists it in the
> `rows` phase), a second `erase` is a no-op, and the next sweep completes
> RFC0047.11 unchanged; When the sweep completes, Then `graph erasures`
> lists nothing and one structured completion event names tenant,
> conversation, rows dropped and tuples deleted; And Given an id outside
> the grammar, Then the verb refuses it before touching the store.

> **RFC0048.5 — backfill feeds history.** Given a tenant with N sealed
> partitions written before `auth.openfga` was configured and a scoped
> principal who is a participant in them, When the principal queries, Then
> it sees no rows; When `graph backfill --tenant T` runs, Then the graph
> holds the RFC0047.10 tuples for every partition (writes ≤ 100 per batch),
> the principal sees exactly its rows, a second run writes nothing new,
> and no Parquet file was rewritten; And Given `--from` at an hour
> boundary, Then partitions whose hour starts at or after it are fed and
> earlier ones are not.

> **RFC0048.6 — the request bridge is gone.** Given a scoped principal and
> a request that attempts to carry a contextual `participant` tuple for a
> conversation it holds no grant on, When it queries, Then no such tuple is
> sent to the graph (asserted on the fake's request log) and the rows do
> not return; And Given the client API, Then `check` /
> `streamed_list_objects` accept only the `ContextualTuples` newtype and
> its sole constructor is the validated group-claim path (a compile-time
> property, exercised by the resolver tests); And RFC 0047 §3.3(b) and the
> RFC0047.5 arm read as struck.

> **RFC0048.7 — the deadline assumption is loud.** Given `auth.openfga`
> configured, When the server starts, Then one
> `ourios.server.graph.list_deadline` event carries `list_timeout_ms` and
> `server_list_objects_deadline_ms`, and (RFC 0047) a `list_timeout_ms` not
> below the deadline is still a startup error.

> **RFC0048.8 — backfill and erasure exclude each other.** Given a pending
> erasure marker for tenant `T`, When `graph backfill --tenant T` runs,
> Then it refuses before reading any partition, naming the pending
> erasure, and leaves **no** backfill lock behind (`graph erasures` lists
> only the erasure); And Given a backfill lock for `T`, When a sweep runs with an
> erasure marker for `T`, Then the erasure is skipped and reported (not
> advanced), and after the lock is removed the next sweep completes it;
> And Given a backfill that finished, Then its lock is gone and
> `graph erasures` lists neither.

## 6. Testing strategy

Unit: the grammar as one function with a table test (each boundary calls
it — the test asserts every boundary routes through it, RFC0048.1); config
validation for `identities` (RFC0048.3); `TenantObjects` without encoding
(RFC0048.2). Integration (`ourios-server` `it/`, the RFC 0047 container
harness): RFC0048.2 and RFC0048.5 against a real OpenFGA; RFC0048.4 and
RFC0048.8 by spawning the subcommands against a temp store and asserting
the markers, the listing, the refusal and the sweep's completion/skip
events; RFC0048.6 on the fake (request log) plus the newtype's
constructor visibility; RFC0048.7 on the served binary's stdout/log. The RFC 0047
container tests keep passing unchanged except for the encoding assertions,
which flip to the verbatim form.

## 7. Open questions

- [ ] **Streamed deadline truncation is silent-until-proven-otherwise** —
      the docs confirm `OPENFGA_LIST_OBJECTS_DEADLINE` applies to the
      streamed endpoint (and `…_MAX_RESULTS` does not — the reason §3.4 of
      RFC 0047 chose it), but say nothing about *how* deadline expiry ends
      the stream: an NDJSON error frame (which the client already turns
      into a fail-closed refusal) or a clean EOF (which the client reads
      as complete — a truncated scoped set would then present as the full
      one; visibility only narrows, never widens, but the partiality is
      silent). The §3.6 implementation must pin this empirically: run a
      real server with a deliberately tiny deadline and assert what
      arrives on the wire, then either rely on the error frame or add a
      completion check. (Docs-AI consult, 2026-08-19.)

- [ ] **Grammar strictness** — 128 bytes and ASCII graphic minus three
      characters is deliberately tight; is there a known deployment that
      needs `.`-free or longer ids? (Defaults to tight; loosening later is
      additive, tightening later is not.)
- [ ] **`graph erasures` output shape** — table for humans, `--json` for
      tooling; both, or JSON only?
- [ ] **Backfill and the receiver's flush cadence** — the flush emit and
      backfill only ever *add* the same idempotent tuples, so no exclusion
      is needed there (unlike erasure, §3.4); confirm nothing else writes
      the graph concurrently before closing.

## 8. References

- RFC 0046 §3.1 (selector normalisation), RFC 0047 §3.1/§3.3/§3.4/§3.6 and
  the slice-1..4 decision paragraphs; #705–#710 (the implementation and
  the review threads that surfaced these); the OpenFGA assistant answers of
  2026-08-18 (`Read` is not a snapshot; no server-side ListObjects
  scoping; object-id limits).
- RFC 0004 (CLI shape), RFC 0005 §3.4 (storage path encoding, unchanged),
  RFC 0005 §3.7 kind 9 (`conversation_erased`).
- `CLAUDE.md` §3.6 (object storage is the source of truth — why the erasure
  primitive stays a store marker), §3.7 (multi-tenancy).

## 9. Follow-ons (recorded, not built here)

Signed request-carried contextual tuples (a producer-side signer would
restore RFC 0047 §3.3(b) safely); a tenant-facing self-service erasure API
if a scenario needs one; a cross-process erasure fence should a second
writer to the same store ever exist.
