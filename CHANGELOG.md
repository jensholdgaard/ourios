# Changelog

All notable changes to this project will be documented in this file.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) · SemVer.
## [0.4.0] - 2026-07-21

### Added

- Rfc 0035 green — receiver.encode_workers config + default pool wiring (468ecf7)
- Rfc 0035 green — submission under the gate + encode-drain-and-flush barrier (ab4f8fc)
- Rfc 0035 green — bounded encode pool + queue-depth instrument (cd036fc)
- Rfc 0035 green — ordered-phase capture + concurrent sink emit (e8419b6)
- In-process multi-tenant soak load (--tenants N) (#570) (bc92ce8)

### Documentation

- §9.23 asserting soak — enact rfc 0034 d1 recast, rfc 0035 green (#580) (b32edbc)
- Unlink private append_off_lock from emit_concurrent rustdoc (a38a465)
- Configuration — receiver.encode_workers (RFC 0035) (f2c85a9)
- Rfc 0034 — status specified (follow-up to #575) (#576) (22d3bbd)
- Rfc 0034 — d1 re-scope on measured premises (#575) (b6be711)
- §9.21 + §9.22 — in-process ceiling, profile, design a a/b (#574) (5d1fbe2)
- Rfc 0035 — name the bit-partitioned u64 as design b's cheaper shape (#573) (66d1ca3)
- Rfc 0035 — ingest concurrency (relax global miner serialization) (#572) (9ad3158)

### Fixed

- Rfc 0035 review fixes — capture-slot unwind safety, honest barrier docs (#579) (d2c622e)

### Tests

- Rfc 0035 green — §5 acceptance suites (.1/.2/.5) (0b6f112)

## [0.3.0] - 2026-07-20

### Added

- RFC 0009 D1/D2 sustained-ingest soak harness (#558) (0979d14)
- Per-role ServiceAccounts — least-privilege IAM seam (IRSA) (#556) (780114e)
- Rfc 0031 §7 — freeze M_L4 on the L2 shape (#548) (0a0c0ca)
- RFC 0031 — record per-pair completeness as a results artifact (#540) (fe82f42)
- RFC 0031 L4 — wire into the live dispatch loop (#536) (0706b7b)
- RFC 0031 L4 harness — aggregation pair, matrix parser, param picker (#534) (44c292e)
- Rfc 0002 green — l4 validation error contract (.14) (#535) (2b46313)
- RFC 0002 green — count-by execution with param(n) and bucket(w) (#533) (b089313)
- Rfc 0031 §7 — m_l2 unfrozen and asserting per rfc 0033 (run #21) (#528) (7faec23)
- RFC 0033 v2 — zstd artifact at the v2 key, publish-outcome labels (#522) (02d622e)
- RFC 0032 green (correctness + descriptions) — .3/.4/.5/.6 (#519) (77ae4b4)
- Rfc 0032 green (resource) — query-schema resource + config threading, .1/.2 (#518) (e1ef0e2)
- RFC 0033 green (observability) — lookup-outcome telemetry, .7 (#513) (707950d)
- RFC 0033 green (comparative) — cold-vs-warm acquisition, .6 (#512) (c43d5cb)
- RFC 0033 green (freshness) — cached read path + write-through, .2/.4/.5 (#511) (8fd8e1d)
- RFC 0033 green (publish) — atomic template_map publish, CAS + tear safety (#510) (3aefac6)
- RFC 0033 green (artifact) — template_map format + one-scan dual fold (#509) (a63f04d)
- RFC 0031 — frozen gates asserted; scenarios .2/.4/.7/.11 green (#506) (01303e4)
- RFC 0031 — latency_p50 channel (RFC0031.7 becomes measurable) (#495) (13087bd)
- RFC 0031 — selective-resource window diagnostic pair (#493) (844d413)
- Rfc 0031 — l1 template-lookup pair (second must-win candidate) (#492) (fddd68d)
- RFC 0005 §3.6 — bloom filters on trace_id and span_id (#489) (44296d2)
- RFC 0031 — L3 trace-correlation pair (first must-win class beyond L2) (#487) (aeb52b7)
- RFC 0031 — honest total-bytes accounting (count + materialize + registry) (#482) (afb1fb3)
- RFC 0031 — floor-direction gate for L6/L7 reporting (#481) (cf79b3d)
- RFC 0031 — three-point selectivity curve in the indicative run (#480) (5190021)
- RFC 0031 — storage-side Loki bytes (conservative metric) (#479) (b05fc86)
- RFC 0031 — indicative comparative run + dispatch workflow (#474) (2ce2c6a)
- RFC 0031 — L-gate must-win math + §7 margins as config (#473) (a9c2137)
- Rfc 0031 — bytes-read measurement channel (§3.6) (#472) (d535099)
- RFC 0031 — Loki container integration, RFC0031.1 green (#471) (b26fb7b)
- RFC 0031 — Loki query-response parser (RFC0031.1) (#470) (2e0c918)
- RFC 0031 — registry-bearing comparative store (RFC0031.1) (#469) (05ea638)
- RFC 0031 — Ourios-side query extraction (RFC0031.1) (#468) (99929b2)
- RFC 0031 — result-set equivalence comparator (RFC0031.1 core) (#467) (b6a1c8d)
- Harvest miner §3.1 counters into the results (#446) (#458) (7059e85)
- Rfc 0030 green (served .8) — served end-to-end over TLS (#454) (788d84d)
- Redefine C2 gate as per-service (#444) (#451) (b257c9a)
- Rfc 0030 green (querier) — TLS on the querier + MCP surface (#450) (1bfb7f3)
- Rfc 0030 green (reload) — hot cert reload without restart (#449) (0ad6856)
- Rfc 0030 green (acceptor) — gRPC + HTTP listeners over TLS (#447) (ff71eb1)
- Per-service C2 decomposition + v8 B2 pricing (#444) (#445) (abd9108)

### Build

- Bump Helm chart appVersion in the release recipe (#569) (a4a290d)

### CI

- Pin the container rustup install by hash (#553) (918769b)
- Retire self-hosted coverage badge; wire codecov test analytics (#552) (77771b8)
- Upload lcov to codecov (informational) (#551) (9fafedd)
- Era-adaptive corpus capture — k6 knobs beside locust (#547) (6dd83a2)

### Changed

- RFC 0031 — split the comparative harness + dispatch class filter (#542) (9ec0111)

### Chore

- Update rust:1.97-bookworm docker digest to 77fac8b (#565) (ead1f27)
- Update gcr.io/oss-fuzz-base/base-builder-rust docker digest to a427e73 (#561) (3c05cb6)
- Update gcr.io/distroless/static-debian12 docker digest to 61b7cce (#560) (15140f4)
- Update gcr.io/distroless/cc-debian12 docker digest to 7ee09f3 (#559) (e7c4198)
- Update cargo (minor/patch) (#566) (5242ab2)
- Update github-actions (#562) (7236154)
- Update cargo (minor/patch) (#505) (891efe4)
- Hold rand at 0.8.x — rides the p256/rustcrypto 0.14 lift (#530) (445510a)
- Update gcr.io/oss-fuzz-base/base-builder-rust docker digest to 90232e3 (#503) (64dde51)
- Hold p256 at 0.13.x until jsonwebtoken moves to rustcrypto 0.14 (#525) (14a8f86)
- Hold reqwest at 0.12.x until datafusion 55 (rfc 0021 phase 2) (#524) (fcb9335)
- Update taiki-e/install-action digest to 43aecc8 (#500) (b314b02)
- Update gcr.io/oss-fuzz-base/base-builder-rust docker digest to 6a1d899 (#497) (3e22246)
- Pin dependencies (#496) (27940f9)
- Update oss-fuzz base-builder-rust digest (#463) (6f20eae)
- Refresh production image base layers (rust 1.97, distroless digests) (#462) (df37cec)
- Update jsonschema to 0.47.0 (#461) (575a8f7)
- Update cargo (in-range patch/minor) — bytes, rmcp (#460) (82894d8)
- Update github-actions (codeql-action, taiki-e/install-action) (#459) (ce85b4a)
- Update sigstore/cosign-installer to v4.1.2, pin cosign v2.5.2 (#216) (#457) (ac80275)
- Replace abandoned serde_yaml with serde_yaml_ng (#216) (#456) (098f0cd)

### Documentation

- §9.20 — baseline-class D1 capacity probes (single-tenant ceiling) (#568) (9bc6879)
- §9.19 — first D1/D2 soak record (#564) (f3918cf)
- Workload-fit positioning — when to choose Ourios, and when not (#557) (a313031)
- Add URLs to bestpractices reporting justifications (#554) (86d9f32)
- Rfc 0022 green -> validated on the §9.9 + §9.11 evidence (#545) (f0d5fea)
- §9.17 — L4 frequency-aggregation measurement record (#544) (20b3b9f)
- Refresh glossary hazard count + roadmap.md to 2026-07-15 (#537) (c30986a)
- RFC 0002 amendment — param(n), bucket(width), aggregation execution criteria (#531) (35c201b)
- Rfc 0033 green — corpus arm measured (#21) and asserting (#23), §9.16 (#529) (f0965d5)
- Rework both front doors — README and the book introduction (#527) (d79634b)
- Rfc 0033 §5.6 amendment — corpus ratio gate 1/10 → 1/2 per run #21, §9.15 record (#526) (544c1f0)
- Rfc 0032 green — all six §5 scenarios discharged (#523) (a925d70)
- RFC 0033 §3.2 amendment — compressed artifact encoding (run #20 answer) (#521) (ae5cebd)
- Rfc 0033 green→red — run #20: .6 corpus arm undischarged, §9.14 record (#520) (1fdbd27)
- Rfc 0032 specified — §5 criteria declared complete (#516) (74b8ca3)
- Rfc 0032 drafted — query-schema + cost-model resource for the mcp surface (#515) (7c949d1)
- Rfc 0033 green — all seven §5 scenarios discharged (#514) (ff67d03)
- Rfc 0033 drafted → specified (#507) (f653617)
- Rfc 0031 §3.6/§7 — precision follow-up to the freeze merge (#504) (ceb8817)
- RFC 0031 §7 — partial calibration freeze (M_L1/M_L3 @10 storage, F_L6 @3 latency) (#502) (fc31a8d)
- RFC 0031 §9.13 comparative entry draft (runs #8–#18) — maintainer-gated fold-in (#494) (b6a139b)
- RFC 0005 §3.6 amendment — blooms on trace-context ids, with measured evidence (#491) (30525c8)
- RFC 0033 drafted — cached template-map artifact (#484) (136b9d1)
- RFC 0031 — comparative evaluation vs Loki (specified) (#464) (a71cbe1)
- Accept RFC 0019 — storage-backend selection (#455) (3d889a1)
- Reconcile §9.12 with the resolved #444 decision (#452) (19886b9)
- §9.12 — otel-demo v8 capture gates (C1 PASS, C2 FAIL) + calibration manifest (#443) (a99a496)

### Fixed

- Silence unused_async on authenticate without oidc (#563) (50339e9)
- Accept proto3-JSON unset AnyValue on the OTLP/JSON paths (#550) (07c75c1)
- Rfc 0031 — query ingesters regardless of range age (l3 flicker diagnosed) (#490) (93bb4a3)
- RFC 0031 — salvage per-pair reports + L3 timeout diagnostics (#488) (6e91c0e)
- RFC 0031 — raise Loki's internal gRPC cap (single-line inflation) (#478) (6604b0c)
- RFC 0031 — 1.5 MB flush cap (Loki inflates OTLP internally) (#477) (e6a4352)
- RFC 0031 — byte-capped Loki push batching (run #2 finding) (#476) (045e40e)
- RFC 0031 — generalize the pair picker (v8 has no ERROR logs) (#475) (01069ad)

### Performance

- Late materialization — page-selective reads on the materialize scan (#486) (b608176)
- Rfc 0031 — elide the count scan when materialization is complete (#485) (e44e978)

### Tests

- RFC 0031 — backdated wide-range Loki interop arm (#541) (f9b6afe)
- RFC 0031 L4 — property tests for the margin comparator (#539) (9ca8feb)
- Red — L4 amendment stubs land (RFC0002.12–.16) (#532) (d8512b0)
- Red — all six §5 stubs land, status specified→red (#517) (a58f422)
- Red — all seven §5 stubs land, status specified→red (#508) (b49758e)
- RFC 0031 — pin promoted service.name column + pruning behavior (#483) (818395d)
- Red — §5 stubs land, status specified→red (#466) (2daf0bf)
- Real OTel Collector → Ourios over TLS + OIDC (#453) (8390da1)
- Rfc 0030 green (mTLS) — RFC0030.4 require-and-verify (#448) (eadc78a)

## [corpus/otel-demo-v8] - 2026-07-09

### Added

- Rfc 0030 green (config) — *_tls blocks, preflight, plaintext warning (#442) (8b7c5de)
- Static-musl variants on distroless/static + scratch (#263) (#439) (938e186)

### CI

- Dispatchable chart-publish workflow (#438) (3d42f06)

### Documentation

- Rfc 0030 specified — tls/mtls on the data-plane listeners (#440) (dc64aff)
- Show the topology diagram on artifact hub (#437) (0bf0805)
- Artifact hub badge (#436) (470ef7a)

### Tests

- Red — all nine §5 stubs land, status specified→red (#441) (425b96c)

## [0.2.1] - 2026-07-08

### Fixed

- Re-include the DSL grammar the mcp server embeds at compile time (#434) (06e16f6)

## [0.2.0] - 2026-07-08

### Added

- Honour the macos_full_fsync knob via rustix (#430) (2e12290)
- Rfc 0029 green — dex end-to-end acceptance and status flip (#426) (69400a8)
- Rfc 0029 green (query/mcp binding) — one resolver on every surface (#425) (a2602fc)
- Rfc 0029 green (ingest binding) — async auth layer on both listeners (#424) (bb02d91)
- Rfc 0029 green (verifier) — local OIDC JWT verification (#423) (3c8a715)
- Rfc 0029 green (config) — auth.oidc section, coexistence rules, oidc-only enforced (#422) (095e891)
- Rfc 0027 green resource — the dsl grammar (RFC0027.6) (#415) (a0fca77)
- Rfc 0027 green tools — query_logs, list_templates, template_drift (#414) (a8ba334)
- Rfc 0027 green transport — /mcp behind querier.mcp.enabled (#413) (94743ff)
- Rfc 0026 green d — rejection telemetry + denial audit (#409) (e0cb265)
- Rfc 0026 green c — query-path authn + tenant gate (#408) (529a575)
- Rfc 0026 green b2 — ingest authn + tenant binding (#398) (7cabed1)
- Rfc 0026 green a — the token store (RFC0026.1) (#390) (a0cb122)
- Rfc 0025 green c — permanent-error quarantine; RFC green (#386) (a1257cf)
- Rfc 0025 green b — rendering distinguishes absent from empty (#381) (1971712)
- **BREAKING** Rfc 0025 green a — body_kind ordinal 2 for absent bodies (#380) (eb28c7c)
- Green — P4 query oracle + adversarial umbrella (#369) (b255df0)
- Green — P1/P2/P3 pipeline properties + canonical decode fix (#363) (563f540)
- Rfc 0024 green — --calibrate pass discharges §5.1/.2 (#361) (6b696e4)
- Rfc 0024 green slice — the ourios-testgen generator crate (#360) (c199701)
- Rfc 0023 green pt2 — parse-failure reason telemetry (RFC0023.6) (#355) (19e0886)
- Rfc 0023 green pt1 — bounded template memory (RFC0023.1–.5) (#354) (d171457)
- Rfc 0022 green pt4 — storage.promoted_attributes config plumbing, status → green (#348) (6e3301b)
- Rfc 0022 green pt2 — promoted predicate compile (RFC0022.3/.4/.6) (#346) (d4eb729)
- Rfc 0022 green pt1 — promoted attribute columns in the writer (#345) (30d0daf)
- One RFC 0005 decoder — delete the RFC 0017 duplicate (rfc0021.4) (#340) (69117e9)
- **BREAKING** Upgrade to datafusion 54 / arrow 58 — one arrow across the workspace (#339) (6ee5e2f)
- Strict registry-backed log events + weaver live-check CI gate (#335) (552d350)

### Build

- Line-tables-only debuginfo for the dev profile (#373) (1a4db29)

### CI

- Attach oci annotations to the multi-arch manifest list (#428) (20fea99)
- Publish the helm chart as an oci artifact on ghcr (#364) (#431) (f6de5b4)
- Pin the cargo-dist installer downloads by hash (#366) (#429) (c8dbf94)
- Rfc 0028 slice 5 — nextest runs the workspace suite (#406) (c1c5ec7)
- Verify the cargo-cyclonedx installer by pinned sha256 (#370) (acc7c61)
- Weekly deep run at elevated proptest cases (#371) (9feef6f)
- Migrate workflow actions off the deprecated node20 runtime (#305) (#342) (de52ce8)

### Changed

- **BREAKING** Rfc 0028 slice 3 — miner tunables extract to ourios-config (#405) (adb1cbd)
- Rfc 0026 green b1 — token store moves to ourios-core (#395) (a000daa)

### Chore

- Artifact hub metadata + appVersion for the first chart publish (#432) (4ef45a2)
- Update dependency ubuntu to v24 (#392) (fcd5a70)
- Update rust:1.96-bookworm docker digest to a339861 (#388) (a054129)
- Hold the arrow family at 58.x until DataFusion 55 (#396) (7ce71b4)
- Update rust crate jsonschema to v0.46.10 (#391) (f86aae1)
- Update github-actions (#387) (b7a6ee3)
- Don't digest-pin the chart's default image tag (#389) (0a466ed)
- Update gcr.io/oss-fuzz-base/base-builder-rust docker digest to 0ca3a7a (#385) (6f55a03)

### Documentation

- Getting-started section for the deployment types (#427) (eeedd13)
- Rfc 0029 specified — §5 acceptance criteria (#420) (47fa28f)
- Rfc 0029 — oidc bearer layer (issuer-agnostic, dex-validated), drafted (#419) (877f0df)
- Rfc 0026 + 0027 accepted — maintainer sign-off 2026-07-07 (#418) (6fc8859)
- Rfc 0026 + 0027 validated — served-binary + independent-client run (#417) (87d6eb2)
- Rfc 0027 green — all seven scenarios discharged (#416) (4c62d55)
- Rfc 0027 §5/§6 — mcp query surface specified (#411) (c79b474)
- Rfc 0026 green — all seven scenarios discharged (#410) (df7d864)
- Rfc 0028 green — all five scenarios discharged (#407) (ea798a2)
- Rfc 0028 §5/§6 — build-feedback program specified (#397) (4e0966b)
- Rfc 0028 — build-feedback program (drafted) (#383) (d22c0cc)
- §5/§6 land, status drafted → specified (#376) (8449dcc)
- §5/§6 land, status drafted -> specified (#377) (acd8272)
- Rfc 0026 authn/tenant binding + rfc 0027 mcp query surface (#374) (083ea96)
- Rfc 0025 — absent-body representation (rfc 0005 amendment) (#372) (97258de)
- Top up bestpractices.json — the ten criteria the 93% run surfaced (#368) (90ddae2)
- Maturity artifacts — maintainers, adopters scaffold, bestpractices.json (#367) (b4ceb4a)
- Rfc 0024 — otlp-envelope property testing (rfc 0006 amendment) (#358) (6870d29)
- §9.11 — authoritative 16 GiB B1/B2 + RFC0023.7 pass; rfc 0023 red→green (#356) (c7ea8c6)
- Rfc 0023 — bounded template memory (rfc 0001 amendment) (#352) (77851ed)
- §9.9 — indicative ci-runner B1/B2 rerun post-RFC 0022 (#349) (d8a9ede)
- Rfc 0022 — queryable attribute columns (rfc 0005 amendment) (#343) (2f22c96)
- DataFusion / Arrow upgrade, phased behind upstream (RFC 0021, drafted) (#337) (f769d5d)

### Fixed

- Busiest-template picker skips NO_TEMPLATE (#357) (167ecc5)
- Skip template-snapshot capture in the query-store builds (#351) (6e05cb8)

### Tests

- Red — all seven §5 stubs land, status specified→red (#421) (7cb852e)
- Red — all seven §5 stubs land, status specified→red (#412) (48bc4d8)
- Rfc 0028 slice 2e — 3 test binaries fold into one harness (#404) (08912b7)
- Rfc 0028 slice 2d — 8 test binaries fold into one harness (#403) (2618c3c)
- Rfc 0028 slice 2c — 11 test binaries fold into one harness (#402) (928dbc1)
- Rfc 0028 slice 2b — 17 test binaries fold into one harness (#401) (cc3ce1b)
- Rfc 0028 slice 2a — 19 test binaries fold into one harness (#400) (36f28ff)
- Rfc 0028 slice 1 — 27 test binaries fold into one harness (#399) (24c2c7c)
- Red — all five §5 stubs land, status specified -> red (#379) (180132d)
- Red — all seven §5 stubs land, status specified -> red (#378) (4e676d4)
- Calibration manifest for the otel-demo-v7 release (#375) (c8005ae)
- Red — all seven §5 stubs land, status drafted→red (#359) (8a01c6a)
- Red — all seven §5 stubs land, status drafted→red (#353) (2eb892d)
- Streaming corpus store builds + opt-in log4j severity (#350) (2655bac)
- Rfc 0022 green pt3 — pruning + promoted-set drift (RFC0022.5/.7) (#347) (3f08632)
- Red — all seven §5 stubs land, status specified→red (#344) (df879d3)
- Rfc 0021 phase 1 green — live lockfile gate + discharged markers (#341) (e47b525)
- Rfc 0021 red — §5 stubs + the pre-upgrade parquet fixture (#338) (b543309)

## [corpus/otel-demo-v7] - 2026-07-02

### Added

- Dogfood our own logs over OTLP (tracing → OTel Logs signal) (#334) (f4da8e5)

### CI

- Fix release publishing (GH_REPO) + publish-only recovery path (#336) (c8e2ea2)

### Documentation

- Finish #177's doc sweep — Ourios-canonical, not OTLP-canonical (#333) (e15a0e8)

## [0.1.1] - 2026-07-02

### Added

- Configure via a mounted RFC 0020 config file (--config) (#328) (6bd76d6)
- Rfc0020 green pt4 — credential-literal enforcement + §3.5 hygiene (#327) (f822b5e)
- Rfc0020 green pt3 — --config CLI wiring + file→ServerConfig map (#326) (034b47f)
- Rfc0020 green pt2 — YAML schema + scalar-value substitution walk (#325) (c527361)
- Rfc0020 green pt1 — env-substitution resolver (#322) (d0d6b22)

### CI

- Attach .intoto.jsonl provenance bundles as release assets (Scorecard Signed-Releases) (#332) (ec134e7)
- Attest global artifacts (SBOMs, installer, source) + doc release verification (#329) (cedef37)
- Ingest via telemetrygen (gRPC) + assert body reconstruction in the deploy smoke test (#319) (697b771)
- End-to-end deploy smoke test on an ephemeral kind cluster (#318) (05d42b0)
- Build arm64 on a native runner instead of QEMU (#317) (3b815e4)

### Chore

- Accept quick-xml DoS advisories RUSTSEC-2026-0194/0195 (no upstream path) (#330) (44cc4f7)

### Documentation

- Configuration file — YAML + env substitution (RFC 0020, specified) (#320) (0bf67ff)

### Tests

- Rfc0020 red — config-file §5 stubs + status (#321) (88fbe1f)

## [0.1.0] - 2026-06-29

### Added

- Generate a SCHEMA_URL constant from the registry manifest (#316) (47b8431)
- S3-native Helm chart for the RFC 0019 split topology (#304) (5a5b1ed)
- Explicit OURIOS_S3_* S3 credentials (RFC 0019 §9 / RFC0019.8) (#307) (69a5e18)
- OURIOS_COMPACTION_ENABLED to disable a pod's compaction sweep (#303) (e15d264)
- Migrate the compaction audit sink onto the Store seam (RFC 0019 slice 2d) (#299) (5cd5b21)
- Run the receiver data write path on the resolved Store (RFC 0019 slice 2c) (#298) (63cb4ad)
- Migrate the compactor onto the Store seam (RFC 0019 slice 2b) (#297) (b1a22be)
- Store-backed Writer + Reader ctors for the RFC 0019 compactor (#294) (46a9f57)
- Store sized listing + blocking delete for the RFC 0019 compactor migration (#293) (71e440e)
- Migrate the audit + scan read paths onto Store (rfc0019 2a) (#292) (cad889a)
- Add AuditReader::open_bytes for the RFC 0019 Store read path (#291) (428f825)
- Store listing wrapper for the RFC 0019 querier/compactor migration (#290) (dcf8917)
- Rfc0019 green .1/.6/.7 — StoreConfig seam + backend selection (#289) (dade7a9)
- Querier OTel metrics + flip RFC 0016 green (rfc0016 .6) (#285) (699bfb4)
- Env-gate the querier role + compose with the receiver (rfc0016 green .5/.7) (#284) (c788f9f)
- Query endpoint handler — POST /v1/query (.1-.4) (#283) (70b1f7a)
- LogRow + QueryResult.records (RFC 0017 green) (#277) (5d548b0)
- Query-time body rendering (LogBody three zones) (#275) (93a66bf)
- Derive_template_registry from the audit stream (#274) (aa90d4c)
- Audit leaf creation via TemplateChange::Created (#273) (443066c)
- Rfc0018.5 green — non-finite doubles round-trip; RFC 0018 green (#271) (dcff67e)
- Rfc0018.3 green — transient ingest failures map to retryable codes (#270) (5504e21)
- Rfc0018.6 green — preserve out-of-range severity + error.type (#269) (5065763)
- Rfc0018.1/.2 green — persist scope attributes + schema_url (#268) (8fda0ab)
- Rfc0018.4 green — event_name DSL filter (#267) (f548858)
- Ingest + sink metrics for perf observability (#247) (0d2074f)
- Wire the RFC 0014 data write path live (rfc0013 green .6) (#245) (ad7b102)
- Rfc0014 green pt1 — ParquetRecordSink + flush policy (#243) (5661984)
- Manifest compare-and-swap publish (rfc0013 green .3/.4) (#239) (24db263)
- Migrate the writer to buffer-and-put on the Store seam (rfc0013 green) (#236) (806595a)
- Implement the S3 / S3-compatible Store backend (rfc0013 green) (#235) (3d3454c)
- Route the manifest through the object-store seam (rfc0013 green) (#234) (30a6efc)
- Route reader through the object-store seam (rfc0013 green) (#233) (651aa24)
- Rfc0013 green — buffer-and-put encode/decode + store round-trip (#232) (650d193)
- Rfc0013 green — store local I/O surface (put/get/delete) (#231) (f7a2e13)
- Rfc0013 red — store module skeleton + §5 ignored stubs (#230) (eb0caea)
- Reclaim crash orphans + RFC0009.4 crash-recovery test (#206) (9c418b6)
- Batched-fsync group commit (#191) (38b40ce)
- WAL segment rotation + rotation-triggered snapshot cadence (#188) (8c2c28c)
- Snapshot restore v2 — per-tenant restore + startup recovery driver (#187) (6649d34)
- WAL checkpoint — durable sidecar, retain-floor housekeeping, offset sink (#186) (e9322ed)
- Alias-index write path v1 — events persisted, map derived (#184) (fa646a3)
- Effective-timestamp column + windowing fallback (RFC0005.13) (#179) (6f7d845)

### Build

- Single-source the workspace version + add release recipes (git-cliff + cargo-dist lockstep) (#315) (9a32bfa)

### CI

- Run the RFC 0019 localstack S3 tests in the s3-integration job (#300) (1e6c251)
- Adopt cargo-dist for signed binary releases (#262) (69b1207)
- Add signed multi-arch container image (#260) (ce42e1d)
- Run fuzz.yml on schedule + dispatch only, not per-PR (#257) (4b12667)
- Give Scorecard a PAT to read branch-protection rules (#253) (ccedb59)
- Add CodeQL static analysis (SAST) for Rust (#252) (0f77836)
- Scope workflow write tokens to the job, not top level (#251) (57aba55)
- Add on-demand ingest_write_path + recovery bench workflow (#249) (d3f2cae)
- Free runner disk space before heavy cargo builds (#244) (39c51c2)
- Deploy docs to Pages on push to main (§6.7 gate cleared) (#227) (5d94cb1)
- Restrict github-action automerge to digest-only (#218 follow-up) (#222) (9861806)
- Consolidate renovate config to .github/, automerge low-risk dep updates (#218) (06f46f1)
- Add cargo-deny supply-chain gate (advisories, licenses, bans, sources) (#213) (2165f6c)
- Run coverage on main pushes only, not PRs (dedup the test run) (#212) (8893dd6)
- Sha-pin actions, least-privilege tokens, add renovate (openssf scorecard) (#211) (efe043a)
- Deny rustdoc breakage — workspace lints + a cargo doc job (#175) (073c7ee)

### Chore

- Hold object_store at 0.13.x until DataFusion upgrades (#313) (c6617c1)
- Update actions/checkout action to v7 (#311) (713487c)
- Update gcr.io/oss-fuzz-base/base-builder-rust docker digest to 253eff2 (#308) (a564b90)
- Update github-actions (#309) (c93d18d)
- Osv-scanner ignores for thrift + quinn-proto advisories (#296) (f2c29be)
- Pin dependencies (#278) (fb313c2)
- Update rust docker tag to v1.96 (#281) (4592882)
- Update cargo (minor/patch) (#280) (f8af2e4)
- Update github-actions to 9e1e580 (#279) (6639f9e)
- Update rust crate prost to v0.14.4 (#220) (7f65f2e)
- Update rust crate opentelemetry_sdk to v0.32.1 (#219) (0fee5d1)
- Update rust crate chrono to v0.4.45 (#215) (f2587ea)
- Update ossf/scorecard-action action to v2.4.3 (#214) (0da8b6d)
- Gitignore .env (local secrets file) (#196) (3f3c987)
- Drop stale leaf retained-body-count comment (#195) (c54530d)

### Documentation

- Explicit OURIOS_S3_* credential env keys (RFC 0019 §3.4 + RFC0019.8) (#306) (113cbb1)
- Storage-backend selection RFC 0019 (specified) (#287) (f0c808f)
- OTLP log-spec compliance amendments (#265) (dc366f6)
- Add read-time template registry & query rendering (specified) (#264) (8d8cade)
- Add query-serving-endpoint RFC (specified) (#259) (51d55f6)
- Add fuzzing-harness RFC (specified) (#254) (dbed6fa)
- §9.8 — baseline ingest/recovery + real-corpus A1/C1/C2 + B1/B2 (#250) (f369fb9)
- Advance ingest write path to specified (#241) (a261dc3)
- Ingest write path — record sink & flush policy (drafted) (#240) (d83d518)
- Advance object-storage backend to specified (#229) (ef22095)
- Object-storage backend (S3-compatible) — drafted (#228) (14f4b03)
- Flip compaction to validated (D2/D3/B2-post on baseline, §9.7) (#226) (8b857c5)
- Flip audit-stream/drift queries to green (§5 RFC0010.1-.8 pass) (#221) (88b3432)
- Flip compaction to green (§5 RFC0009.1–.6 pass) (#210) (9fd7521)
- Refresh stale CLAUDE.md status banner (informal meta: waiver) (#205) (bd05edf)
- Align §1 thesis sentence + README with the pillar-#2 reframe (#204) (0d9aa4f)
- Enact RFC 0012 — §2 pillar-#2 is a logical 50–200×, not byte-level (#203) (7f9e57d)
- Draft meta-RFC for CLAUDE.md §2 pillar-#2 wording (#202) (42cad6e)
- Refresh §3 current-state to the shipped stack (#201) (96c4d83)
- Reframe A1 as a diagnostic, not a gating thesis-gate (RFC 0011) (#200) (6a871db)
- Accept RFC 0001, RFC 0008, and RFC 0011 (#199) (b2e4b6e)
- Flip status to validated (#198) (46da33d)
- Record authoritative baseline C1/C2 on HDFS_v1 (§9.6) (#194) (ae217f6)
- RFC 0011 — re-scope A1 (template-mining compression is logical, not byte-level) (#193) (9a57ace)
- Advance RFC 0001 and RFC 0008 to green on the maturity ladder (#192) (8569fab)
- Defer the RFC0008.5 corruption audit event to system-scoped audit (#189) (76e9e03)
- Specify snapshot restore v2 — offset sink, retain floor, recovery driver (#185) (f75d001)
- Alias events in the audit stream + v1 reader-side map derivation (#183) (fa66a6a)
- Reflect the implemented state + live status badges (#181) (42a7aa0)
- Authoritative baseline results; rfc 0007 validated (#182) (8171422)
- Record the ~1GB A1/C1/C2 + first B1/B2 readings; RFC 0007 stays green (#180) (656986a)
- Effective-timestamp fallback for windowing + the stored column (#178) (407c12a)

### Fixed

- Persist miner template audit events from the receiver (#302) (#312) (18eccac)
- Exact f64 canonical round-trip via serde_json float_roundtrip (#176) (e29231b)

### Tests

- RFC 0019 localstack S3 e2e (.2–.5) + flip RFC 0019 green (#301) (75f0139)
- Rfc0019 red — storage-backend selection §5 stubs + status (#288) (f1ff7c0)
- Red — §5 query-endpoint stubs + status (#282) (2fb278b)
- Red — §5 registry/rendering stubs + status (#272) (c231efa)
- Rfc0018 red — §5 OTLP-compliance stubs + status (#266) (39e769a)
- Add ClusterFuzzLite continuous fuzzing + coverage (rfc0015 phase 2) (#258) (a2d1fb7)
- Add smoke-fuzz CI workflow + seed corpora (rfc0015 green) (#256) (250a6fe)
- Add cargo-fuzz targets + wal fuzzing feature (rfc0015 red) (#255) (df629cf)
- Ingest write-path criterion benches (#248) (244eece)
- Rfc0014 green .5 — crash no-loss (real SIGKILL) (#246) (0d4e2c3)
- Rfc0014 red — §5 ingest-write-path stubs + status (#242) (e92c7af)
- Green RFC0013.1 + .7 via a testcontainers + LocalStack CI lane (#238) (b33bff1)
- Green RFC0013.2 + .5 local acceptance scenarios (#237) (3bc1223)
- B2-post-compaction query-latency comparison (RFC0009.7) (#225) (4d52288)
- Band-scale baseline mode for d2/d3 compaction bench (#224) (a4bcbb3)
- D2/d3 compaction throughput + small-file-collapse bench (RFC0009.7) (#223) (c968bd6)
- Rfc0005.6 row-group sizing test, flip rfc 0005 to green (#217) (768f256)
- Mis-partitioned input aborts, not merged (RFC0009.5) (#209) (ee48ef1)
- Union-schema merge across an amendment (RFC0009.6) (#208) (e261c36)
- RFC0009.1 small-file count collapses under compaction (#207) (ecea311)
- Make RFC0008.8 latency test deterministic via virtual clock (#197) (cf3b58b)
- Flip RFC0008.1/.3/.4/.5/.9 acceptance arms (#190) (be77456)

## [corpus/otel-demo-v6] - 2026-06-10

### Added

- Template-tree snapshot format + v1 full-replay recovery (RFC0001 §3.5) (#170) (03a55cb)
- Structured-body short-circuit + canonical render (RFC0001.9) (#167) (04cd107)
- Drift query over the audit stream (RFC 0010, H5.3) (#165) (3e588df)
- Reader render emits body+RetainedVerbatim for lossy rows (H7.3) (#163) (523adaf)
- Expose the ourios.miner.* §6.8 telemetry via the weaver registry (#160) (aa4b903)
- Expand resolves_to via the alias map (RFC0002.9) (#154) (d8a6651)
- Add the operator-driven alias map + audited alias events (RFC0001.12-.16) (#153) (2e4704c)
- Yaml-embeddable queries + structured-surface JSON schema (RFC0002.10/.11) (#149) (f225be9)
- Compile the DSL IR to the execution layer (RFC0002.1/.3/.4/.5/.6) (#146) (1d40c8a)
- Parse the DSL string + structured surface to one IR (RFC0002.2/.7/.8) (#145) (396af2c)
- Serve the OTLP receiver role — RFC0003.16 green (#141) (614adf6)
- Add the OTLP/gRPC LogsService listener (RFC0003.11/.15) (#136) (11a8eea)
- Add the OTLP/HTTP listener (RFC0003.13/.14 + .11 HTTP arms) (#135) (3c0dcda)
- Add the WAL-before-ack ingest pipeline (RFC0003.1/.12) (#134) (19e05d0)
- Tenant derivation + fan-out (RFC0003.3/.4) (#133) (8a890fc)
- Materialize LogRecord into OtlpLogRecord (RFC0003.7–.10) (#132) (5ae5435)
- Add OTLP/JSON decode + RFC0003.6 (encoding-rule conformance) (#131) (10c2fc5)
- Add OTLP wire-decode layer + RFC0003.5 (protobuf equivalence) (#129) (7667e36)
- Implement sync + replay — crash recovery §6.3/§6.6 (#123) (077332b)
- Add the ourios.compaction.backlog observable gauge (RFC 0009 §3.6) (#122) (ad9c18b)
- Time-windowed b2/otel-demo arm measuring partition pruning (#118) (d0f50f5)
- Partition-level time pruning (RFC 0007) (#117) (8a3788d)
- Add the B1 predicate-pushdown bench + zstd|grep reference (#115) (166662b)
- Add severity_text predicate for the B1 level filter (#114) (69befe2)
- Add compaction io + H4 file-size telemetry (RFC 0009 §3.6) (#113) (f6e6ace)
- Add the ourios-server binary running the compaction role (#112) (12fc507)
- Add a durable audit sink over the §3.7 Parquet stream (#111) (a4e52b9)
- Emit a compaction audit event per committed sweep (#110) (f77767c)
- Add AuditPayload enum + route compaction events to the audit stream (#109) (909428c)
- Instrument the compaction sweep with RFC 0009 §3.6 metrics (#106) (e1e7ca9)
- Generate ourios-semconv constants crate via weaver forge (#105) (8f9e16c)
- Scaffold ourios-telemetry crate (OTLP MeterProvider bootstrap) (#104) (9470cca)
- Scaffold crate + background compaction runner (RFC 0009 §3.2) (#101) (ce38e55)
- Add compaction candidate planner (RFC 0009 §3.3) (#100) (2c1b2f9)
- Add sealed-partition compaction module (RFC 0009) (#97) (ea1f98f)
- Resolve partition files through the RFC 0009 manifest (#96) (cb0810a)
- Add B2 query-latency criterion bench (synthetic + otel-demo) (#92) (c94be49)
- Prove B2 — template-exact work tracks result, not corpus (slice 3) (#89) (d0bceb9)
- Extract row-group pruning stats from DataFusion — B1 live (slice 2) (#88) (532a98d)
- Execute minimal tenant/time/template queries via DataFusion (slice 1) (#87) (b287e0c)
- Scaffold ourios-querier crate — RFC 0007 red gate (#86) (767d01a)
- Make Parquet ZSTD level configurable for the A1 codec sweep (#84) (0bfc63c)

### CI

- Expose otel-demo failure feature flags in the corpus capture (#172) (7dbaa07)
- Add query-bench workflow for the B1/B2 query-results artifact (#119) (0e4fefd)
- Add informational cargo-llvm-cov coverage job (#98) (14af485)

### Documentation

- Reconcile §6.4 — canonicalisation happens at ingest (#174) (dc9d809)
- Pin the §6.9 snapshot format + recovery as local-disk, per-tenant (#169) (6337eeb)
- Pin the structured-body canonical encoding as an ourios-local rule (#166) (a6e8561)
- Audit-stream queries + template drift surface (#164) (d733287)
- Define the §6.6 reader-render contract + lossy warning marker (#162) (202f088)
- Advance RFC 0002 + RFC 0007 to green (#155) (af1c3cd)
- Specify the operator-driven alias-index write path (#151) (d9f0d66)
- Specify the query DSL — Branch B, surface β (#143) (acd6f1f)
- Advance status specified → green (RFC0003.16 landed) (#142) (2d2af25)
- Resolve §9 (process model + partial success) + add RFC0003.16 (#139) (31c6455)
- Advance status specified → green (#138) (9bc4e2a)
- Specify §5 G/W/T scenarios + OTel enrichments — drafted → specified (#127) (b695560)
- Amend §3.7/§3.8 for compaction audit events (#107) (6caee22)
- Realign §6.8 telemetry export to OTel SDK + OTLP (#103) (ce28ee5)
- Define compaction telemetry per OTel semantic conventions (#102) (b37d0a0)
- Enable mdbook-mermaid so RFC diagrams render (#99) (2c21307)
- Advance to specified with the manifest approach (#95) (83939c0)
- Draft background compaction RFC (#93) (2781d4c)
- Draft querier — DataFusion execution frontend for the logs DSL (#83) (c7f0a57)
- Record first A1/C1/C2 results — scale + codec sweep (§9.1) (#85) (5304482)

### Fixed

- Raise the capture job ceiling so long captures survive (#173) (31a5181)
- Query the corpus tenant in b2/otel-demo (was always empty) (#116) (6a70866)

### Tests

- Land RFC0001.10 (ts preserved) + §3.7.3 (per-ResourceLogs tenant), relocated (#168) (484f8f3)
- Prove template_id spans versions + ignores aliases (RFC0001.5/.6) (#161) (7dd1e74)
- Flip H5.2 — slot type-set growth bumps version + emits TemplateTypeExpanded (#158) (5b336ba)
- Flip H1.2 — lossy-zone match keeps a fresh leaf + retains body (#157) (c33bb33)
- Lock (severity_number, scope_name) template keying (H1.4/H1.5/RFC0001.11) (#156) (834c8c3)
- Add the RFC 0001 alias write-path red gate (RFC0001.12-.16) (#152) (72a2982)
- Add the RFC 0002 red gate — DSL acceptance stubs (#144) (3405048)
- Add the RFC0003.16 red-gate stub + scenario prose polish (#140) (3aa4b5a)
- Land rfc0003_2 crash-before-ack — RFC 0003 fully green (#137) (77d809f)
- Red-gate the OTLP receiver — rfc0003.1–.15 acceptance stubs (#128) (d49bf0a)
- Land rfc0008_2 crash-recovery via real SIGKILL harness (#126) (fa6b302)
- Compaction atomic-publish / no-torn-read test (RFC0009.3) (#121) (86ab128)
- Proptest the compaction row-conservation invariant (RFC0009.2) (#120) (5ade02a)
- Cover forward-compatible reads across heterogeneous schemas (RFC0007.4) (#91) (3839118)
- Cover no-DataFusion/arrow/SQL-leakage boundary (RFC0007.3) (#90) (1196bb0)

### Bench

- Wire B1/B2 real-corpus arms + bench-time corpus staging (#171) (da4da0e)

## [corpus/otel-demo-v4] - 2026-05-31

### Added

- Freeze demo corpus as a release asset + wire bench capture: otel-demo (PR-N4) (#79) (85f311a)

## [corpus/otel-demo-v1] - 2026-05-31

### Added

- Make load-generator user count tunable to scale corpus (PR-N3.5) (#78) (1e3a4d4)
- Add OTel Demo corpus capture workflow (PR-N3) (#73) (02f33ad)
- Wire telemetrygen OTLP capture into bench.yml (PR-N1) (#70) (4cd216a)
- Add Wal::append + frame format §6.2.2 (PR-M5) (#69) (dd0c0c6)
- Add Wal::open + segment header layout (PR-M4) (#68) (2215aec)
- Ourios-wal crate red-gate stubs for RFC0008.1-.9 (PR-M2) (#66) (0c2c770)
- Restore OTLP envelope 1:1 mapping + kvlist fixture (PR-L2) (#63) (3dabe02)
- Implement RFC 0005 §3.3 canonical-JSON encoding (PR-L1) (#62) (618d8c2)
- Add OTLP/JSON corpus loader — RFC 0003 §6.5 MVP path (PR-K2) (#58) (0118322)
- Close RFC 0006 — reproducibility + mismatch diagnostics, flip to green (PR-J4) (#56) (9c3e66d)
- §9 benchmarks.md results appender (PR-J3) (#55) (a9cc4ec)
- Land C2 template-count convergence (PR-J2) (#54) (2877230)
- Clap CLI + JSON results-file writer (PR-J1) (#53) (b75b81c)
- Land A1 compression-ratio measurement (PR-I2) (#52) (d1f4045)
- Land C1 reconstruction-rate measurement (PR-I1) (#51) (a6e89a4)
- Red-gate test stubs and rfc 0006 status bump (PR-H2) (#50) (f4bb48f)
- Scaffold ourios-bench crate (PR-H1 of RFC 0006) (#49) (6d56236)
- Audit-stream writer/reader (PR-G) (#46) (55d8730)
- Reader with §3.9 contract + RFC0005.1/2/3/4/9/11 (PR-F) (#45) (59e127a)
- Writer + partition derivation + RecordBatch builder (PR-E2) (#44) (4206d59)
- Extend MinedRecord with the OTLP-envelope fields (PR-E1) (#43) (8662a43)
- Scaffold ourios-parquet with RFC 0005 schemas (PR-D) (#42) (dc973c5)
- H7.1 reconstruction property test against corpus (PR-C) (#40) (d100e99)
- §6.5 OVERFLOW marker + per-parameter byte-limit enforcement (closes Phase 1) (#39) (61b2405)
- Per-tenant config overrides + prefix_depth tunable (RFC 0004 impl) (#38) (0e192a0)
- Reconstruct() + tokenizer NUL + params alignment (PR-B-2 of §6.6) (#35) (aa94f9b)
- Mask-emit positions enter leaf as Wildcard from creation (PR-B-1) (#34) (529d607)
- Typed wildcard slots + TemplateTypeExpanded emitter (PR-B-0) (#33) (2d3837f)
- Mined-record schema + emission scaffolding (PR-A of §6.6) (#31) (548195d)
- Three-zone confidence + parse-failure floor (#30) (2359d9c)
- Widen step + audit emission + best-candidate selection (#29) (6fec688)
- Consume OtlpLogRecord with body_kind fork (#28) (d5a7083)
- Introduce OtlpLogRecord with AnyValue body (#27) (af9ab18)
- Route MinerCluster through Drain tree + sim_seq exact-match (#19) (1cd0d45)
- Add Drain prefix tree skeleton (RFC 0001 §6.2 step 3) (#17) (0c16e33)
- Add sim_seq + confidence_ratio (RFC 0001 §3.2, §6.3) (#15) (d1a7077)
- Add MinerCluster — flips §3.7.1, §3.7.2 (#13) (1dc9ec1)
- Add MinerConfig — flips §3.1.1, §3.2.1, §3.2.2 (#11) (e0aa71b)
- Add mask() — RFC 0001 §6.2 step 2 (#10) (af46c91)
- Implement tokenize() — RFC0001.3 (first red → green step) (#8) (c166a60)

### CI

- Add corpus_dir input to bench.yml (PR-K3) (#60) (3cc985b)
- Add on-demand thesis-gate bench workflow (PR-K1) (#57) (738e6ff)
- Add PR title lint, advisory commitlint, and release workflow (00fd7e0)
- Fix workflow parse failure; land workspace stub + toolchain pin (979ebe7)
- Pre-crate Layer 0 + Layer 3 — CI workflow and justfile (349bd0e)

### Changed

- Split MaskTag from ParamType for exhaustiveness (#12) (17953a3)

### Chore

- Add ourios-miner crate with §5 acceptance stubs (specified → red) (#7) (69eed13)
- Add ourios-core skeleton (#3) (1bd0e59)
- Add git-cliff, commitlint, committed, and renovate config (7275482)
- Add GitHub community config (8d439af)

### Documentation

- Specified → red (PR-M3) (#67) (03c7d2b)
- Specified — formal §5 acceptance criteria (PR-M1.1) (#65) (33482bc)
- Draft write-ahead log RFC (drafted, PR-M1) (#64) (afc4af5)
- Add RFC 0006 — bench harness (A1 / C1 / C2) (#48) (7c808c3)
- Refresh §3 — Phase 2 closed, Phase 3 unblocked (#47) (8390295)
- Add RFC 0005 — Parquet storage (schema, writer, audit stream) (#41) (be1bff5)
- Add RFC 0004 — configuration policy (tunables vs invariants) (#37) (2a027cd)
- Capture Perses dashboard integration as deferred capability (#36) (5215a32)
- Extend §5 with OTLP-receiver acceptance criteria (#25) (5a5c389)
- Add §10 open question on first-class OTel query dimensions (#26) (22702ff)
- Draft RFC 0003 — OTLP receiver (#24) (7b26ae7)
- Rewrite §6.2 algorithm for the OTLP body.kind fork (#23) (7c1be70)
- Split OTLP scope — record shape in MVP, wire endpoints post-MVP (#22) (f690ded)
- Amend §6.1 to align record schema with OTLP LogRecord (#21) (3507918)
- OTLP log-format gap analysis (Half 1 investigation) (#20) (8f3ab3e)
- Add §6.2 "Tests are specifications" bullet (#18) (bf3de01)
- Add docs/roadmap.md — current state to MVP path (#16) (0ac445b)
- Clarify template_id allocator scope (PR #13 driver) (#14) (a443b52)
- Patch §§6.4, 6.7, 6.9, 9 with event-storming deltas (#9) (42be82a)
- Add §5 acceptance criteria (drafted → specified) (#6) (eea1ae2)
- Fill in drafted-bar content for the template miner (#5) (dde4864)
- Apply RFC maturity-model amendments (#4) (9161557)
- Add verification process spec (#1) (f054047)
- Add community health files (02842c3)
- Add forward-looking H8 (dedup under drift) and agent-friendly DSL question (8e5dfc9)
- Write hazards.md and glossary.md (resolve broken cross-refs) (ff459af)
- Align CLAUDE.md with present-day repo (meta-RFC informally waived) (b50067d)
- Stop cropping the "confidence" label in fig-3 (84eed86)
- Give lecture SVGs a light canvas so they read on dark themes (1558451)
- Replace ASCII figures with SVG; document diagram conventions (46b4b1a)
- Render math notation via MathJax (d267169)
- Align mdBook config with 0.5.x; bump CI pin to match local (55e0c5d)

### Fixed

- Filter collector + load-generator noise from demo corpus (PR-N3.4) (#77) (7b2234e)
- Bypass frontend-proxy, drive frontend directly (PR-N3.3) (#76) (22ac807)
- Gate demo capture on frontend-proxy readiness + slice steady state (PR-N3.2) (#75) (e3fa3ad)
- Bind-mount a writable capture dir for the demo collector (PR-N3.1) (#74) (b08b177)
- Move collector config out of workflows/ + unthrottle telemetrygen (PR-N1.2) (#72) (d132e25)
- Bump pinned OTel toolchain to v0.153.0 (PR-N1.1) (#71) (4ba907c)
- Handle Body::Structured records in harness + C1 (PR-K4) (#61) (f48c16d)
- Tighten OTLP-loader consistency surface (PR-K2.1) (#59) (106ae5c)


