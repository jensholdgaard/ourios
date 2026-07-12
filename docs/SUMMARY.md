# Summary

[Introduction](./introduction.md)

# Getting started

- [Quickstart (single binary)](./guides/quickstart.md)
- [Configuration](./guides/configuration.md)
- [Docker](./guides/docker.md)
- [Kubernetes (Helm)](./guides/kubernetes.md)
- [Authentication](./guides/authentication.md)

# Architecture

- [Overview]()
- [OTLP log format vs. Ourios miner](./architecture/otlp-log-format.md)
- [Hazards](./hazards.md)
- [Verification](./verification.md)
- [Glossary](./glossary.md)

# Benchmarks

- [Goals and thesis gates](./benchmarks.md)
- [Roadmap to MVP](./roadmap.md)

# RFCs

- [Process](./rfcs/README.md)
- [RFC 0001 — Template miner](./rfcs/0001-template-miner.md)
- [RFC 0002 — Query DSL](./rfcs/0002-query-dsl.md)
- [RFC 0003 — OTLP receiver](./rfcs/0003-otlp-receiver.md)
- [RFC 0004 — Configuration policy](./rfcs/0004-configuration-policy.md)
- [RFC 0005 — Parquet storage](./rfcs/0005-parquet-storage.md)
- [RFC 0006 — Bench harness](./rfcs/0006-bench-harness.md)
- [RFC 0007 — Querier](./rfcs/0007-querier.md)
- [RFC 0008 — Write-ahead log](./rfcs/0008-wal.md)
- [RFC 0009 — Compaction](./rfcs/0009-compaction.md)
- [RFC 0010 — Audit-stream queries & template drift](./rfcs/0010-audit-stream-queries.md)
- [RFC 0011 — A1 re-scope](./rfcs/0011-a1-rescope.md)
- [RFC 0012 — meta: CLAUDE.md §2 pillar-#2 wording](./rfcs/0012-claude-md-pillar-2-wording.md)
- [RFC 0013 — Object storage (S3-compatible)](./rfcs/0013-object-storage.md)
- [RFC 0014 — Ingest write path (record sink & flush)](./rfcs/0014-ingest-write-path.md)
- [RFC 0015 — Fuzzing harness](./rfcs/0015-fuzzing-harness.md)
- [RFC 0016 — Query-serving endpoint](./rfcs/0016-query-serving-endpoint.md)
- [RFC 0017 — Template registry & query rendering](./rfcs/0017-template-registry-query-rendering.md)
- [RFC 0018 — OTLP log-spec compliance amendments](./rfcs/0018-otlp-log-spec-compliance.md)
- [RFC 0019 — Storage-backend selection](./rfcs/0019-storage-backend-selection.md)
- [RFC 0020 — Configuration file](./rfcs/0020-configuration-file.md)
- [RFC 0021 — DataFusion / Arrow upgrade](./rfcs/0021-datafusion-arrow-upgrade.md)
- [RFC 0022 — Queryable attribute columns](./rfcs/0022-queryable-attribute-columns.md)
- [RFC 0023 — Bounded template memory](./rfcs/0023-bounded-template-memory.md)
- [RFC 0024 — OTLP-envelope property testing](./rfcs/0024-otlp-envelope-property-testing.md)
- [RFC 0025 — Absent-body representation](./rfcs/0025-absent-body-representation.md)
- [RFC 0026 — Authentication & tenant binding](./rfcs/0026-authentication-tenant-binding.md)
- [RFC 0027 — MCP query surface](./rfcs/0027-mcp-query-surface.md)
- [RFC 0028 — Build-feedback program](./rfcs/0028-build-feedback-program.md)
- [RFC 0029 — OIDC bearer layer](./rfcs/0029-oidc-bearer-layer.md)
- [RFC 0030 — TLS/mTLS on the listeners](./rfcs/0030-tls-mtls-listeners.md)
- [RFC 0031 — Comparative evaluation vs Loki](./rfcs/0031-comparative-evaluation-loki.md)
- [RFC 0033 — Cached template-map artifact](./rfcs/0033-cached-template-map.md)

# Talks

- [Template mining in Ourios](./talks/0001-template-miner.md)
