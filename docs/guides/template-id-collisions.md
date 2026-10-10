# Check a store for template-id collisions

Before v0.12.0, a receiver whose miner snapshot was discarded could
rebuild its template-id counter below ids it had already published
(#898). The next new template could then take an id that existing
rows and audit events already bind to a different template. v0.12.0
(RFC 0059) stops this from happening again, but it does not repair a
store where it already happened.

Run this check once on **every store written by a release before
v0.12.0**. A logged snapshot discard (the
`ourios.receiver.snapshot.discarded` event, e.g. `unknown_version` on an
upgrade or `restore_failed`) is the most visible trigger. A missing
snapshot after WAL frames were reclaimed, or a replaced local root, can
cause the same collision without logging a discard, so the absence of
that event is not evidence of a clean store. The check is read-only.

## The continuous signal

The querier also reports collisions itself. Each time it folds
a tenant's audit stream into the template map, it checks whether one
`(template_id, version)` is bound to two different templates. For every
such pair it finds, it emits:

- the `ourios.template_map.binding.conflicted` log event (WARN),
  carrying `ourios.tenant`, `ourios.template.id` and
  `ourios.template.version`. The event never carries the template texts,
  because they can contain user data.
- the `ourios.template_map.lookup.anomaly = binding_conflict` attribute
  on that acquisition's `ourios.template_map.lookups` data point. Alert
  on any increase of that series.

Two identical bindings, such as a re-emitted event, are not a conflict.
Neither is a widened template (same id, new version), and neither is an
adoption riding an existing leaf, because it restates the leaf's
template exactly. Detection changes nothing about what is served: rows
still render against the last binding.

The check runs only when the querier folds the audit stream. A query
answered from a fresh cached template map (lookup outcome `hit`) does
not fold, so a tenant whose audit stream stops changing goes quiet once
its map is cached (#928). The next audit file the tenant writes triggers a
fold and the signal again.

The signal names the colliding pairs. It does not list the rows that
render ambiguously, and it does not show the texts. For that, and for
stores written before v0.12.0, use the queries below.

## Historical damage assessment

The template map and `list_templates` keep the last binding per
`(template_id, version)`, which hides the earlier one, and the drift
query never shows template text. So the check reads the Parquet files
directly with [DuckDB](https://duckdb.org/).

## Setup

Give DuckDB read access to the object store:

```sql
CREATE SECRET (TYPE s3, KEY_ID '…', SECRET '…', REGION '…',
               ENDPOINT 'host:port', URL_STYLE 'path');
```

In every query below, replace `s3://BUCKET/PREFIX` with the store's
bucket and `storage.s3.prefix`. For a local store, use the store root
path in place of the `s3://` URL.

## The queries

**1. Within a tenant: one `(template_id, version)` bound to more than
one template text.** This is the corrupting case: rows carrying that id
render against the wrong text.

```sql
SELECT tenant_id, template_id, new_version AS version,
       count(DISTINCT new_template) AS texts,
       list(DISTINCT new_template) AS templates,
       min("timestamp") AS first_bound, max("timestamp") AS last_bound
FROM read_parquet('s3://BUCKET/PREFIX/audit/**/*.parquet', union_by_name = true)
WHERE event_type IN ('template_created', 'template_widened',
                     'template_type_expanded', 'template_adopted')
GROUP BY ALL
HAVING count(DISTINCT new_template) > 1
ORDER BY tenant_id, template_id, version;
```

**2. Across tenants: one `template_id` created under more than one
tenant.** This breaks RFC 0001 §6.1's cluster-wide uniqueness, but it
doesn't corrupt rendering, because each tenant's registry is separate.

```sql
SELECT template_id, list(DISTINCT tenant_id) AS tenants
FROM read_parquet('s3://BUCKET/PREFIX/audit/**/*.parquet', union_by_name = true)
WHERE event_type IN ('template_created', 'template_adopted')
GROUP BY template_id
HAVING count(DISTINCT tenant_id) > 1
ORDER BY template_id;
```

**3. The rows affected by query 1.** These are the rows that render
ambiguously.

```sql
WITH bound AS (
  SELECT tenant_id, template_id, new_version AS version
  FROM read_parquet('s3://BUCKET/PREFIX/audit/**/*.parquet', union_by_name = true)
  WHERE event_type IN ('template_created', 'template_widened',
                       'template_type_expanded', 'template_adopted')
  GROUP BY ALL
  HAVING count(DISTINCT new_template) > 1
)
SELECT r.tenant_id, r.template_id, r.template_version,
       count(*) AS rows, min(r.observed_time_unix_nano) AS first_observed,
       max(r.observed_time_unix_nano) AS last_observed
FROM read_parquet('s3://BUCKET/PREFIX/data/**/*.parquet', union_by_name = true) r
JOIN bound b ON r.tenant_id = b.tenant_id AND r.template_id = b.template_id
            AND r.template_version = b.version
GROUP BY ALL
ORDER BY ALL;
```

These queries were validated against the #898 reproduction, a
discarded tenant re-minting after its WAL was reclaimed:

- query 1 reports the tenant's colliding id with two texts;
- query 2 reports the id shared by two tenants;
- query 3 reports the affected rows.

## Reading the results

- **Query 1 is empty:** no rendering was corrupted.
- **Query 1 has rows:** the listed `(tenant_id, template_id, version)`
  binds more than one text. Rows carrying it (query 3) render against
  whichever binding the registry folds last. The other binding's
  original lines are still available wherever the body was retained
  (CLAUDE.md §3.1 and §3.3), but rendered output for those rows is
  ambiguous. Record the affected time range from `first_observed` and
  `last_observed`, and open an issue if the data matters.
- **Only query 2 has rows:** ids repeat across tenants, but rendering is
  correct.

## Caveats

- An adoption that reuses an existing leaf restates the leaf's
  template text byte for byte. The miner adopts onto a leaf only when
  the upstream template's tokens equal the leaf's, and both texts are
  written from the same tokens. So an adoption is not a false positive
  in query 1.
- The `data/**` glob in query 3 can include files that compaction has
  replaced but not yet removed, so a row may be counted twice.
- The audit scans in queries 1 and 2 (and query 3's `bound` CTE) must
  cover a tenant's **whole** audit history: the two bindings of one id
  can be days apart, and a day-narrowed glob would see only one of them
  and report no collision. You may narrow the audit glob to a single
  tenant's prefix (`audit/tenant_id=<tenant>/**`), never by day. Only the
  `data/**` glob in query 3 may be narrowed to the days query 1 reported
  (`first_bound` to `last_bound` and later).
