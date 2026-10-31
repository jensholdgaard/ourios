---
rfc: 0062
title: Attribute paths into map values
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-10-31
supersedes: —
superseded-by: —
---

# RFC 0062 — Attribute paths into map values

> **Status: `drafted`.** Tracks #1134. This RFC **amends RFC 0002 §6.1**
> (`accepted`), which reads `attr.<key>` as the attribute key "taken
> literally including dots", and **RFC 0022 §3.3** (`accepted`), whose
> JSON arm compares one top-level key. RFC 0042 §3.4's integer JSON arm
> is defined "as RFC 0022 §3.3" and follows it. The change alters result
> sets, a major version under RFC 0002 §6.6 (§3.4, §7). RFC 0002 and
> RFC 0022 get a dated pointer here once this RFC is accepted. It builds on the top-level comparison of
> #1137, which restores what RFC 0022 §3.3 already states and needs no
> RFC.

## 1. Summary

`attr.a.b == "x"` and `attr.a.b != "x"` also look inside map values.
When the record has a top-level attribute named `a.b`, that attribute
is the answer, as today. When it has none, a dotted key is read as a
path: `a.b` is the value under key `b` in the map held by attribute `a`.
`resource.<key>` follows the same rule. Nothing is written differently
and no column changes; the change is in how the JSON arm of an `==` /
`!=` comparison finds its value.

## 2. Motivation

OTLP lets an attribute hold a map of any depth (OTel common spec,
*AnyValue*), and producers use it: a logging bridge that records a
structured argument, or a Collector `transform` that nests attributes,
writes `user = {id: "u7", tier: "gold"}` instead of `user.id` and
`user.tier`. Today `attr.user.id == "u7"` finds nothing in such a
record, and the only way to reach the value is to know it is nested and
search the raw JSON. RFC 0002 §6.1 made the literal reading the rule
because OTel names are dotted namespaces (`http.response.status_code`
is one key, not a path), and that still has to win. But a query that
names a path the record only holds as a map should not silently return
nothing.

The OTel semantic conventions tell convention authors to "assume that
backends do not index individual properties of complex attributes"
(*How to write conventions*, *Defining attributes*). That is a
statement about indexing cost, not a requirement to ignore nested
values, and this RFC does not index them: the lookup runs on the stored
JSON, and the measurements in §3.6 show what it costs.

## 3. Proposed design

### 3.1 The resolution rule

For an attribute list `L` and a key `p`, `resolve(L, p)` is:

1. If `L` has a pair whose key is exactly `p`, the value of the first
   such pair, whatever its type. The search stops here, even when that
   value is empty or not a string.
2. Otherwise, for each `.` in `p`, from the last to the first (the
   longest leading key first): split `p` into `head` and `tail` there.
   If the first pair named `head` holds a map, and `resolve(map, tail)`
   is defined, that is the answer.
3. Otherwise `p` resolves to nothing, which is a missing field.

Examples, for `attr.a.b.c`:

| Attributes | Resolves to |
|---|---|
| `a.b.c = "x"`, `a = {b: {c: "y"}}` | `"x"` (rule 1) |
| `a.b = {c: "x"}`, `a = {b.c: "y"}` | `"x"` (`a.b` is the longer head) |
| `a = {b.c: "y"}` | `"y"` |
| `a = {b: {c: "x"}}` | `"x"` |
| `a = "s"` | nothing (a string is not a map) |
| `a = [{b: {c: "x"}}]` | nothing (arrays are not traversed) |

The first-occurrence choice in rules 1 and 2 matches the promoted
column's projection (`promoted::project_string_value`) for records that
break OTLP's unique-key rule. Arrays have no index syntax in the DSL,
so a path never enters one.

`attr["a.b.c"]` resolves the same way as `attr.a.b.c`. The DSL's
bracket form exists for keys a bare identifier cannot spell (RFC 0002
§6.1), and the IR does not record which form was used (§7 asks whether
it should).

### 3.2 Where the rule applies

- **`==` / `!=` on `attr.<key>`, `resource.<key>` and `service`**,
  through the JSON arm (RFC 0022 §3.3). `service` is
  `resource.service.name` and follows the same rule. The missing-field
  rule is unchanged: `!=` matches only when the key resolves to a value
  of the literal's type that differs. The integer JSON arm of an `i64`
  promoted key (RFC 0042 §3.4) uses the same rule.
- **Promoted keys.** The promoted column still projects the literal
  top-level key only (RFC 0022 §3.1); nothing on the write side
  changes. The two-arm form stays exact under rule 1: a non-`NULL` cell
  means the literal key is present, so it is the answer; a `NULL` cell
  falls through to the JSON arm, which applies the full rule.
- **Not covered:** ordering and regex compile against the promoted
  column alone (RFC 0022 §3.3), so a value only reachable by a path
  never matches them, the same documented non-match as for
  pre-amendment files. Grouping and aggregation (`by attr.k`,
  `sum(attr.k)`) read promoted columns only, and string functions on
  attribute fields stay rejected. Each of these would need a write-side
  projection of nested values, which is §4's first alternative.

### 3.3 Compilation

The JSON arm keeps its two steps:

1. **Prefilter.** A `LIKE` on the stored pair text, as today, but one
   pattern per key the pair can be stored under: `p` itself and every
   tail after one of its dots (`a.b.c`, `b.c`, `c`), joined with `OR`.
   For a key without dots this is the single pattern used today.
2. **Confirm.** The internal `ourios_attr_cmp` UDF applies §3.1 to the
   candidate rows. It reads the stored text without decoding it: a row
   with no map value cannot hold a path, so the top-level shortcut from
   #1137 answers it; other rows are scanned, descending only into the
   maps on the path.

A row the scan cannot read fails the query, as in #1137. The UDF keeps
no stack, so value depth is not limited by the lookup.

### 3.4 Amendments

- **RFC 0002 §6.1, *Paths*.** "`attr.<key>` (`attr.http.status_code` →
  attribute `"http.status_code"`)" becomes: the attribute named
  `<key>`; when there is none, `<key>` is resolved as a path into map
  values by RFC 0062 §3.1. The same for `resource.<key>`.
- **RFC 0022 §3.3.** The JSON arm `J` compares the value `resolve`
  (RFC 0062 §3.1) yields, instead of the top-level key's value.
- **RFC 0002 §6.6.** This alters result sets, so it is a major grammar
  version. It only adds rows: a record holding the literal key answers
  exactly as before, and only a record without it can newly match. §7
  asks how to handle the deprecation window that section requires.

### 3.5 Invariants and hazards

- **H6 (DSL vs SQL), `ourios-querier/src/plan/`:** the DSL surface
  changes meaning as §3.4 records; no DataFusion name or behaviour
  reaches the user. The query cookbook and the RFC 0032 query-schema
  resource text describing `attr.<key>` are updated with it.
- **§3.5 Parquet schema, §3.3 / H7 reconstruction, H1–H5:** untouched.
  Nothing is written differently.
- **§3.7 tenancy:** the lookup runs inside the tenant-scoped scan; no
  new path reads data.

### 3.6 Cost

Measured on the `attr_predicate` criterion bench (200k rows, seven
attributes each, the user id stored as a map in half the rows and as a
literal `user.id` key in the other half; median of 30 samples). The
experiment compared the #1137 comparison before its final optimisation
with a path-resolving version of it:

| Query | top-level only | with paths |
|---|---:|---:|
| unpromoted `http.route == "/cart"` | 37.6 ms | 40.7 ms (+8%) |
| unpromoted, no match | 24.2 ms | 27.5 ms (+14%) |
| unpromoted `user.id == "u7"` | 22.3 ms | 26.9 ms (+20%) |
| unpromoted `user.id != "u7"` | 261 ms (99,900 rows) | 557 ms (199,800 rows) |
| promoted keys | 19.4 ms | +0.5–0.8% |

The extra cost comes from the extra `LIKE` patterns and from the rows
that now match. #1137's final version moved the top-level baseline to
within 5% of `main` for `==` and 40% for `!=`. RFC0062.6 re-measures
this RFC against that baseline.

## 4. Alternatives considered

**Project nested paths into promoted columns at write time.** An
operator would promote `user.id` and the writer would fill the column
from either shape. Ordering, regex and grouping would then work on
paths, with pruning. It changes what the writer projects (RFC 0022
§3.1, a §3.5 schema-contract change) and has to choose a winner at
write time; it can follow this RFC, whose rule it would reuse.

**Keep literal-only and document it.** Cheapest, and what RFC 0002
§6.1 says today. It leaves map-shaped records unreachable by name,
which the maintainer chose not to accept (#1134).

**Try every split, not just the longest head first.** `a.b.c` has
2ⁿ⁻¹ readings; trying them all only changes the answer when more than
one resolves, which the longest-head order already makes deterministic.

**A separate path syntax** (`attr.user/id`, or chained brackets
`attr["user"]["id"]` as OTTL writes it). Unambiguous, but every user
has to know which shape a producer chose before writing the query,
which is the problem this RFC removes. Chained brackets remain
available as an additive form later.

## 5. Acceptance criteria

> **Scenario RFC0062.1 — the literal key wins**
> - **Given** records with `user.id = "u9"` and `user = {id: "u7"}`,
>   written once with `user.id` promoted and once without
> - **When** `attr.user.id == "u7"` and `attr.user.id != "u7"` run
> - **Then** neither `==` query matches the record and both `!=`
>   queries do

> **Scenario RFC0062.2 — a dotted key resolves into a map**
> - **Given** records `user = {id: "u7"}`, `user = {id: "u8"}`,
>   `a = {b: {c: "x"}}`, `a.b = {c: "long"}` with `a = {b.c: "short"}`
> - **When** `attr.user.id == "u7"`, `attr.user.id != "u7"`,
>   `attr.a.b.c == "x"` and `attr.a.b.c == "long"` run, promoted and not
> - **Then** each matches exactly one record, and `attr.a.b.c ==
>   "short"` matches none

> **Scenario RFC0062.3 — what a path does not enter**
> - **Given** records `user = "u7"`, `user = ["u7"]`,
>   `user = [{id: "u7"}]` and a record without `user`
> - **When** `attr.user.id == "u7"` and `attr.user.id != "u8"` run
> - **Then** neither matches any of them

> **Scenario RFC0062.4 — resource attributes and `service`**
> - **Given** a record whose resource holds `service = {name: "api"}`
>   and no `service.name`
> - **When** `resource.service.name == "api"` and `service == "api"` run
> - **Then** both match it

> **Scenario RFC0062.5 — the UDF agrees with a reference resolver**
> - **Given** attribute lists from the RFC 0024 adversarial generator
> - **When** `ourios_attr_cmp` is evaluated for every key and every
>   dotted join of nested keys in the list, against a resolver that
>   decodes the list and applies §3.1 directly
> - **Then** both give the same `==` and `!=` answers

> **Scenario RFC0062.6 — the cost stays in bounds**
> - **Given** the `attr_predicate` bench, committed with this RFC's
>   implementation
> - **When** it runs against the pre-RFC baseline
> - **Then** promoted-key cases stay within 3%, and unpromoted `==`
>   cases within 20%, and the results are recorded in the implementing
>   PR

## 6. Testing strategy

- RFC0062.1–.4: integration tests in
  `ourios-querier/tests/it/rfc0022_attr_columns.rs`, each run with and
  without promotion, through the existing table-driven `assert_counts`
  helper.
- RFC0062.5: a proptest beside the UDF in `plan/attr_value.rs`,
  extending #1137's scan-against-decode property.
- RFC0062.6: the `attr_predicate` criterion bench in `ourios-bench`,
  run locally first, then on the ci-runner.
- The existing tests that pin top-level-only behaviour for keys
  without dots (`a_nested_pair_is_not_a_top_level_attribute`, the
  #1137 query tests) stay unchanged: a key without dots has no path.

## 7. Open questions

- [ ] RFC 0002 §6.6 asks for a deprecation window and a documented
  migration for a result-set change. Since this change only adds rows,
  the suggestion is to record it as grammar v2.0 with a `feat(querier)!`
  commit and a cookbook note, and no window. Maintainer to decide.
- [ ] Should `attr["a.b"]` stay literal-only, as the Collector's stanza
  field syntax reads brackets, so a user can opt out of path
  resolution? That needs the IR to record the form. The suggestion is
  no: rule 1 already makes the literal key win.

## 8. References

- OTel specification, *Common*: [AnyValue](https://opentelemetry.io/docs/specs/otel/common/#anyvalue)
  ("Arbitrary deep nesting of values for arrays and maps is allowed"),
  [Attribute Collections](https://opentelemetry.io/docs/specs/otel/common/#attribute-collections)
- OTel semantic conventions: [Naming](https://opentelemetry.io/docs/specs/semconv/general/naming/)
  (dot-delimited namespaces),
  [Defining attributes](https://opentelemetry.io/docs/specs/semconv/how-to-write-conventions/#defining-attributes)
  (complex attributes and backend indexing)
- RFC 0002 §6.1, §6.6 (amended here); RFC 0022 §3.1, §3.3 (amended
  here); RFC 0042 §3.4 (integer JSON arm, same rule); RFC 0024
  (adversarial generator); RFC 0032 (query-schema resource)
- Issue #1134, PR #1137, issue #1136
