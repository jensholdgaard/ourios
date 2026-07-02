# Changelog

All notable changes to this project will be documented in this file.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) · SemVer.
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


