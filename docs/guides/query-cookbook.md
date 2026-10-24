# Query DSL by example

Sixteen sample queries, each stated first in plain English and then in
the logs DSL (RFC 0002), followed by a migration sketch from Grafana
Loki's LogQL and Amazon CloudWatch Logs Insights. This page is also the
RFC 0002 §9 readability sheet: if a query's meaning is not obvious from
the English line above it, that is a defect in the DSL — file an issue.

Every query is a single line: a **predicate** (what rows match),
optionally followed by pipe **stages**. A bare `true` predicate means
"no filter". Queries without a `range(...)` stage get the server's
default look-back window (one hour unless
`querier.default_window_secs` says otherwise).

## What the querier runs today

The grammar has more stages than the querier executes. These run:

- `range(from, to)` — the time window.
- `limit n` — on a query without an aggregation.
- One aggregation: `count [by …]`, or `sum`/`min`/`max`/`avg(path) [by …]`.

`sort`, `project` and `render` parse, but the querier rejects them as
an invalid query ("the `sort` stage is not yet supported by the
querier"). `limit` together with `count` is rejected too. Every record
the querier returns already carries its timestamp and its body,
reconstructed from the template or taken from the retained body.

Two more limits apply to the samples below:

- **Aggregates need promoted attributes.** Grouping by, or summing, an
  `attr.<key>` or `resource.<key>` reads a promoted column. If the key
  is not promoted, the query fails with `grouping by 'attr.model'
  requires the attribute to be promoted …; add 'model' to
  storage.promoted_attributes.log`. Promote keys in the config file
  (`storage.promoted_attributes`, see
  [Configuration](./configuration.md#promoted-attributes)); promotion
  applies to data written after the change. `service` is always
  promoted. Predicates such as `attr.decision == "deny"` work on any
  attribute, promoted or not.
- **Body text matching is limited.** A mined line is stored as its
  template and parameters, not as body text. `body == "…"` and
  `body != "…"` match the full reconstructed line, so they find mined
  lines. `matches(body, …)`, `starts_with(body, …)`, `body =~ …` and
  `body !~ …` are rejected as an invalid query ("`matches` on body is
  not supported yet"). `contains(body, …)` and `ends_with(body, …)` are accepted but
  see only bodies the row retained, so they miss most mined lines
  ([#968](https://github.com/jensholdgaard/ourios/issues/968)). To find
  a family of lines, match its template: `template_id == N`. Find `N`
  with `true | count by template_id` or the MCP `list_templates` tool.

## The samples

1. Every error or worse in the last hour:

   ```text
   severity >= error | range(-1h, now)
   ```

2. Everything the `checkout` service logged in the last 15 minutes:

   ```text
   service == "checkout" | range(-15m, now)
   ```

3. Errors from `checkout` whose whole line is a known message
   (matched against the reconstructed line, so mined lines count):

   ```text
   service == "checkout" and severity >= error and body == "upstream request timeout"
   ```

4. How many log lines each service produced today:

   ```text
   true | range(-24h, now) | count by service
   ```

5. Error count per template, to find the noisiest failure shape:

   ```text
   severity >= error | count by template_id
   ```

6. Ten lines of one template, each with its body rendered back to the
   original bytes (reconstructed from the template, or the retained
   body verbatim for lossy rows):

   ```text
   template_id == 42 | limit 10
   ```

7. Structured events whose name starts with `gen_ai.` (a regular
   expression; add `^`/`$` yourself if you want anchoring). Regexes work
   on `event_name` and `scope`, not on `body`:

   ```text
   event_name =~ "^gen_ai[.]"
   ```

8. Everything with a given trace id, across all services:

   ```text
   trace_id == "4bf92f3577b34da6a3ce929d0e0e4736"
   ```

9. Warnings and errors, excluding a known-noisy service:

   ```text
   severity >= warn and service != "vacuum-daemon"
   ```

10. Lines carrying a specific attribute value (any OTLP attribute is
    addressable in a predicate, promoted or not):

    ```text
    attr.decision == "deny" | range(-6h, now)
    ```

11. Kubernetes-style resource lookup — one pod's logs:

    ```text
    resource["k8s.pod.name"] == "checkout-7d4b9f-x2m8p" | range(-30m, now)
    ```

12. Total LLM spend by model over the last day. `cost_usd` and `model`
    must be promoted log attributes. If the source sends `cost_usd` as
    a number, promote it as `{ key: cost_usd, type: f64 }` (RFC 0042);
    if it sends a string, keep the plain spelling and `sum` parses the
    text:

    ```text
    true | range(-24h, now) | sum(attr.cost_usd) by attr.model
    ```

13. Login-failure count per five-minute bucket — a rate over time:

    ```text
    template_id == 42 | range(-3h, now) | count by bucket(5m)
    ```

14. The same failures broken down by the template's first parameter
    (e.g. which user), pinned to one template:

    ```text
    template_id == 42 | range(-3h, now) | count by param(0)
    ```

15. Structured OTel events by name (RFC 0043/0044):

    ```text
    event_name == "gen_ai.client.inference.operation.details" | limit 50
    ```

16. Low-confidence parses that kept their original body — the miner's
    own honesty check:

    ```text
    lossy == true and confidence < 0.5 | limit 50
    ```

## Migrating from LogQL (Grafana Loki)

The structural difference: Loki selects **streams** by label matchers
in `{braces}`, then pipes line filters; Ourios has no stream/label
split — everything (service, resource attributes, log attributes,
severity, template) is one predicate namespace, and aggregation is a
pipe stage instead of a wrapping function.

Loki's line filters (`|=`, `!=`, `|~`) have no reliable body
equivalent yet (see [body text matching](#what-the-querier-runs-today)).
Match the line's template instead; `N` below is the template id of the
lines you are after.

| You write in LogQL | You write in the Ourios DSL |
|---|---|
| `{service_name="checkout"}` | `service == "checkout"` |
| `{service_name="checkout"} \|= "timeout"` | `service == "checkout" and template_id == N` |
| `{service_name="checkout"} \|~ "user [0-9]+"` | `service == "checkout" and template_id == N` (the miner already turned `user 42` into `user <*>`) |
| `{service_name="checkout"} != "healthz"` | `service == "checkout" and template_id != N` |
| `{env="prod"} \| json \| decision="deny"` | `attr.decision == "deny"` (no extraction step — attributes are already columns) |
| `sum by (service_name) (count_over_time({env="prod"}[24h]))` | `true \| range(-24h, now) \| count by service` |
| `count_over_time({service_name="checkout"} \|= "timeout" [5m])` (rate panel) | `service == "checkout" and template_id == N \| count by bucket(5m)` |
| `{...} \| logfmt \| level="error"` | `severity >= error` (severity is first-class and numeric; no line-format parsing) |

Two things have no LogQL counterpart and come free here: `template_id`
(query the *shape* of a line, pre-clustered at ingest — no regex needed
to group "user N logged in" lines) and `param(n)` (group by a
template's wildcard value without an extraction stage).

Not carried over from LogQL: `unwrap`-style duration/bytes conversions
and metric-query arithmetic (`/`, `+` between range vectors) — the DSL
returns counts and typed-attribute aggregates, and arithmetic between
result sets is the caller's job today.

## Migrating from CloudWatch Logs Insights

Insights is closest in spirit — a pipe language over discovered fields
— so most queries transliterate stage by stage:

| You write in Logs Insights | You write in the Ourios DSL |
|---|---|
| `fields @timestamp, @message \| limit 20` | `true \| limit 20` (every record carries its timestamp and body) |
| `filter @message like /timeout/` | `template_id == N` (body substring and regex: see above) |
| `filter @message like /user \d+/` | `template_id == N` |
| `filter level = "ERROR"` | `severity >= error` |
| `stats count(*) by bin(5m)` | `true \| count by bucket(5m)` |
| `stats count(*) by service` | `true \| count by service` |
| `stats sum(cost_usd) by model` | `true \| sum(attr.cost_usd) by attr.model` (both keys promoted) |
| `sort @timestamp desc \| limit 10` | not yet: `sort` parses but the querier rejects it |
| `parse @message "user * logged in from *" as user, ip \| stats count(*) by user` | `template_id == 42 \| count by param(0)` (the miner already parsed it) |

The `parse`-then-`stats` idiom is the one to notice: what Insights does
with an ad-hoc glob at query time, Ourios did once at ingest — the
template is a stored column, so the grouping needs no pattern and costs
no scan-time extraction.

Not carried over from Insights: cross-log-group joins and `dedup` —
out of scope for v1 of the DSL.
