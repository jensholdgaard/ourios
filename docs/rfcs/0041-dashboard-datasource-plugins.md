---
rfc: 0041
title: Dashboard datasource plugins — Ourios as a Grafana / Perses source
status: drafted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-07-25
supersedes: —
superseded-by: —
---

# RFC 0041 — Dashboard datasource plugins

> **Status: `drafted` (2026-07-25).** §§1–4 and §§7–8 are filled per
> `docs/rfcs/README.md`'s lifecycle; §5 acceptance criteria and §6 testing
> strategy are deliberately **not** written, because the open question is not
> *how* to build this but *whether it is worth building and where*. Both
> options were spiked to a rendered dashboard before this was written (§3.1),
> so the estimates here are measured rather than guessed. **This RFC asks for
> a decision, not for implementation.**

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
plugin system. Measured effort: **1–2 days for Grafana, 3–5 for Perses.**

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
| Time series from `count by bucket(w)` | ✅ | not built (needs a third plugin) |
| Plugins to write | 1 datasource | 2 (`Datasource` + `LogQuery`) |
| Backend language | **none** (data proxy) | none (frontend + Cue) |
| Schema language | none | **Cuelang, mandatory** |
| Config validated | at render | **at write time** |
| Measured effort | **1–2 days** | **3–5 days** |

Findings that shape any real implementation:

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

The dashboard time range becomes a `range(...)` stage. **A range the user
wrote by hand wins** — silently overriding it would make the editor lie about
what ran.

### 3.3 Where the plugin lives

A plugin is TypeScript; this is a Rust workspace. `CLAUDE.md` §7 pins the
layout and makes a new component an architectural commitment. The plugin
should therefore live in **its own repository**, not in `crates/`. That keeps
this repo's toolchain single-language and lets the plugin version against its
host rather than against Ourios releases.

### 3.4 The severity gap (needs a decision either way)

Confirmed live: a natural first query — `severity >= trace` — returns **zero
rows** against real agent telemetry, because Claude Code's GenAI events carry
`severity_number: 0`, below `trace`, so every row group prunes. Through a
dashboard this looks like a broken datasource with no explanation.

Storing the `0` is correct — it is what the source sent, and
`project_otlp-fidelity-paramount`'s preserve/flag/never-correct rule applies.
The question is what a **comparison** should do with it, and the OTel logs
data model addresses that directly (consultation, 2026-07-25):

> In the contexts where severity participates in less-than / greater-than
> comparisons `SeverityNumber` field should be used. **Special handling MAY be
> given to `SeverityNumber=0` when it is used to represent an unspecified
> severity.**
> — [Logs Data Model, *Comparing Severity*](https://opentelemetry.io/docs/specs/otel/logs/data-model/#comparing-severity)

So the spec anticipates exactly this case and leaves the handling to us. Three
options, each with upstream precedent:

1. **Unspecified bypasses severity filters.** This is what OTel's *own* SDK
   does: "If a log record's `SeverityNumber` is specified (i.e. not `0`) and is
   less than the configured `minimum_severity`, the log record MUST be dropped
   … Log records with an unspecified severity (i.e. `0`) are **not affected by
   this parameter and therefore bypass minimum severity filtering**"
   ([Logs SDK, `LoggerConfig`](https://opentelemetry.io/docs/specs/otel/logs/sdk/#loggerconfig)).
   Under this reading `severity >= trace` would *return* the GenAI events —
   the current behaviour is the inverse of the SDK's. Most spec-aligned;
   changes existing query semantics, so it is a DSL contract change (RFC 0002).
2. **Interpret unspecified as INFO (9).** Explicitly permitted: "Backend and UI
   … may interpret log records with missing `SeverityNumber` and `SeverityText`
   fields as if the `SeverityNumber` was set equal to INFO (numeric value of
   9)" ([Logs Data Model, *Severity Fields*](https://opentelemetry.io/docs/specs/otel/logs/data-model/#severity-fields)).
   Simplest to explain; but it invents a severity the source did not send,
   which sits badly against the fidelity rule if applied at **storage** time.
   Applied only at **query** time it is defensible.
3. **Make it explicit at the query surface.** The Collector's
   `attributesprocessor` takes this route with a `match_undefined` boolean —
   "controls whether logs with 'undefined' severity matches". A DSL equivalent
   keeps today's default, makes the choice visible, and does not silently
   change results.

Option 1 is the most spec-faithful and option 3 the most conservative; option 2
is a middle path but should be query-time only. **Note this is a pruning
correctness question, not only a UX one**: whichever semantics win, the
row-group pruning must agree with them, or a filter will skip files that
contain matching rows.

This wants deciding regardless of which host wins — it is the one finding here
that touches Ourios rather than a plugin.

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
`#selector` from the datasource schema, and pins a Cue module
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

Deliberately empty at `drafted`. To be written when §7's first question is
answered; a plugin in its own repository (§3.3) would carry its own criteria,
with only the §3.4 severity decision landing against this repo.

## 6. Testing strategy

Deliberately empty at `drafted`. See §5.

## 7. Open questions

- [ ] **Is this worth doing now?** The honest framing: this is a client for a
      stable API, not engine work. Nothing in `CLAUDE.md` §2 moves. Competing
      calls on the same time include RFC 0036's implementation (the remaining
      storage lever against hazard #4), the RFC 0009 D1/D2 soak cadence, and
      the agent-observability/FinOps direction. **Answer this before the
      others.**
- [ ] **Which host — Grafana, Perses, or both?** §4 lays out the trade;
      §3.1 gives measured effort for each.
- [ ] **§3.4 severity — which of the three options?** `severity >= trace`
      returning nothing against real agent telemetry is Ourios-side and wants a
      decision whether or not a plugin is built. The OTel spec explicitly
      permits special handling of `SeverityNumber=0` in comparisons, and OTel's
      own SDK filter lets unspecified records *bypass* severity filtering —
      i.e. today's behaviour is the inverse of the SDK's. Options 1 and 3 in
      §3.4 change DSL semantics and so need an RFC 0002 amendment; option 2 is
      query-time only. Whichever wins, row-group pruning must be made to agree
      with it (a pruning-correctness issue, not just UX).
- [ ] **Repository placement.** §3.3 argues for a separate repo. If it lives
      here instead, that is a `CLAUDE.md` §7 layout change and needs a `meta:`
      RFC.
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
