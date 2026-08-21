---
rfc: 0049
title: Agent delegation — refusing silent impersonation via the RFC 8693 `act` claim
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-08-21
supersedes: —
superseded-by: —
---

# RFC 0049 — Agent delegation and the `act` claim

> **Status: `specified` (2026-08-21).** §5 criteria written and testable.
> Prerequisites: RFC 0026 (OIDC, `accepted`), RFC 0047 + RFC 0048 (the
> graph, both `accepted`). This RFC closes a gap left by those RFCs:
> Ourios
> derives its principal from `sub` alone, so a standards-compliant
> delegation token authenticates as the **subject**, discarding the
> actor — impersonation, which RFC 8693 exists to distinguish from
> delegation.

## 1. Summary

[RFC 8693][rfc8693] defines the `act` (actor) claim: a token whose
subject is one party and whose *acting* party is another. Ourios's OIDC
verifier never inspects it (`crates/ourios-core/src/auth/oidc.rs`: the
principal is `sub`, and `agent:` versus `user:` is decided solely by the
configured `agent_claim`). A deployment that turns on token exchange
therefore gets **silent impersonation**: an agent presenting a valid
delegation token authenticates as the human, inherits every one of that
human's grants, and no signal anywhere records that an agent was
involved. Nothing is malformed and nothing is misread — the actor is
simply dropped.

Three changes, in the order they matter:

1. **A token carrying `act` is refused by default** (`401`, named
   reason), because a principal Ourios cannot represent must never be
   silently downgraded to one it can.
2. **An opt-in `actor` mode** maps the principal to the *current* actor —
   never the subject — so a delegation token grants exactly what the
   agent itself holds, and the delegating subject travels as attribution
   only.
3. **`act` never writes to the graph and never becomes a contextual
   tuple.** The group claim stays the single contextual carrier
   (RFC 0048 §3.5). A blanket `act` → `delegate` expansion is rejected
   here on the record, so nobody reaches for it later.

## 2. Motivation

### 2.1 What the specification actually says

RFC 8693 separates two things that look alike:

> With impersonation, "A is given all the rights that B has within some
> defined rights context and is indistinguishable from B in that
> context." With delegation, "principal A still has its own identity
> separate from B, and it is explicitly understood that while B may have
> delegated some of its rights to A, any actions taken are being taken by
> A representing B."

Discarding `act` collapses the second into the first. The RFC also
bounds what a consumer may believe: a chain of delegation nests `act`
claims, but "the consumer of a token MUST only consider the token's
top-level claims and the party identified as the current actor by the
`act` claim. Prior actors identified by any nested `act` claims are
informational only." And on the risk: "Any time one principal is
delegated the rights of another principal, the potential for abuse is a
concern."

### 2.2 Why this is not hypothetical

Ourios already treats agents as first-class principals (RFC 0047 §3.1)
and ships an MCP surface (RFC 0027) whose callers are agents by
construction. Token exchange is how an agent platform obtains a
credential for a user's session; the moment a deployment's IdP issues
one, our resolver reads `sub` and hands the agent the human's visibility
— the exact outcome layer 2 exists to prevent. OWASP's LLM06 (Excessive
Agency) names the mitigation in the same terms: "Track user
authorization and security scope to ensure actions taken on behalf of a
user are executed on downstream systems in the context of that specific
user, and with the minimum privileges necessary."

### 2.3 What the ecosystem recommends instead

OpenFGA's agent guidance is the shape this project already follows —
"'on behalf of' is not the same as 'as.' A well-modeled agent has its
own identity, inherits only the permissions it actually needs, and can
be revoked independently of the user it serves" — and its current
recommendation for *actual* delegation is task-based authorization:
agents start with zero permissions and receive narrowly scoped,
optionally session-bounded grants, with a contextual tuple binding the
calling agent to the task. That is a successor to this RFC (§3.5), not
its content.

## 3. Proposed design

### 3.1 `act` is a refusal by default

The OIDC verifier inspects the validated claim set for a top-level
`act` member. When present and the deployment has not opted in (§3.2),
verification fails with each surface's existing unauthenticated path —
`401` on the querier, `/mcp` and **OTLP/HTTP** ingest, a trailers-only
`UNAUTHENTICATED` (grpc-status 16) on **OTLP/gRPC** — carrying the stable
kind `delegation_unsupported` and `error.type = "unauthenticated"` on
`ourios.auth.resolutions`. The message names the claim and the config
key that enables handling, so an operator meets a diagnosis rather than
a mystery.

Refusal — not "ignore and continue as the subject" — is the point: the
token asserts something about authority that this server does not
implement, and RFC 0047's posture is that an unanswerable authorization
question fails closed.

### 3.2 `actor` mode: the principal is the actor, never the subject

```yaml
auth:
  oidc:
    delegation: reject        # default; `actor` opts in
```

`delegation` changes **nothing** for a token without an `act` claim: in
either mode such a token keeps today's mapping — the principal is the
top-level `sub`, typed by `agent_claim` — so enabling the mode cannot
alter ordinary authentication. The rules below apply only when `act` is
present.

Under `delegation: actor` a token carrying `act` authenticates as the
**current actor**:

- the principal id is the actor's `sub` (the `act` object's `sub`
  member); a missing or non-string `sub` inside `act` is a refusal;
- the principal *type* applies the existing rule to the `act` object —
  `agent:` when the configured `agent_claim` appears inside it, else
  `user:`;
- **nested `act` claims are ignored** for authorization (RFC 8693's
  MUST); only the outermost actor decides the principal;
- the token's own `sub` (the delegating party) grants **nothing**. It is
  not consulted, not intersected, not unioned. An actor with no grants
  is refused or scoped exactly as that actor would be without the token.

The guarantee is precise, and narrower than "a delegation token can only
reduce access": the **subject's grants are never inherited**. What the
bearer sees is exactly what the actor sees authenticating alone — which
may be less than the subject's access, and may be *more* where the actor
holds grants the subject does not. The property that matters is that a
delegation token is never a way to acquire someone else's visibility;
it is only ever a way to authenticate as the actor, with attribution
attached.

### 3.3 Attribution, not authority

An accepted delegation records the delegating subject on the request
span and the auth audit event — attribution, so an operator can answer
"who was this agent acting for?" — and nowhere else. It is never a
principal, never part of a predicate, never a graph object. The
attribute name is minted through `semconv/registry/` with the OTel
naming rules applied (the project's standing rule: query the OTel MCP
and check for a semconv collision before adding any name), so the exact
key is settled at implementation, not asserted here.

### 3.4 `act` never reaches the graph

No tuple — persisted or contextual — is ever derived from the `act`
claim. (The OIDC *group* claim remains the system's one claim-derived
tuple and is unchanged: request-scoped `team:<group>#member` through the
sealed carrier.) RFC 0048 §3.5 sealed the contextual carrier so that
its only constructor is the validated group-claim path; this RFC adds
nothing to it. Concretely, the rejected design is `act` ⇒
`conversation:T/<id>#delegate@agent:A` for every conversation the
subject participates in: `act` carries no resource scope, so expanding
it into a per-conversation grant over the subject's whole footprint is
impersonation reached by a longer route — precisely what §2.1
distinguishes, and what LLM06's minimum-privilege guidance forbids.

### 3.5 Where real delegation goes (not built here)

If a deployment needs an agent to read a user's conversations, the
answer stays an **explicit, revocable grant**: today the `delegate`
relation an operator or user writes on a specific conversation
(RFC 0047 §3.2); tomorrow, if the scenario justifies it, OpenFGA's
task-based pattern — a `task` (and optionally `session`) object, grants
scoped to it, and a contextual tuple binding the calling agent to the
task so a task cannot be replayed by a different agent. That is a
separate RFC with its own producer story; naming it here is what stops
`act` → `delegate` being rediscovered as a shortcut.

### 3.6 One identity per agent (deployment requirement)

OWASP's Non-Human Identities Top 10 lists NHI9 *NHI Reuse* — "sharing
identities across multiple services or agents, complicating
attribution" — and NHI5 *Overprivileged NHI*. Ourios cannot detect
sharing: a fleet behind one `sub` is one principal, so per-agent
revocation and attribution both quietly disappear, and every grant is
held by all of them. This RFC therefore requires the rule be written
into the authentication guide beside `agent_claim` when it is
implemented: **one subject per agent identity**. It is a deployment
contract the server cannot enforce.

## 4. Alternatives considered

- **Keep today's behaviour (ignore `act`).** This *is* the vulnerability:
  a valid delegation token becomes full impersonation of the subject with
  no record of the actor. Rejected.
- **Accept, warn, and continue as the subject.** A warning nobody reads
  does not change who the query ran as. Rejected.
- **Mint `delegate` tuples from `act`.** §3.4. Rejected on the record.
- **Treat `act` as a contextual `delegate` (request-scoped, not
  persisted).** Better than persisting, still wrong: the grant is
  unscoped, so it is impersonation for the life of the request.
  Rejected.
- **Build task-based authorization now.** The right destination (§3.5),
  but it needs a task/session model, a producer contract for creating
  tasks, and a consent story. Deferred; this RFC makes the unsafe
  interim behaviour impossible rather than shipping the full feature.

## 5. Acceptance criteria

Scenario ids `RFC0049.<n>`. Eight criteria (RFC0049.1–.8).

> **RFC0049.1 — a delegation token is refused by default.** Given
> `auth.oidc` configured without `delegation`, When a token carrying a
> top-level `act` claim is presented to the OTLP receiver (HTTP and
> gRPC), the querier and `/mcp`, Then each refuses with the surface's
> unauthenticated status (`401` / `UNAUTHENTICATED`), the stable kind
> `delegation_unsupported`, a message naming the `act` claim and the
> `auth.oidc.delegation` key, and one `ourios.auth.resolutions`
> increment with `error.type = "unauthenticated"`; And no data is read,
> written or acknowledged.

> **RFC0049.2 — the actor is the principal, and the subject grants
> nothing.** Given `delegation: actor`, a subject `user:alice` holding
> tenant-wide `reader` and an actor `agent:bot` holding nothing in the
> tenant, When a token with `sub = alice` and `act.sub = bot` queries,
> Then the principal is `agent:bot`, the visibility branch is the one
> `agent:bot` would get alone (scoped or refused — never alice's
> tenant-wide read), and alice's grants are not consulted.

> **RFC0049.3 — only the outermost actor counts.** Given
> `delegation: actor` and a token whose `act` nests a further `act`
> (`alice` → `bot` → `inner`), When it is presented, Then the principal
> is the outermost actor (`bot`) and the nested actor is ignored for
> authorization — including when the *nested* `act` is itself malformed,
> which never affects the outcome; And Given a top-level `act` that is
> not an object (`null`, a string, a number, an array), or an object
> whose `sub` is absent, not a string, empty, or not a valid object id,
> Then the token is refused with the same unauthenticated path as
> RFC0049.1 — a malformed delegation is never downgraded to the
> subject.

> **RFC0049.8 — a token without `act` is untouched.** Given
> `delegation: actor` and a token carrying no `act` claim, When it
> authenticates on any surface, Then the principal is the top-level
> `sub` typed by `agent_claim`, exactly as with `delegation: reject` and
> exactly as before this RFC — enabling the mode changes nothing for
> ordinary tokens.

> **RFC0049.4 — the actor's principal type follows the same rule.** Given
> `delegation: actor` and `agent_claim: ourios_principal_type=agent`,
> When the `act` object carries that claim/value, Then the principal is
> `agent:<act.sub>`; And when it does not, Then it is `user:<act.sub>`.

> **RFC0049.5 — attribution without authority.** Given an accepted
> delegation, Then the delegating subject appears on the request span and
> the auth audit event under the registry-minted attribute, And it never
> appears as a principal, in a visibility predicate, or as a graph
> object (asserted on the fake's request log, as RFC0048.6 does).

> **RFC0049.6 — `act` never reaches the graph.** Given any delegation
> token in either mode, Then no `Write` is issued and no contextual tuple
> other than the group-claim tuples is sent (the sealed
> `ContextualTuples` carrier gains no new constructor).

> **RFC0049.7 — the knob is validated.** Given
> `auth.oidc.delegation: <anything else>`, Then startup fails naming the
> key and the accepted values.

## 6. Testing strategy

Unit: the verifier's claim handling as a table (absent `act`, top-level
`act` in each mode, nested `act`, malformed `act`, the `agent_claim`
inside `act`) next to the existing OIDC tests; config validation for the
knob. Integration (`ourios-server` `it/`): RFC0049.1 across the three
surfaces on the served binary with the existing issuer fixture;
RFC0049.2 and .4 against the fake graph, asserting the branch and the
request log; RFC0049.5 on the span exporter and the audit sink. No
OpenFGA container test is required — the authorization *model* does not
change, which is itself the point of §3.4.

## 7. Open questions

- [ ] **`may_act`.** RFC 8693's companion claim states that a party is
      authorized to *become* an actor. It is an authorization-server
      concern; is there any value in Ourios reading it (for instance to
      refuse an `act` the issuer never sanctioned), or is that
      double-checking the IdP's own job?
- [ ] **Per-tenant delegation mode.** `delegation` is deployment-wide as
      specified. A deployment that trusts token exchange for one tenant
      and not another would need it per credential or per tenant — no
      scenario asks for it yet.
- [ ] **Task-based authorization (§3.5).** Its own RFC when a scenario
      arrives: what creates a task, who consents, how expiry is
      expressed (OpenFGA conditions vs a TTL on the object).

## 8. References

- [RFC 8693 — OAuth 2.0 Token Exchange][rfc8693]: `act` (§4.1), `may_act`
  (§4.4), delegation versus impersonation (§1.1), security
  considerations (§5).
- OWASP Top 10 for LLM Applications 2025, **LLM06 Excessive Agency** —
  minimum privileges, acting in the context of the specific user,
  human-in-the-loop for high-impact actions.
- OWASP **Non-Human Identities Top 10 (2025)** — NHI5 Overprivileged
  NHI, NHI9 NHI Reuse.
- OpenFGA modelling guides: *AI agent authorization*, *Modeling agents as
  principals*, *Task-Based Authorization* (the §3.5 successor pattern).
- RFC 0047 §3.1 (principal mapping), §3.2 (`delegate`), RFC 0048 §3.5
  (the sealed contextual-tuple carrier).

[rfc8693]: https://www.rfc-editor.org/rfc/rfc8693.html
