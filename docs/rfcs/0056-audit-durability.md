---
rfc: 0056
title: Audit-sink durability on permanent write failure
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-09-15
supersedes: —
superseded-by: —
---

# RFC 0056 — Audit-sink durability on permanent write failure

> **Status note.** `drafted`. Split out of RFC 0053's status note so an
> amendment to an *accepted* RFC is not hidden. Source wording: #802 @
> `30a21f80`, status note + the three-way `write_owned` result in §3.2.

## 1. Summary

RFC 0005 §7 says the writer guarantees no audit event is lost across
crashes. The current audit sink reports a *permanent* write failure as
success and `write_ordered` then publishes the dependent records.

This RFC amends that clause: a permanent audit-write failure is a
third outcome. Dependent records for that tenant stay unpublished and
requeued. The tenant becomes server-terminal until repaired.

## 2. Motivation

A record in Parquet without the template event that describes it
breaks `CLAUDE.md` §3.1 (no silent template merges / audit
durability). The 0053 draft changed that behaviour in a status note.
An accepted RFC needs its own amendment and criterion.

## 3. Proposed design

- `write_owned` returns a three-way result: durable / transient /
  permanent.
- `write_ordered` refuses the dependent record publish for that tenant
  on permanent.
- That tenant is marked terminal; other tenants continue.
- Derive failure on the audit side takes the same path.
- Authorization-denial events (`IngestDenied`, RFC 0026) are emitted
  before any frame exists; replay cannot regenerate them. That gap
  stays RFC 0026's, named here so 0056 does not pretend to cover it.

## 4. Alternatives considered

- Keep the silent drop. Rejected against RFC 0005 §7 and §3.1.
- Fold into RFC 0053. Rejected: wrong document.

## 5. Acceptance criteria

> **RFC0056.1**
> - **Given** a permanent audit-write failure for one tenant
> - **When** `write_ordered` returns
> - **Then** that tenant's records are unpublished and requeued, the
>   tenant is terminal, other tenants publish, and the RFC 0005 §7
>   clause is the amended one

## 6. Testing strategy

One fault-injected permanent audit failure beside a healthy second
tenant. Implementing PR.

## 7. Open questions

- Operator repair path for a terminal tenant (restart vs explicit
  clear).
- RFC 0026 `IngestDenied` durability, owned by 0026.

## 8. References

- RFC 0005 §7, RFC 0026, RFC 0053 draft status note on #802
