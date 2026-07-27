---
rfc: 0041
title: Dashboard datasource plugins — Ourios as a Grafana / Perses source
status: specified
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-25
supersedes: —
superseded-by: —
---

# RFC 0041 — Dashboard datasource plugins

> **Status: `specified` (2026-07-27).** The decision this RFC asked for has
> been made (maintainer, same date): **build it now, Perses first, three
> plugins, in the dedicated
> [`ourios-perses-plugin`](https://github.com/jensholdgaard/ourios-perses-plugin)
> repository** — with the FinOps dashboard as the capstone artifact and the
> Grafana datasource an ungated later follow-up. What changed since
> `drafted`: RFC 0042 landed typed numeric promotion and verified live spend
> aggregation (RFC0042.9), so a dashboard now charts *money* — the plugin
> became the demo artifact for the agent-FinOps direction rather than a
> generic API client. §5/§6 are now written; §7 records the resolutions.
>
> *(Original `drafted` framing, 2026-07-25: §5/§6 were deliberately empty
> because the open question was not* how *but* whether and where*; both
> hosts were spiked to a rendered dashboard first, so §3.1's figures are
> measured, not guessed. The §3.4 severity finding shipped separately as
> RFC0002.21.)*

## 1. Summary

Ourios answers queries over HTTP (`POST /v1/query`, RFC 0016) in a logs DSL
(RFC 0002) that was designed with dashboard authors as its primary audience.
Neither Grafana nor Perses can consume it today: each needs a datasource
plugin. This RFC proposes writing one — and asks which host, or whether the
work belongs in this cycle at all.

Working spikes exist for **both** Grafana and Perses. Each renders real
ingested logs in a real dashboard against the live querier. The Ourios-side
work (query shape, field mapping, time-range injection) is identical across
them and took ~20 minutes to port; effectively all the cost is in each host's
plugin system. Measured effort **at log parity**: 1–2 days for Grafana, 3–5
for Perses — and Grafana's figure already includes time series, which Perses
would need a further plugin for (§3.1).

## 2. Motivation

**The query surface is stable and nothing consumes it but us.** RFC 0002
(DSL) and RFC 0016 (query endpoint) are both `green`. The only clients today
are `curl`, the MCP surface (RFC 0027), and the bench harness. An operator who
wants a wallboard has no path that does not involve writing one themselves.

**The DSL was built for this and the debt is already paid.** RFC 0002 §3.6
names "Perses dashboard authors (declarative YAML/CRDs)" as the **primary**
audience, and §4 P7 makes YAML-embeddability a first-class requirement, tested
by RFC0002.10 (a property test asserting every well-formed query is a
single-line scalar surviving a YAML round-trip). That constraint shaped the
grammar. It buys nothing until a dashboard tool can actually issue the query.

**The stated blocker has cleared.** `docs/roadmap.md` §5 defers the Perses
plugin with the rationale that "a Perses plugin queries a query interface that
doesn't exist yet", gated on RFC 0031 close-out. RFC 0031 is `accepted`
(2026-07-22) and the query API is `green`. The roadmap line now reads: "Its
stated prerequisite (RFC 0031 close-out) is now met."

**Counter-motivation, stated plainly.** This is a client, not the engine.
Nothing in `CLAUDE.md` §2's pillars moves. The MCP surface already gives an
*agent* the same access a dashboard would give a human, and the
agent-observability direction is arguably the more differentiated one. A
reader should be able to conclude "not now" from this RFC as easily as "yes" —
§7 puts that question first.

## 3. Proposed design

### 3.1 What the spikes established (measured, not estimated)

Both spikes ran against the live dogfood querier with real ingested telemetry
and were driven to a rendered dashboard via headless Chromium.

| | Grafana | Perses |
|---|---|---|
| Renders Ourios log lines | ✅ | ✅ |
| Time series from `count by bucket(w)` | ✅ (same plugin) | not built |
| Plugins for logs | 1 datasource | 2 (`Datasource` + `LogQuery`) |
| Plugins for logs **and** time series | still 1 | 3 (adds `TimeSeriesQuery`) |
| Backend language | **none** (data proxy) | none (frontend + CUE) |
| Schema language | none | **CUE, mandatory** |
| Config validated | at render | **at write time** |
| Measured effort (log parity) | **1–2 days** | **3–5 days** |

Findings that shape any real implementation:

- **Grafana needs one plugin for every frame type; Perses needs one per
  query kind.** A single Grafana datasource returns logs, time series and
  tables — the response shape picks the frame. Perses splits `LogQuery` from
  `TimeSeriesQuery`, so the same coverage is a third plugin. The spikes built
  logs on both and time series on Grafana only, which is why the effort figures
  below are not directly comparable at equal scope: Perses at *log parity* is
  3–5 days; adding its time-series plugin is more.
- **No backend component is required on either host.** Grafana's data proxy
  (`plugin.json` `routes`) and Perses's datasource proxy both forward
  server-side, injecting `x-ourios-tenant` from datasource config. This
  removes the assumed Go backend and is most of the cost saving.
- **`count by bucket(w)` already returns RFC 3339 bucket keys**, so the
  time-series path needs no server-side work. Verified rendering as a graph.
- **Grafana's ISO range format parses as-is.** `range(2026-07-25T08:00:00.000Z,
  …)` — milliseconds included — is accepted by the DSL. No translation layer.
- **Perses timestamps are seconds; Grafana's are milliseconds.** A silent
  1000× error if assumed rather than checked.
- **Perses's `LogQueryStats.bytesExamined`** maps directly onto Ourios's
  `stats.bytes_read`, so pillar #1's pruning win surfaces in the UI for free.
  Grafana has no equivalent slot.

### 3.2 The Ourios-side mapping (host-independent)

This is the portable half — identical in both spikes:

| Ourios (`POST /v1/query`) | Dashboard field |
|---|---|
| `records[].time_unix_nano` | timestamp (÷1e6 → ms for Grafana, ÷1e9 → s for Perses) |
| `records[].body.line` | log line |
| `records[].severity_text`, else OTLP `severity_number` band | level |
| `attributes[]` + `resource_attributes[]`, `AnyValue`-unwrapped | labels |
| `aggregate[]` with RFC 3339 keys | time series |
| `aggregate[]` with other keys | table |
| `stats.bytes_read` | query stats (Perses only) |

The dashboard time range becomes a `range(...)` stage. Two properties a plugin
author needs and should not have to infer:

- **The window is half-open.** RFC 0002 §6.2 fixes `range(from, to)` as
  `from <= effective < to`, matching RFC 0010's `[from, to)`. Both Grafana's
  and Perses's pickers hand over an inclusive-looking `to`, so a row exactly on
  the upper bound is *excluded* — worth stating, because the alternative is
  each plugin quietly guessing and drifting apart.
- **A range the user wrote by hand wins.** The injected stage is skipped
  entirely when the query already contains a `range(...)`; silently overriding
  it would make the editor lie about what ran.

### 3.3 Where the plugin lives

A plugin is TypeScript; this is a Rust workspace. `CLAUDE.md` §7 pins the
layout and makes a new component an architectural commitment. The plugin
should therefore live in **its own repository**, not in `crates/`. That keeps
this repo's toolchain single-language and lets the plugin version against its
host rather than against Ourios releases.

### 3.4 The severity gap — RESOLVED, shipped separately

This began as the one finding here that touched Ourios rather than a plugin,
and it has since been decided and merged on its own: **RFC0002.21** (RFC 0002
§6.1 amendment, PR #641). It is recorded here because the spike is what
surfaced it, and because it is the clearest example of the kind of defect only
a dashboard client exposes.

Confirmed live during the spike: a natural first query — `severity >= trace` —
returned **zero rows** against real agent telemetry, because Claude Code's
GenAI events carry `severity_number: 0`, below `trace`, so every row group
pruned. Through a dashboard that looks like a broken datasource.

Storing the `0` was never in question — it is what the source sent, and
RFC 0018's rule governs: the backend is a *faithful witness, not a
corrector*. What was wrong was the **comparison**. The OTel Logs SDK drops a record on
`minimum_severity` only when its SeverityNumber "is specified (i.e. not `0`)";
unspecified records "bypass minimum severity filtering". Ourios did the
inverse. The data model sanctions the special case explicitly: "Special
handling MAY be given to `SeverityNumber=0` when it is used to represent an
unspecified severity."

Shipped semantics: a floor (`>=` / `>`) above `0` admits unspecified rows; a
ceiling (`<` / `<=`) excludes them, so a predicate and its negation still
partition; an explicit `0` threshold keeps ordinary numeric meaning, so
`severity > 0` still means "has a specified severity". The rule is compiled
into the predicate rather than applied after the scan, because it is a
**pruning-correctness** matter and not only a UX one — a post-filter would have
left the old min/max pruning in place and silently skipped whole files of
unspecified rows.

**Nothing here blocks or depends on the plugin decision**, and the fix stands
whether or not this RFC is ever implemented — which is why it shipped first.

## 4. Alternatives considered

**Grafana first, Perses later (or never).** Cheapest path to the most users,
and the spike proves 1–2 days. Grafana is also where the comparative work
already points (RFC 0031 benchmarks against Grafana Loki, so reviewers of that
work already have Grafana running). Against: Perses is the roadmap item and the
DSL's stated primary audience, so shipping Grafana first is a deliberate
reordering of a documented plan.

**Perses first.** Matches the roadmap and RFC 0002 §3.6's primary audience,
validates dashboards at write time, and surfaces pruning stats natively. But
it is 2–3× the effort, and `percli`'s scaffolding is currently **broken for
query plugins**: it cannot generate a `LogQuery` at all, omits `#kind`/
`#selector` from the datasource schema, and pins a CUE module
(`perses/perses/cue`) that does not define `#datasourceSelector`, while the
shipped plugins use a different one (`perses/shared/cue`). Each of those is a
silent failure a newcomer loses hours to. The spike documents the fixes.

**Both.** The Ourios-side mapping ported in ~20 minutes, so the marginal cost
of the second is mostly its host's plugin system, not re-derivation. Still two
artefacts to version, sign, and maintain against two moving APIs.

**Grafana's Infinity datasource (no plugin at all).** Configure the existing
generic JSON/HTTP datasource against `/v1/query`. Zero code, works today.
Against: every panel hand-maps fields, there is no query editor, no schema
awareness, and nothing to publish — it is a workaround an operator can already
discover, not a project deliverable. Worth documenting in the guide either way.

**A Loki-compatible query API.** If Ourios spoke LogQL over Loki's HTTP API,
Grafana support would be free via the built-in datasource, and Perses's Loki
plugin would work too. This is the only option that gets both hosts for one
piece of work. Against: it is an enormous surface to imitate faithfully, it
would make Loki's semantics a compatibility constraint on the DSL forever, and
`CLAUDE.md` §1 says we are "not a Loki/Mimir/ClickHouse clone". RFC 0031 uses
Loki strictly as a benchmark target and never proposed API compatibility.
Rejected, but recorded because it is the obvious "why not just…" question.

**Do nothing.** The MCP surface (RFC 0027) already lets an agent query Ourios,
and `docs/guides/agent-telemetry.md` documents that loop. If the
agent-observability direction is the differentiated one, a human wallboard may
simply not be the constraint worth spending on this cycle. This is a live
option, not a strawman — see §7.

## 5. Acceptance criteria

Written at the `specified` flip (2026-07-27), the §7 host question resolved:
**Perses first** — three plugins in the dedicated
[`ourios-perses-plugin`](https://github.com/jensholdgaard/ourios-perses-plugin)
repository — with the Grafana datasource an explicitly cheap follow-up this
RFC does not gate on. Criteria RFC0041.1–.5 are satisfied by tests in the
plugin repository's CI (run against the released `ourios-server` container
image, the collector-interop pattern inverted); RFC0041.6 by an artifact in
this repository. The RFC ladder here tracks their aggregate state.

- **RFC0041.1 (datasource).** Given a Perses instance with the
  `OuriosDatasource` plugin configured against a running `ourios-server`
  container, when the datasource health/connection path runs, then it
  succeeds with and without an RFC 0026 bearer token per the server's mode,
  and a wrong-tenant token is surfaced as the API's 403, not swallowed.
- **RFC0041.2 (log query).** Given ingested fixture records, when a Perses
  log panel runs an RFC 0002 DSL statement through `OuriosLogQuery`, then
  the rendered rows equal the RFC 0016 response (body, timestamp, severity,
  service mapped per §3.2), and a DSL error surfaces as the panel's error
  state carrying the API's message.
- **RFC0041.3 (time series).** Given fixture records spanning multiple
  bucket windows, when a time-series panel runs `count by bucket(w)` and
  `sum(attr.<f64 key>) by attr.<key>, bucket(w)` through
  `OuriosTimeSeriesQuery`, then the series match the API's aggregate
  groups — bucket keys as timestamps, group keys as series labels, NULL
  aggregate values as gaps (never zeros — the RFC 0042 §3.5 rule shown,
  not re-derived).
- **RFC0041.4 (runtime schema).** Given a deployment's RFC 0032
  `ourios://query-schema` document, when the query editors initialize, then
  field and promoted-attribute suggestions derive from that document, not
  from names hardcoded in the plugin (severity band names included).
- **RFC0041.5 (compatibility declaration).** The plugin release metadata
  declares its minimum `ourios-server` version, and CI exercises exactly
  that image tag alongside `latest` — a contract break fails the plugin's
  gate, not a user's dashboard.
- **RFC0041.6 (the FinOps dashboard).** This repository carries a committed
  Perses dashboard definition — agent spend by model over time
  (`sum(attr.cost_usd)`), token throughput, and tool-decision mix — that
  renders against the dogfood capture with no manual edits. This is the
  demo artifact the host decision was made for.

## 6. Testing strategy

Per `CLAUDE.md` §6.2, adapted to a TypeScript workspace: RFC0041.1–.5 are
end-to-end tests in the plugin repository (Playwright or the Perses e2e
harness against the GHCR `ourios-server` image; unit tests for the DSL
request/response mapping), pinned to the criterion ids so the mapping stays
greppable. RFC0041.6 is verified by rendering the committed dashboard
against a dogfood capture — the same corpus discipline as RFC0042.9. The
main repository's CI is untouched: the contract surface it already gates
(RFC 0016 shapes, RFC 0032 document, RFC0002.10 YAML-embeddability) is what
the plugin builds on.

## 7. Open questions

- [x] **Is this worth doing now? — RESOLVED yes (2026-07-27).** What changed
      the calculus: RFC 0042 landed typed numeric promotion and RFC0042.9
      verified live spend aggregation over MCP, so a dashboard now shows
      *money*, not just logs — the plugin became the FinOps demo artifact
      rather than a generic API client.
- [x] **Which host — RESOLVED: Perses first (2026-07-27).** Grafana wins the
      measured-effort comparison (§3.1), but Perses wins the posture that
      matters: Apache-2.0 + CNCF end-to-end (Grafana OSS is AGPL),
      dashboards-as-code fitting the GitOps/air-gapped story, and the §5
      deferred-capabilities commitment the roadmap has carried from the
      start. Scoped to all three plugins (the FinOps dashboard needs time
      series). The Grafana datasource remains a cheap later follow-up and
      is not gated by this RFC.
- [x] **§3.4 severity — RESOLVED (RFC0002.21, PR #641).** Ourios's floor
      semantics were the inverse of the OTel Logs SDK's; floors now admit
      unspecified severity, ceilings exclude it, and the rule is compiled into
      the predicate so row-group pruning agrees with it. Shipped independently
      of this RFC's decision.
- [x] **Repository placement — RESOLVED: separate repo (2026-07-27),**
      [`ourios-perses-plugin`](https://github.com/jensholdgaard/ourios-perses-plugin).
      The boundary is the stable public query surface (unlike the rejected
      intra-workspace splits, which cut private co-evolving internals); the
      toolchains, release cadences, and supply-chain postures are disjoint;
      and standalone plugin repositories are the host ecosystem's
      convention. Drift is gated by RFC0041.5, adaptation by RFC0041.4. The
      FinOps dashboard definition stays in this repository (RFC0041.6).
- [ ] **Does a per-row `id` belong in the query response?** Neither host gets
      one today; both spikes synthesize `{ts}-{template_id}-{index}`. Adequate
      for display, not stable across pagination, which matters for live
      tailing. Adding one is an RFC 0016 response-shape change.
- [ ] **Grafana log-volume histogram.** Grafana currently derives the volume
      graph from returned lines only, noting the datasource "does not support
      full-range histograms". Implementing `getLogsVolumeDataProvider` over
      `count by bucket(w)` would give a true full-range graph — the capability
      already exists and is verified. Small, high-value, but only if Grafana
      is chosen.

## 8. References

- RFC 0002 (logs DSL) §3.6 (Perses dashboard authors as primary audience), §4
  P7 + RFC0002.10 (YAML-embeddability, property-tested), §6.3 amendment
  (`bucket(width)` — the time-series path).
- RFC 0016 (query-serving endpoint) — `POST /v1/query`, the surface a plugin
  consumes. RFC 0026 (tenant binding) — why every request carries a tenant.
- RFC 0027 (MCP surface) / RFC 0032 (`ourios://query-schema`) — the existing
  programmatic client, and the introspectable schema a query editor could use
  for autocomplete.
- RFC 0031 (comparative evaluation vs Grafana Loki) — the stated prerequisite,
  now `accepted`; note it uses Loki as a benchmark target, not an API contract.
- `docs/roadmap.md` §5 — "The Perses datasource plugin — deliberately deferred
  (§5), not started. Its stated prerequisite (RFC 0031 close-out) is now met."
- `CLAUDE.md` §1 ("not a Loki clone", "not a managed service"), §7 (layout /
  new-component commitment).
- OpenTelemetry — [Logs Data Model *Comparing Severity*](https://opentelemetry.io/docs/specs/otel/logs/data-model/#comparing-severity)
  (special handling of `SeverityNumber=0` in comparisons is explicitly
  permitted), [*Severity Fields*](https://opentelemetry.io/docs/specs/otel/logs/data-model/#severity-fields)
  (a backend MAY interpret missing severity as INFO), and
  [Logs SDK `LoggerConfig`](https://opentelemetry.io/docs/specs/otel/logs/sdk/#loggerconfig)
  (unspecified severity bypasses minimum-severity filtering).
- Grafana — [logs data frame contract](https://grafana.com/developers/plugin-tools/tutorials/build-a-logs-data-source-plugin),
  [frontend data proxy](https://grafana.com/developers/plugin-tools/how-to-guides/data-source-plugins/fetch-data-from-frontend).
- Perses — [plugin creation](https://perses.dev/perses/docs/plugins/creation/);
  the bundled `Loki` plugin is the reference for a `LogQuery` implementation.
