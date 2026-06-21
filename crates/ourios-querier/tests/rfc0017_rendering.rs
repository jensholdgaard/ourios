//! RFC 0017 — read-time template registry & query-row rendering, the
//! body-rendering scenarios (`.3`, `.4`, `.9`).
//!
//! `.3` (clean rows render bit-identically), `.4` (lossy / parse-failure rows
//! return the retained body), and `.9` (a structured `AnyValue` body is
//! returned as structure) drive `render_log_body` against a directly-built
//! `TemplateRegistry`.
//!
//! See `docs/rfcs/0017-template-registry-query-rendering.md` §3.3 / §3.4 / §5 / §6.

use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::canonical::encode_any_value;
use ourios_core::otlp::{AnyValue, KeyValue, KeyValueList};
use ourios_core::record::{BodyKind, MinedRecord, Param};
use ourios_core::tenant::TenantId;
use ourios_miner::reconstruct::Reconstruction;
use ourios_miner::tree::OwnedToken;
use ourios_querier::{LogBody, TemplateRegistry, render_log_body};

/// A baseline `MinedRecord` — a string body, no template, nothing retained.
/// Tests override the fields the scenario exercises.
fn record(body_kind: BodyKind) -> MinedRecord {
    MinedRecord {
        tenant_id: TenantId::new("acme"),
        template_id: 0,
        template_version: 0,
        severity_number: 0,
        severity_text: None,
        scope_name: None,
        scope_version: None,
        scope_attributes: Vec::new(),
        resource_schema_url: None,
        scope_schema_url: None,
        time_unix_nano: 0,
        observed_time_unix_nano: None,
        attributes: Vec::new(),
        dropped_attributes_count: 0,
        resource_attributes: Vec::new(),
        trace_id: None,
        span_id: None,
        flags: 0,
        event_name: None,
        body_kind,
        params: Vec::new(),
        separators: Vec::new(),
        body: None,
        confidence: 0.0,
        lossy_flag: false,
    }
}

/// Scenario RFC0017.3 — a stored clean-path (`Faithful`-eligible) row, rendered
/// via the derived registry tokens, equals the originally-ingested line
/// byte-for-byte (CLAUDE.md §3.3 bit-identical invariant), and the row's
/// `Reconstruction` marker is `Faithful`.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
fn rfc0017_3_clean_row_renders_bit_identically() {
    // Leaf v2 of template 7: "user <*> logged in from <*>".
    let tokens = vec![
        OwnedToken::Fixed("user".to_owned()),
        OwnedToken::Wildcard,
        OwnedToken::Fixed("logged".to_owned()),
        OwnedToken::Fixed("in".to_owned()),
        OwnedToken::Fixed("from".to_owned()),
        OwnedToken::Wildcard,
    ];
    let registry: TemplateRegistry = TemplateRegistry::from([((7, 2), tokens)]);

    let mut r = record(BodyKind::String);
    r.template_id = 7;
    r.template_version = 2;
    r.params = vec![
        Param {
            type_tag: ourios_core::audit::ParamType::Num,
            value: "42".to_owned(),
        },
        Param {
            type_tag: ourios_core::audit::ParamType::Ip,
            value: "10.0.0.1".to_owned(),
        },
    ];
    r.separators = vec![
        String::new(),
        " ".to_owned(),
        " ".to_owned(),
        " ".to_owned(),
        " ".to_owned(),
        " ".to_owned(),
        String::new(),
    ];

    let body = render_log_body(&r, &registry);
    assert_eq!(
        body,
        LogBody::Rendered {
            line: b"user 42 logged in from 10.0.0.1".to_vec(),
            reconstruction: Reconstruction::Faithful,
        },
        "clean row renders bit-identically with the Faithful marker",
    );
}

/// Scenario RFC0017.4 — a row flagged lossy or with no template (parse failure),
/// whose `body` was retained, renders the retained `body` verbatim with marker
/// `RetainedVerbatim` — no template walk, never a wrong reconstruction.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
fn rfc0017_4_lossy_rows_return_retained_body() {
    let registry = TemplateRegistry::new();

    // Lossy clean-shape row: even with a template in the registry, the
    // lossy_flag forces the retained body verbatim (no reconstruction).
    let mut lossy = record(BodyKind::String);
    lossy.template_id = 1;
    lossy.template_version = 1;
    lossy.lossy_flag = true;
    lossy.body = Some("raw line as ingested".to_owned());
    let registry_with_token: TemplateRegistry =
        TemplateRegistry::from([((1, 1), vec![OwnedToken::Wildcard])]);
    assert_eq!(
        render_log_body(&lossy, &registry_with_token),
        LogBody::Rendered {
            line: b"raw line as ingested".to_vec(),
            reconstruction: Reconstruction::RetainedVerbatim,
        },
        "a lossy row returns the retained body verbatim",
    );

    // Parse-failure row: no template (id 0, absent from the registry), body
    // retained — the not-in-registry empty-token fallback returns it verbatim.
    let mut parse_fail = record(BodyKind::String);
    parse_fail.body = Some("unparseable !@#$ line".to_owned());
    assert_eq!(
        render_log_body(&parse_fail, &registry),
        LogBody::Rendered {
            line: b"unparseable !@#$ line".to_vec(),
            reconstruction: Reconstruction::RetainedVerbatim,
        },
        "a parse-failure row (no template) returns the retained body verbatim",
    );
}

/// Scenario RFC0017.9 — a stored row with `body_kind = Structured` (the OTLP
/// `Body` was a map/array, canonical JSON in `body`) is returned as
/// `LogBody::Structured(AnyValue)`, preserving the original map/array shape
/// (not flattened to a byte line) and round-tripping the ingested `AnyValue`.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
fn rfc0017_9_structured_body_returned_as_structure() {
    // A structured Body: a kvlist {"event":"login","attempts":<int>}.
    let original = AnyValue {
        value: Some(Value::KvlistValue(KeyValueList {
            values: vec![
                KeyValue {
                    key: "event".to_owned(),
                    value: Some(AnyValue {
                        value: Some(Value::StringValue("login".to_owned())),
                    }),
                    ..Default::default()
                },
                KeyValue {
                    key: "attempts".to_owned(),
                    value: Some(AnyValue {
                        value: Some(Value::IntValue(3)),
                    }),
                    ..Default::default()
                },
            ],
        })),
    };
    let canonical = encode_any_value(&original).expect("encode");

    let mut r = record(BodyKind::Structured);
    r.body = Some(String::from_utf8(canonical).expect("canonical JSON is UTF-8"));

    let body = render_log_body(&r, &TemplateRegistry::new());
    assert_eq!(
        body,
        LogBody::Structured(original),
        "a structured body returns as structure, round-tripping the AnyValue",
    );
}

/// Scenario RFC0017.5 — a row carrying `template_version = N` renders against
/// the N-version tokens (the registry entry whose version is `N`), not the
/// latest: a line ingested before a widening reconstructs as it was then.
/// See `docs/rfcs/0017-template-registry-query-rendering.md` §5.
#[test]
fn rfc0017_5_rows_render_against_their_own_version() {
    // Template 7 across two versions: v1 "user <*>" (one slot), v2
    // "user <*> <*>" (two slots, after a widening).
    let registry: TemplateRegistry = TemplateRegistry::from([
        (
            (7, 1),
            vec![OwnedToken::Fixed("user".to_owned()), OwnedToken::Wildcard],
        ),
        (
            (7, 2),
            vec![
                OwnedToken::Fixed("user".to_owned()),
                OwnedToken::Wildcard,
                OwnedToken::Wildcard,
            ],
        ),
    ]);

    // A row stamped version 1, carrying one param — the shape of v1.
    let mut v1_row = record(BodyKind::String);
    v1_row.template_id = 7;
    v1_row.template_version = 1;
    v1_row.params = vec![Param {
        type_tag: ourios_core::audit::ParamType::Num,
        value: "42".to_owned(),
    }];
    v1_row.separators = vec![String::new(), " ".to_owned(), String::new()];

    // Renders against v1 tokens → "user 42", Faithful. Had it used the v2
    // tokens (two wildcards) the single-param shape wouldn't match and the
    // marker would be RetainedVerbatim, not a clean "user 42".
    assert_eq!(
        render_log_body(&v1_row, &registry),
        LogBody::Rendered {
            line: b"user 42".to_vec(),
            reconstruction: Reconstruction::Faithful,
        },
        "a version-1 row renders against version-1 tokens, not the widened v2 tokens",
    );
}

/// A structured row whose `body` is absent (a corrupt row — no structure to
/// return) falls back to the render contract's empty / `RetainedVerbatim`,
/// never `Structured` over nothing (RFC 0017 §3.4 edge).
#[test]
fn rfc0017_9_structured_row_with_absent_body_falls_back() {
    let r = record(BodyKind::Structured);
    assert_eq!(
        render_log_body(&r, &TemplateRegistry::new()),
        LogBody::Rendered {
            line: Vec::new(),
            reconstruction: Reconstruction::RetainedVerbatim,
        },
        "a structured row with no body renders empty/RetainedVerbatim, not Structured",
    );
}
