# Check a store for template-id collisions

Before v0.12.0, a receiver whose miner snapshot was discarded could
rebuild its template-id counter below ids it had already published
(#898). The next new template could then take an id that existing
rows and audit events already bind to a different template. v0.12.0
(RFC 0059) stops this from happening again, but it does not repair a
store where it already happened.

Run this check once on any store that ran a release before v0.12.0 and
saw a snapshot discard. A discard shows up in the logs as the
`ourios.receiver.snapshot.discarded` event, for example
`unknown_version` on an upgrade or `restore_failed`. The check is
read-only.

No Ourios query surface shows these collisions. The template map and
`list_templates` keep the last binding per `(template_id, version)`,
which hides the earlier one, and the drift query never shows template
text. So the check reads the Parquet files directly with
[DuckDB](https://duckdb.org/).

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

- An adoption that reuses an existing leaf can show up as a false
  positive in query 1, if its canonical text is formatted differently
  from the leaf's.
- The `data/**` glob in query 3 can include files that compaction has
  replaced but not yet removed, so a row may be counted twice.
- The audit and data globs read the whole history. On large stores,
  narrow the glob to the tenant prefixes or day partitions you care
  about.
