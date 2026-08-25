# Query DSL by example

Sixteen sample queries, each stated first in plain English and then in
the logs DSL (RFC 0002), followed by a migration sketch from Grafana
Loki's LogQL and Amazon CloudWatch Logs Insights. This page is also the
RFC 0002 §9 readability sheet: if a query's meaning is not obvious from
the English line above it, that is a defect in the DSL — file an issue.

Every query is a single line: a **predicate** (what rows match),
optionally followed by pipe **stages** (window, aggregate, sort, limit,
project, render). A bare `true` predicate means "no filter". Queries
without a `range(...)` stage get the server's default look-back window.

## The samples

1. Every error or worse in the last hour:

   ```text
   severity >= error | range(-1h, now)
   ```

2. Everything the `checkout` service logged in the last 15 minutes:

   ```text
   service == "checkout" | range(-15m, now)
   ```

3. Errors from `checkout` whose body mentions a timeout:

   ```text
   service == "checkout" and severity >= error and contains(body, "timeout")
   ```

4. How many log lines each service produced today, busiest first:

   ```text
   true | range(-24h, now) | count by service | sort count desc
   ```

5. Error count per template, to find the noisiest failure shape:

   ```text
   severity >= error | count by template_id | sort count desc | limit 20
   ```

6. The ten most recent lines of one template, reconstructed byte for
   byte:

   ```text
   template_id == 42 | sort ts desc | limit 10 | render
   ```

7. Lines matching a regular expression (anchored to the body):

   ```text
   matches(body, "user [0-9]+ locked out")
   ```

8. Everything with a given trace id, across all services:

   ```text
   trace_id == "4bf92f3577b34da6a3ce929d0e0e4736"
   ```

9. Warnings and errors, excluding a known-noisy service:

   ```text
   severity >= warn and not service == "vacuum-daemon"
   ```

10. Lines carrying a specific attribute value (any OTLP attribute is
    addressable, promoted or not):

    ```text
    attr.decision == "deny" | range(-6h, now)
    ```

11. Kubernetes-style resource lookup — one pod's logs:

    ```text
    resource["k8s.pod.name"] == "checkout-7d4b9f-x2m8p" | range(-30m, now)
    ```

12. Total LLM spend by model over the last day (typed numeric
    attribute, RFC 0042):

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
    lossy == true and confidence < 0.5 | project ts, service, body
    ```

## Migrating from LogQL (Grafana Loki)

The structural difference: Loki selects **streams** by label matchers
in `{braces}`, then pipes line filters; Ourios has no stream/label
split — everything (service, resource attributes, log attributes,
severity, template) is one predicate namespace, and aggregation is a
pipe stage instead of a wrapping function.

| You write in LogQL | You write in the Ourios DSL |
|---|---|
| `{service_name="checkout"}` | `service == "checkout"` |
| `{service_name="checkout"} \|= "timeout"` | `service == "checkout" and contains(body, "timeout")` |
| `{service_name="checkout"} \|~ "user [0-9]+"` | `service == "checkout" and matches(body, "user [0-9]+")` |
| `{service_name="checkout"} != "healthz"` | `service == "checkout" and not contains(body, "healthz")` |
| `{env="prod"} \| json \| decision="deny"` | `attr.decision == "deny"` (no extraction step — attributes are already columns) |
| `sum by (service_name) (count_over_time({env="prod"}[24h]))` | `true \| range(-24h, now) \| count by service` |
| `count_over_time({service_name="checkout"} \|= "timeout" [5m])` (rate panel) | `service == "checkout" and contains(body, "timeout") \| count by bucket(5m)` |
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
| `fields @timestamp, @message \| limit 20` | `true \| project ts, body \| limit 20` |
| `filter @message like /timeout/` | `contains(body, "timeout")` |
| `filter @message like /user \d+/` | `matches(body, "user [0-9]+")` |
| `filter level = "ERROR"` | `severity >= error` |
| `stats count(*) by bin(5m)` | `true \| count by bucket(5m)` |
| `stats count(*) by service` | `true \| count by service` |
| `stats sum(cost_usd) by model` | `true \| sum(attr.cost_usd) by attr.model` |
| `sort @timestamp desc \| limit 10` | `true \| sort ts desc \| limit 10` |
| `parse @message "user * logged in from *" as user, ip \| stats count(*) by user` | `template_id == 42 \| count by param(0)` (the miner already parsed it) |

The `parse`-then-`stats` idiom is the one to notice: what Insights does
with an ad-hoc glob at query time, Ourios did once at ingest — the
template is a stored column, so the grouping needs no pattern and costs
no scan-time extraction.

Not carried over from Insights: cross-log-group joins and `dedup` —
out of scope for v1 of the DSL.
