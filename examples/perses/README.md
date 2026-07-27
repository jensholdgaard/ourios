# Perses FinOps dashboard (RFC 0041)

The committed dashboard definition required by RFC0041.6: agent spend by
model over time (`sum(attr.cost_usd)`), token throughput, tool-decision
mix, and a raw event log — all against a dogfood capture of Claude Code's
own telemetry.

## Prerequisites

- A running Ourios querier with the RFC 0042 typed promotions configured
  (the repo's `dogfood-config.yaml` promotes `cost_usd` as `f64` and
  `input_tokens`/`output_tokens` as `i64`).
- A Perses instance (≥ 0.53) with the
  [`ourios-perses-plugin`](https://github.com/jensholdgaard/ourios-perses-plugin)
  archive installed — it provides `OuriosDatasource`, `OuriosLogQuery`,
  and `OuriosTimeSeriesQuery`.

## Import

```sh
# from the repository root; adjust url + tenant header for your deployment first
percli apply -f examples/perses/datasource.example.json
percli apply -f examples/perses/agent-finops.json
```

`datasource.example.json` proxies `POST /v1/query` (panel queries) and
`POST /mcp` (the query editor's schema suggestions, RFC 0032) to the
querier, injecting the tenant header server-side so the browser never
handles credentials.

## What to expect

Every panel renders from the capture with no edits to this definition
(the RFC0041.6 clause). Hours where the agent was idle produce no bucket,
and NULL aggregates render as gaps, never zeros — that is the RFC 0042
null-propagation contract made visible.
