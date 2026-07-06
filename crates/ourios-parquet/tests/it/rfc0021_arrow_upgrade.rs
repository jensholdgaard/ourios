//! RFC 0021 — `DataFusion` / Arrow upgrade, the §5 acceptance scenarios (red).
//!
//! Scenario ids (RFC0021.1–.6, phase 1) are pinned here so the RFC → test
//! mapping is greppable from `red` on (per `docs/verification.md` §2.3).
//! Phase-2 scenarios (`.7`–`.9`) are upstream-gated (RFC 0021 §5) and get
//! their stubs only when a released `DataFusion` carries `object_store` ≥ 0.14 /
//! `parquet` 59.
//!
//! Two things here are **live from red**, not stubs:
//!
//! - [`rfc0021_fixture_writes_the_pre_upgrade_file`] (`#[ignore]`d,
//!   run manually) generated `testdata/rfc0021/pre-upgrade.parquet` with the
//!   **pre-upgrade (arrow 55) writer** — committed before any dependency
//!   moves, per RFC 0021 §6.
//! - [`rfc0021_2_pre_upgrade_fixture_reads_identically`] is the RFC0021.2
//!   scenario itself, live as a permanent regression guard: it decodes that
//!   committed file and asserts full `MinedRecord` equality against the
//!   in-code expectation. It passes on the current stack and MUST keep
//!   passing across the arrow 55 → 58 bump (§3.5: files written before the
//!   upgrade read identically after it) — there is no separate ignored stub
//!   for `.2`.
//!
//! At green, `.1`/`.4`/`.6` resolve where the code lives (lockfile /
//! querier row path / CI); `.3` maps onto the existing reconstruction
//! property + corpus suites; `.5` onto the benchmarks.md B1/B2 runs.
//!
//! See `docs/rfcs/0021-datafusion-arrow-upgrade.md` §5 / §6.

use ourios_core::audit::ParamType;
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_parquet::{DEFAULT_ZSTD_LEVEL, Reader, encode_records_to_parquet};

/// The committed pre-upgrade fixture, resolved from the workspace root (the
/// repo pattern for `testdata/` paths — parent-walk from `CARGO_MANIFEST_DIR`,
/// no `..` components in the resulting path).
fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root is two levels above CARGO_MANIFEST_DIR")
        .join("testdata/rfc0021/pre-upgrade.parquet")
}

/// Deterministic, representative rows covering the column shapes the §3.5
/// invariant protects: a templated body (params + separators + trace ids +
/// attributes), a structured body carrying Ourios-canonical JSON with a
/// proto3 non-finite double string form (RFC 0018 §3.4), a parse-failure
/// row (retained body, no template), and a minimal row (absent optionals).
fn fixture_records() -> Vec<MinedRecord> {
    let kv = |k: &str, v: &str| ourios_core::otlp::KeyValue {
        key: k.to_owned(),
        value: Some(ourios_core::otlp::AnyValue {
            value: Some(ourios_core::otlp::any_value::Value::StringValue(
                v.to_owned(),
            )),
        }),
        ..Default::default()
    };
    let base = MinedRecord {
        tenant_id: TenantId::new("rfc0021"),
        template_id: 7,
        template_version: 1,
        severity_number: 9,
        severity_text: Some("INFO".to_owned()),
        scope_name: Some("lib.checkout".to_owned()),
        scope_version: Some("2.1.0".to_owned()),
        scope_attributes: Vec::new(),
        resource_schema_url: Some("https://opentelemetry.io/schemas/1.42.0".to_owned()),
        scope_schema_url: None,
        time_unix_nano: 1_782_950_400_000_000_000,
        observed_time_unix_nano: Some(1_782_950_400_000_000_001),
        attributes: vec![kv("http.request.method", "GET")],
        dropped_attributes_count: 0,
        resource_attributes: vec![kv("service.name", "checkout")],
        trace_id: Some([0xA1; 16]),
        span_id: Some([0xB2; 8]),
        flags: 0x01,
        event_name: None,
        body_kind: BodyKind::String,
        params: vec![Param {
            type_tag: ParamType::Num,
            value: "42".to_owned(),
        }],
        separators: vec![String::new(), " ".to_owned(), String::new()],
        body: None,
        confidence: 1.0,
        lossy_flag: false,
    };

    let structured = MinedRecord {
        template_id: 8,
        event_name: Some("ourios.fixture.event".to_owned()),
        body_kind: BodyKind::Structured,
        params: Vec::new(),
        separators: Vec::new(),
        // Ourios-canonical JSON incl. the proto3 non-finite string form
        // (RFC 0018 §3.4) — the shape most sensitive to codec drift.
        body: Some(
            r#"{"kvlistValue":{"values":[{"key":"ratio","value":{"doubleValue":"NaN"}},{"key":"msg","value":{"stringValue":"checkout failed"}}]}}"#
                .to_owned(),
        ),
        ..base.clone()
    };

    let parse_failure = MinedRecord {
        template_id: 0,
        template_version: 0,
        severity_number: 17,
        severity_text: Some("ERROR".to_owned()),
        attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        params: Vec::new(),
        separators: Vec::new(),
        body: Some("\u{7f}binary\u{0} garbage line".to_owned()),
        confidence: 0.0,
        lossy_flag: true,
        ..base.clone()
    };

    let minimal = MinedRecord {
        severity_text: None,
        scope_name: None,
        scope_version: None,
        resource_schema_url: None,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        resource_attributes: Vec::new(),
        flags: 0,
        params: Vec::new(),
        separators: vec![String::new(), String::new()],
        ..base.clone()
    };

    vec![base, structured, parse_failure, minimal]
}

/// One-shot generator for the committed fixture — run manually **before** the
/// dependency bump (`cargo test -p ourios-parquet --test rfc0021_arrow_upgrade
/// -- --ignored rfc0021_fixture`), never in CI. Re-running it after the bump
/// would defeat the fixture's purpose (it must stay a pre-upgrade artefact).
#[test]
#[ignore = "fixture generator — run manually pre-upgrade only"]
fn rfc0021_fixture_writes_the_pre_upgrade_file() {
    let path = fixture_path();
    // Refuse to overwrite: regenerating after the dependency bump would
    // silently replace the pre-upgrade artefact with a post-upgrade one and
    // defeat the RFC0021.2 guard. Delete the file by hand first if a
    // regeneration is ever genuinely intended (pre-upgrade only).
    assert!(
        !path.exists(),
        "{} already exists — refusing to overwrite the pre-upgrade fixture",
        path.display(),
    );
    let bytes = encode_records_to_parquet(&fixture_records(), DEFAULT_ZSTD_LEVEL).expect("encode");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, bytes).expect("write fixture");
}

/// Scenario RFC0021.2 — old files read identically (§3.5).
/// See `docs/rfcs/0021-datafusion-arrow-upgrade.md` §5.
///
/// Live (not a stub) from red on purpose: the committed pre-upgrade file
/// decodes to exactly the expected rows. Green today on arrow 55; the
/// scenario is *discharged* by this same test staying green across the
/// 55 → 58 bump — a permanent regression guard, not a one-off check.
#[test]
fn rfc0021_2_pre_upgrade_fixture_reads_identically() {
    let records = Reader::open_file(&fixture_path())
        .expect("open fixture")
        .read_all()
        .expect("read fixture");
    assert_eq!(records, fixture_records());
}

/// Scenario RFC0021.1 — one arrow.
/// See `docs/rfcs/0021-datafusion-arrow-upgrade.md` §5.
///
/// Inspects the committed lockfile: every `arrow*` package is on the
/// single post-upgrade major, `datafusion` is on 54.x, and the workspace
/// MSRV is 1.88. A second arrow major sneaking back in via a transitive
/// bump fails here, not in a human review of a renovate diff.
#[test]
fn rfc0021_1_one_arrow_major_and_datafusion_54() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root is two levels above CARGO_MANIFEST_DIR");
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");

    let mut arrow_majors = std::collections::BTreeSet::new();
    let mut datafusion_versions = Vec::new();
    for block in lock.split("[[package]]") {
        let field = |key: &str| {
            block.lines().find_map(|l| {
                l.trim()
                    .strip_prefix(&format!("{key} = \""))
                    .and_then(|rest| rest.strip_suffix('"'))
            })
        };
        let Some(name) = field("name") else { continue };
        let version = field("version").expect("every package has a version");
        if name == "arrow" || name.starts_with("arrow-") {
            let major = version.split('.').next().expect("semver major");
            arrow_majors.insert(major.to_string());
        }
        if name == "datafusion" {
            datafusion_versions.push(version.to_string());
        }
    }
    assert_eq!(
        arrow_majors.into_iter().collect::<Vec<_>>(),
        ["58"],
        "exactly one arrow major (58.x) in the lockfile"
    );
    assert_eq!(
        datafusion_versions.len(),
        1,
        "exactly one datafusion in the lockfile, got {datafusion_versions:?}"
    );
    assert!(
        datafusion_versions[0].starts_with("54."),
        "datafusion 54.x, got {}",
        datafusion_versions[0]
    );

    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    assert!(
        manifest.contains("rust-version = \"1.88\""),
        "workspace MSRV is 1.88"
    );
}

/// Scenario RFC0021.3 — reconstruction stays bit-identical (§3.3).
/// See `docs/rfcs/0021-datafusion-arrow-upgrade.md` §5.
#[test]
#[ignore = "RFC0021.3 discharged — the reconstruction property + corpus suites (ourios-miner) ran unchanged and green on the upgraded stack (#339/#340); this marker points at that oracle"]
fn rfc0021_3_reconstruction_property_and_corpus_green() {}

/// Recursively swap `Utf8`→`Utf8View` / `Binary`→`BinaryView` through
/// `List`/`Struct` nesting — the view mix `DataFusion`'s scan can hand the
/// querier, including the `params` and `separators` element types.
fn to_view_type(dt: &arrow_schema::DataType) -> arrow_schema::DataType {
    use std::sync::Arc;

    use arrow_schema::DataType;
    match dt {
        DataType::Utf8 => DataType::Utf8View,
        DataType::Binary => DataType::BinaryView,
        DataType::List(f) => DataType::List(Arc::new(
            f.as_ref()
                .clone()
                .with_data_type(to_view_type(f.data_type())),
        )),
        DataType::Struct(fs) => DataType::Struct(
            fs.iter()
                .map(|f| {
                    Arc::new(
                        f.as_ref()
                            .clone()
                            .with_data_type(to_view_type(f.data_type())),
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Scenario RFC0021.4 — the RFC 0017 dual decoder is gone (#276).
/// See `docs/rfcs/0021-datafusion-arrow-upgrade.md` §5.
///
/// The structural half (the querier's `row_decode` duplicate deleted, the
/// `schema_force_view_types` override removed, `DataFusion` feeding its
/// default view representations through the shared decoder end-to-end) is
/// proven by the RFC 0017 suites. This test pins the property that makes
/// that possible: the **single** [`batch_to_mined_records`] path decodes a
/// batch whose string/binary columns are `Utf8View`/`BinaryView` to exactly
/// the rows the plain `Utf8`/`Binary` representation yields.
#[test]
fn rfc0021_4_shared_decoder_reads_view_and_plain_representations() {
    use std::sync::Arc;

    use ourios_parquet::ShapeValidation;

    // A real plain-representation batch, straight from the parquet bytes.
    let bytes = encode_records_to_parquet(&fixture_records(), DEFAULT_ZSTD_LEVEL).expect("encode");
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        bytes::Bytes::from(bytes),
    )
    .expect("builder")
    .build()
    .expect("reader");
    let batches: Vec<_> = reader.collect::<Result<_, _>>().expect("batches");
    assert_eq!(batches.len(), 1, "fixture fits one batch");
    let plain = &batches[0];

    // The same batch with every Utf8/Binary — flat *and* nested inside the
    // `params` list-of-struct and `separators` list — cast to its view
    // representation via `to_view_type`.
    let plain_schema = plain.schema();
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    let mut recast = Vec::new();
    for (field, column) in plain_schema.fields().iter().zip(plain.columns()) {
        let target = to_view_type(field.data_type());
        if target == *field.data_type() {
            fields.push(field.as_ref().clone());
            columns.push(Arc::clone(column));
        } else {
            let cast = arrow_cast::cast(column, &target).expect("cast to view");
            fields.push(field.as_ref().clone().with_data_type(target));
            columns.push(cast);
            recast.push(field.name().as_str());
        }
    }
    // Guard the test itself: the nested columns must actually change
    // representation, or the view-decode assertion below proves nothing
    // about the params/separators element paths.
    for name in ["tenant_id", "body", "params", "separators"] {
        assert!(
            recast.contains(&name),
            "{name} was not recast to a view type"
        );
    }
    let schema = Arc::new(arrow_schema::Schema::new(fields));
    let view = arrow_array::RecordBatch::try_new(schema, columns).expect("view batch");

    let from_plain = ourios_parquet::batch_to_mined_records(plain, 0, ShapeValidation::Enforce)
        .expect("plain decodes");
    let from_view = ourios_parquet::batch_to_mined_records(&view, 0, ShapeValidation::Enforce)
        .expect("view decodes");
    assert_eq!(from_view, from_plain);
    assert_eq!(from_plain, fixture_records());
}

/// Scenario RFC0021.5 — the B1/B2 pruning thesis holds.
/// See `docs/rfcs/0021-datafusion-arrow-upgrade.md` §5.
#[test]
#[ignore = "RFC0021.5 discharged — structural: the rfc0007_1_* pruning tests (ourios-querier) are green on the upgraded stack; wall-clock: indicative ci-runner query-bench on PR #340 showed no regression (authoritative baseline rerun stays maintainer opt-in)"]
fn rfc0021_5_pruning_gates_hold() {}

/// Scenario RFC0021.6 — the full gate is green.
/// See `docs/rfcs/0021-datafusion-arrow-upgrade.md` §5.
#[test]
#[ignore = "RFC0021.6 discharged — CI is the oracle: the complete suite (incl. s3-integration + live-check) is green on main with phase 1 merged (#339/#340)"]
fn rfc0021_6_full_gate_green() {}
