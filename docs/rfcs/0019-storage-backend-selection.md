---
rfc: 0019
title: Storage-backend selection — wiring the server to choose local vs S3
status: accepted
author: Jens Holdgaard Pedersen <jens@holdgaard.org>
drafting-assistance: Claude
created: 2026-06-22
supersedes: —
superseded-by: —
---

# RFC 0019 — Storage-backend selection: wiring the server to choose local vs S3

## 1. Summary

`ourios-server` always constructs `Store::local(OURIOS_BUCKET_ROOT)` today,
even though `ourios-parquet` already exposes `Store::s3(S3Config)` (RFC 0013,
`green`). This RFC wires **backend selection** through the server: an operator
picks `local` or `s3` via config, and the chosen `Store` is threaded into all
three roles. To make S3 actually usable, the querier and the compactor — which
still address the bucket through raw `std::fs` — are migrated onto the
`Store` / `object_store` abstraction the receiver already uses. The
write-ahead log stays local always (`CLAUDE.md` §3.6). This is the follow-on
RFC 0014 §7 and RFC 0013 §7 named; it is the prerequisite for an
object-storage-native deployment (and the `CLAUDE.md` §3.6-correct Helm chart).

## 2. Motivation

`CLAUDE.md` §3.6 makes object storage the source of truth: *"Local disk is
cache and WAL. Parquet on S3 is the truth."* RFC 0013 built the storage seam
(`Store`, `S3Config`, conditional-PUT atomics) and proved it on localstack, but
deferred the **selection** at the server config layer. The consequence today is
concrete: a deployment cannot put data on S3, so the first Helm chart had to
back the data store with a local `ReadWriteOnce` volume and a single replica —
a stopgap that contradicts `CLAUDE.md` §3.6 and blocks horizontal querier
scaling. Doing
selection at this layer, now, unblocks the architecturally-correct shipping
shape and exercises the RFC 0013 S3 path end-to-end through the real server.

The work is at this layer (the server + the querier/compactor read paths)
because that is the only place the bucket is still addressed as a local path;
the receiver write path (RFC 0014) already goes through `Store`.

## 3. Proposed design

### 3.1 Configuration (extends RFC 0004)

A new **startup configuration surface** — the storage backend and its
addressing — is added under RFC 0004's governance (its validation +
secret-hygiene rules). It is *not* an RFC 0004 **tunable** in the strict sense:
a tunable is global-with-per-tenant-override, whereas backend selection is
necessarily **process-wide** (one store per process). Credentials are not
Ourios configuration at all: they are operator secrets resolved by the standard
AWS credential chain, **or** supplied explicitly as S3-named secret keys
(`OURIOS_S3_ACCESS_KEY_ID` / `OURIOS_S3_SECRET_ACCESS_KEY` /
`OURIOS_S3_SESSION_TOKEN`), distinct from the non-secret addressing keys above
and never logged (see §3.4; added by the 2026-06-28 amendment, §9).

| Env var | Backend | Meaning | Default |
| --- | --- | --- | --- |
| `OURIOS_STORAGE_BACKEND` | both | `local` or `s3` | `local` |
| `OURIOS_BUCKET_ROOT` | local | data + audit store root (existing) | — (required for `local`) |
| `OURIOS_S3_BUCKET` | s3 | bucket name | — (required for `s3`) |
| `OURIOS_S3_ENDPOINT` | s3 | S3-compatible endpoint (MinIO, R2) | unset (AWS) |
| `OURIOS_S3_REGION` | s3 | region | unset |
| `OURIOS_S3_PREFIX` | s3 | key prefix within the bucket | unset (bucket root) |
| `OURIOS_S3_ACCESS_KEY_ID` | s3 | static access key (**secret**, §3.4) | unset (→ credential chain) |
| `OURIOS_S3_SECRET_ACCESS_KEY` | s3 | static secret key (**secret**, §3.4) | unset (→ credential chain) |
| `OURIOS_S3_SESSION_TOKEN` | s3 | session token for temporary credentials (**secret**) | unset (valid only with the static key pair) |

`OURIOS_WAL_ROOT` is unchanged and remains a **local** path under every
backend (`CLAUDE.md` §3.6 — the WAL is never an object-store key). "Local" here
means **fsync-durable local-filesystem semantics, not ephemeral storage**: the
WAL is the recovery mechanism (WAL-before-ack, `CLAUDE.md` §3.4), so the path
MUST be backed by storage that survives a process/pod crash — i.e. a persistent
volume, never a scratch/`emptyDir`-style mount. S3 is deliberately *not* used
for the WAL: it offers no atomic append or fsync and would put S3 PUT latency on
the ack path, defeating `CLAUDE.md` §3.4's batched-fsync latency/durability
knob; S3 is the truth for the *flushed* Parquet, which is all `CLAUDE.md` §3.6
requires. The WAL's durability obligation is bounded by the flush horizon
(`CLAUDE.md` §3.6 — local disk need not be durable *beyond* it). Surviving the
loss of the volume itself (node/AZ failure) is a separate, out-of-scope tier —
WAL **replication / archiving**, which `CLAUDE.md` §3.4 reserves as an addition
to the WAL, not a replacement, and which a future RFC may add. The
prior art is the PostgreSQL model (CloudNativePG's Barman Cloud,
`barman-cloud-wal-archive`): a hot fsync'd WAL on a local persistent volume,
*plus* asynchronous archiving of completed segments to object storage for
off-node recovery (§8).

### 3.2 The `StoreConfig` seam

`ourios-server` replaces the `bucket_root: PathBuf` it threads to each role
with a resolved, validated descriptor:

```rust
enum StoreConfig {
    Local(PathBuf),   // OURIOS_BUCKET_ROOT
    S3(S3Config),     // OURIOS_S3_* (S3Config is the RFC 0013 type)
}
```

`config_from_env` parses `OURIOS_STORAGE_BACKEND` and fails fast on a missing
required field (`OURIOS_S3_BUCKET` when `s3`; `OURIOS_BUCKET_ROOT` when
`local`) or an unknown backend. `StoreConfig::open() -> Result<Store, …>`
dispatches to `Store::local` / `Store::s3`. The receiver, compactor, and
querier each take a `StoreConfig` (or a constructed `Store`) instead of a
`PathBuf`.

```mermaid
flowchart LR
  env[OURIOS_STORAGE_BACKEND + addressing] --> cfg{StoreConfig}
  cfg -->|Local| sl[Store::local]
  cfg -->|S3| ss["Store::s3 / AmazonS3Builder::from_env()"]
  sl --> store[(Store)]
  ss --> store
  store --> rcv[receiver write path]
  store --> cmp[compactor sweep]
  store --> qry[querier read path]
  wal[OURIOS_WAL_ROOT] -->|always local| rcv
```

### 3.3 Migrating the querier and compactor onto `Store`

- **Querier.** The bulk Parquet scan moves to DataFusion's native
  object-store support: register the `Store`'s `object_store` on the
  `SessionContext` and address tables by object-store URL rather than a local
  `ListingTableUrl` path. The audit-stream helpers that read with `std::fs`
  (`audit_scan`, `alias_store::derive_alias_map`,
  `template_registry::derive_template_registry`) move to `Store` listing +
  `get_blocking`. `Querier::new` takes a `Store` (or `StoreConfig`).
- **Compactor.** The filesystem walks (`tenants`, `plan_candidates`,
  `compact_partition`, `gc_orphans`) move to `Store` listing + the
  `ourios-parquet` `Store`-based read/write/delete. The manifest swap adopts
  `Manifest::publish_cas` (conditional PUT, RFC0013.3/.4) so concurrent or
  retried sweeps cannot clobber a generation. `Compactor::new` takes a `Store`.

`Store` exposes object/key I/O (`get_blocking`/`put_blocking`/…) but not yet a
listing method (listing lives on the inner `object_store::ObjectStore`). This
RFC's implementation adds a thin **`Store` listing wrapper** over
`ObjectStore::list` (prefix → keys, bridged off-runtime like the existing
blocking helpers) so the querier and compactor never reach past the `Store`
seam; the alternative — calling `ObjectStore::list` directly via
`Store::object_store()` — is equivalent but leaks the abstraction.

Both migrations preserve the on-disk layout and the partition key scheme
(RFC 0005 §3.4) byte-for-byte — only the *addressing* changes (a local path
vs. an object-store key under the prefix), so historical local stores and the
existing reader/writer remain valid (RFC 0013 §3.2).

### 3.4 Credentials and secret hygiene

S3 credentials resolve **explicit-over-chain**:

1. **Explicit Ourios config.** `OURIOS_S3_ACCESS_KEY_ID` /
   `OURIOS_S3_SECRET_ACCESS_KEY` (and optionally `OURIOS_S3_SESSION_TOKEN`), when
   set, are read by Ourios and applied to the `AmazonS3Builder`
   (`with_access_key_id` / `with_secret_access_key` / `with_token`). These are
   **S3-API** names, not AWS-the-cloud names — they authenticate AWS S3 and every
   S3-compatible store (MinIO, R2, Hetzner, Ceph, …) identically. The static
   access key and secret are a pair: setting one without the other, or a session
   token without that pair, fails fast (the error names only the offending key,
   never a value).
2. **The standard credential chain (fallback).** When the explicit keys are all
   unset, `AmazonS3Builder::from_env()` resolves the usual way: static `AWS_*`
   keys, a shared profile, **IRSA**, or instance metadata. Retained because AWS
   IRSA injects its own `AWS_ROLE_ARN` / `AWS_WEB_IDENTITY_TOKEN_FILE` (the EKS
   pod-identity webhook, outside Ourios's control), for which there is no
   Ourios-named equivalent.

**Secret hygiene.** Credential and secret values MUST never appear in logs,
error messages, metric attributes, or a `Debug` rendering. Ourios reads the
explicit `OURIOS_S3_*` credential keys, so it owns their redaction — `S3Config`'s
`Debug` shows only credential *presence*, `StoreError` withholds backend
internals, and a missing-required-config error names only the *key*
(`OURIOS_S3_BUCKET`), never a credential. Non-secret config values (an addressing
knob, an interval) MAY be echoed in a resolution error for diagnosability — e.g.
the `OURIOS_COMPACTION_INTERVAL_SECS` parser reporting the offending value —
since those carry no secret; the prohibition is specifically on credential/secret
material. (Introduced by the 2026-06-28 amendment, §9.)

## 4. Alternatives considered

- **Overload `OURIOS_BUCKET_ROOT` with an `s3://bucket/prefix` URL.** One var,
  no new knobs — but it conflates path, addressing, endpoint, and region into a
  single string, hides the MinIO/R2 endpoint override, and couples config
  parsing to `object_store`'s URL grammar. Rejected for a flat, explicit knob
  set that RFC 0004 can govern.
- **Only the receiver writes S3; querier/compactor stay local.** Incoherent —
  the data store is a single backend; a querier reading a local path would find
  nothing the S3 receiver wrote. Rejected.
- **Project S3 as a filesystem (CSI / s3fs mount).** Lets the existing `std::fs`
  code run unchanged, but defeats the conditional-PUT atomicity RFC 0009/0013
  rely on for the manifest swap, and adds an opaque failure surface. Rejected.
- **Defer (keep local-only).** Leaves the shipping chart on a single-replica
  RWO stopgap that contradicts `CLAUDE.md` §3.6 and blocks querier scaling.
  Rejected — this RFC is the unblock.

## 5. Acceptance criteria

> **Scenario RFC0019.1 — backend selection from config**
> - **Given** `OURIOS_STORAGE_BACKEND` unset and `OURIOS_BUCKET_ROOT` set
> - **When** the server resolves its config
> - **Then** it selects the local backend from `OURIOS_BUCKET_ROOT`;
>   **and** with `OURIOS_STORAGE_BACKEND=s3` + `OURIOS_S3_BUCKET` it
>   selects S3; **and** `s3` without `OURIOS_S3_BUCKET`, or an unknown
>   backend value, is a clear fail-fast startup error.

> **Scenario RFC0019.2 — the WAL stays local under every backend (`CLAUDE.md` §3.6)**
> - **Given** `OURIOS_STORAGE_BACKEND=s3`
> - **When** the receiver role runs
> - **Then** the WAL is written under the local `OURIOS_WAL_ROOT` and
>   never as an object-store key; the data + audit Parquet go to S3
>   (extends RFC0013.6).

> **Scenario RFC0019.3 — end-to-end ingest→query on S3**
> - **Given** the server configured for an S3-compatible backend (localstack)
> - **When** a batch is ingested and a DSL query runs
> - **Then** the Parquet lands under the S3 prefix and the query returns
>   the rows with non-zero pruning stats — the same result the local
>   backend produces.

> **Scenario RFC0019.4 — compaction operates on S3**
> - **Given** several small files for a partition on the S3 backend
> - **When** a compaction sweep runs
> - **Then** they are consolidated via `Store` I/O and the manifest is
>   swapped with a conditional PUT (`publish_cas`); a losing concurrent
>   sweep does not clobber the winning generation (RFC0013.3/.4).

> **Scenario RFC0019.5 — tenant isolation on S3 (`CLAUDE.md` §3.7)**
> - **Given** two tenants' data on the S3 backend
> - **When** one tenant queries
> - **Then** only that tenant's prefix is read; another tenant's objects
>   are never returned.

> **Scenario RFC0019.6 — config is governed by RFC 0004; no secret leakage**
> - **Given** S3 credentials supplied via the AWS chain
> - **When** the server starts, logs, errors, or exports metrics
> - **Then** no credential value appears in any log line, error message,
>   or metric attribute; a missing-S3-config error names only the missing
>   key, never a credential (non-secret knobs may be echoed for
>   diagnosability) (`CLAUDE.md` §6.3, RFC 0004).

> **Scenario RFC0019.7 — local backend regression**
> - **Given** no `OURIOS_STORAGE_BACKEND` set and `OURIOS_BUCKET_ROOT` set
>   (the default local path)
> - **When** the full existing suite runs
> - **Then** behaviour is byte-for-byte unchanged from the local path
>   today: receiver, querier, and compactor produce identical results,
>   and every pre-existing local test passes.

> **Scenario RFC0019.8 — explicit S3 credentials, S3-named and never leaked**
> - **Given** `OURIOS_S3_ACCESS_KEY_ID` / `OURIOS_S3_SECRET_ACCESS_KEY` set
>   and no `AWS_*` static keys in the environment
> - **When** the server resolves its config and runs an ingest→query against
>   an S3-compatible backend (localstack)
> - **Then** the explicit keys authenticate the store (the round-trip
>   succeeds), confirming Ourios applies them to the builder; **and** when the
>   explicit keys are all unset the standard credential chain
>   (`AmazonS3Builder::from_env()`, including IRSA) is used unchanged; **and** a
>   partial set (one of the static pair, or a token alone) fails fast naming
>   only the offending key; **and** no credential value ever appears in a config
>   error, log line, `StoreError`, `Debug` output, or metric attribute —
>   extending RFC0019.6's redaction to the S3 credential keys.

## 6. Testing strategy

All eight scenarios have passing tests; the RFC is `accepted` (§9).

- **RFC0019.1 / .6 / .7** — unit tests on `build_store_config` / `build_config`
  (the `main.rs` pattern), including the missing-key / secret-scrub assertion
  for hygiene and the local-default regression. They live in
  `crates/ourios-server/src/main.rs` (`rfc0019_1_*` / `rfc0019_6_*` /
  `rfc0019_7_*`) and run in the default `cargo test` job.
- **RFC0019.2 / .3 / .4 / .5** — server-level testcontainers + localstack
  integration tests in `crates/ourios-server/tests/rfc0019_storage_backend.rs`,
  reusing the `rfc0013_object_store.rs` harness (`Store::s3` against a localstack
  endpoint) and spawning the `ourios-server` binary configured for the S3
  backend, driven over HTTP. `.2` asserts the WAL stays local while the data
  backend is S3 (the `rfc0013_6_wal_stays_local` pattern); `.3` ingests then
  queries end to end on S3; `.4` runs the background compactor against S3 and
  asserts the conditional-PUT manifest swap; `.5` proves cross-tenant isolation.
  They are `#[ignore]`d for the default `cargo test` run and gated to the CI
  `s3 integration (localstack)` job (Docker-API runtime + the `AWS_*` env),
  invoked by name via `--ignored --exact`.
- **RFC0019.7 (regression)** — in addition to the unit test above, the existing
  local receiver/querier/compactor suites run unchanged over the default config
  path; they are the byte-for-byte regression guard.
- **RFC0019.8** — two halves. The **redaction + validation** half is unit tests:
  the `rfc0019_6_*` no-leak assertion covers the `OURIOS_S3_*` secret keys, a
  `with_s3_credentials` test (`main.rs`) that the explicit keys land in
  `S3Config` (blank reads as unset, `local` carries none), and
  `ourios-parquet` `store` tests that `Store::s3` accepts a full pair, fails
  fast on a partial set without echoing the value, and that `S3Config`'s `Debug`
  redacts credentials. The **authentication** half is the localstack
  `rfc0019_8_explicit_s3_credentials_authenticate` integration test (the server
  configured with the S3 credential keys only, `AWS_*` removed), gated to the
  `s3 integration (localstack)` CI job.

## 7. Open questions

- [ ] **Single-writer lease vs. conditional-PUT contention** (carried from
  RFC 0013 §7) — is `publish_cas` retry sufficient for the compactor under
  multi-writer races, or is a dedicated lease object warranted? This RFC adopts
  `publish_cas`; a lease is a follow-up if contention shows up.
- [ ] **Local read cache** for hot S3 objects (RFC 0013 §7) — deferred.
- [ ] **Migration tool** to copy an existing local store to S3 — deferred; new
  deployments start on the chosen backend.
- [ ] **Multipart upload threshold** for the 256 MiB–2 GiB RFC 0009 outputs
  (RFC 0013 §7) — confirm `object_store` defaults suffice or expose a knob.

## 8. References

- RFC 0013 (object-storage backend — `Store`, `S3Config`, conditional-PUT;
  §7 open questions this resolves), RFC 0014 §7 (names this follow-on),
  RFC 0004 (configuration policy — the tunable/invariant line this extends),
  RFC 0005 §3.4 (partition layout, preserved), RFC 0009 (compaction — manifest
  swap), RFC 0007/0016 (the querier read path being migrated).
- `CLAUDE.md` §3.6 (object storage is the source of truth; local disk is cache
  and WAL), §3.7 (multi-tenancy on every data path), §6.3 (observability /
  self-telemetry — no secret leakage).
- `crates/ourios-parquet/src/store.rs` (`Store`, `S3Config`, `StoreError`),
  `crates/ourios-parquet/tests/rfc0013_object_store.rs` (the localstack
  harness), `crates/ourios-server/src/main.rs`,
  `crates/ourios-server/src/receiver.rs`,
  `crates/ourios-server/src/querier.rs`,
  `crates/ourios-ingester/src/compactor.rs`.
- Prior art for the deferred WAL-replication/archive tier (§3.1): PostgreSQL
  WAL archiving (`archive_command` / `archive_library`) and CloudNativePG's
  Barman Cloud (`barman-cloud-wal-archive`) — the same layering, a hot
  fsync'd WAL on a local persistent volume plus asynchronous shipping of
  completed segments to object storage for off-node recovery / PITR.

## 9. Amendment history

- **2026-06-28 — explicit S3-named credentials.** §3.4 as originally accepted
  introduced "no Ourios-specific credential config" and resolved S3 credentials
  solely through `AmazonS3Builder::from_env()` (the AWS-SDK-named chain). Ourios
  is **S3-compatible**, not AWS-specific (MinIO, Cloudflare R2, Hetzner,
  Ceph/RADOS, GCS S3-interop, …), so its credential surface should read as S3.
  This amendment added the S3-named credential keys
  (`OURIOS_S3_ACCESS_KEY_ID` / `OURIOS_S3_SECRET_ACCESS_KEY` /
  `OURIOS_S3_SESSION_TOKEN`, §3.1 table + §3.4) layered explicit-over-chain —
  the chain retained as the fallback AWS IRSA requires — with the partial-set
  fail-fast and the widened redaction, and added acceptance scenario RFC0019.8
  (§5). Specified, then implemented in the same change set; the RFC stays
  `green` (all eight §5 criteria pass).
- **2026-07-10 — accepted (maintainer sign-off).** Promoted `green →
  accepted` (terminal). All eight §5 criteria have passing tests (green since
  #301, amended #306/#307): unit coverage of backend selection + credential
  scrub, and the localstack S3 integration covering WAL-stays-local, the
  ingest→query round-trip on S3, the compactor's conditional-PUT manifest
  swap, and cross-tenant isolation. No `validated` stage applies — backend
  selection is server wiring, not a thesis-gate benchmark — so acceptance
  follows `green` directly (the RFC 0001 / 0008 precedent).
