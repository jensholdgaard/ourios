//! RFC 0050 §5 property obligations for the upstream grammar
//! (the RFC0050.4 render-equals-body arm, at the module level).
//!
//! Two properties over generated inputs:
//!
//! 1. **Constructed masks align.** For any tokenized line, masking
//!    an arbitrary subset of tokens with `<*>` yields a template
//!    that parses, aligns, captures exactly the masked tokens as
//!    parameters, and interleaves back to the original line byte
//!    for byte — whatever the separators were.
//! 2. **Arbitrary inputs are safe.** For *any* pair of strings, a
//!    successful parse + align implies byte-identical
//!    reconstruction; everything else is a typed rejection, never
//!    a panic and never a silent partial match.

use std::collections::HashMap;

use ourios_config::{MinerConfig, UpstreamTemplates};
use ourios_core::otlp::{AnyValue, Body, KeyValue, OtlpLogRecord, any_value};
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::{MinerCluster, NO_TEMPLATE};
use ourios_miner::reconstruct::reconstruct;
use ourios_miner::tree::OwnedToken;
use ourios_miner::upstream::{
    Alignment, LOG_RECORD_TEMPLATE_ATTR, UpstreamTemplate, UpstreamToken, align, parse_template,
};
use proptest::prelude::*;

/// Interleave an alignment back into a line — the oracle for the
/// §3.4 byte-identity claim.
fn reassemble(template: &UpstreamTemplate<'_>, a: &Alignment<'_, '_>) -> String {
    let mut out = String::from(a.separators[0]);
    let mut next_param = 0;
    for (i, t) in template.tokens().iter().enumerate() {
        match t {
            UpstreamToken::Literal(s) => out.push_str(s),
            UpstreamToken::Wildcard { .. } => {
                out.push_str(a.params[next_param].value);
                next_param += 1;
            }
        }
        out.push_str(a.separators[i + 1]);
    }
    out
}

/// Literal-token alphabet that cannot collide with the wildcard
/// shape (`<`/`>`) or the foreign-placeholder detectors
/// (`%`/`$`/`{`/`}`) — those rejections get their own coverage in
/// the unit table and in `arbitrary_inputs_are_safe`.
fn literal_token() -> impl Strategy<Value = String> {
    prop::string::string_regex("[A-Za-z0-9_.:=,/-]{1,10}").expect("valid regex")
}

fn whitespace() -> impl Strategy<Value = String> {
    prop::string::string_regex("[ \t]{1,3}").expect("valid regex")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(ourios_testgen::proptest_cases(64)))]

    #[test]
    fn constructed_masks_align_and_reconstruct(
        tokens in prop::collection::vec(literal_token(), 1..12),
        mask in prop::collection::vec(any::<bool>(), 12),
        seps in prop::collection::vec(whitespace(), 12),
        lead in prop::option::of(whitespace()),
        trail in prop::option::of(whitespace()),
    ) {
        let mut body = lead.unwrap_or_default();
        for (i, tok) in tokens.iter().enumerate() {
            if i > 0 {
                body.push_str(&seps[i]);
            }
            body.push_str(tok);
        }
        body.push_str(&trail.unwrap_or_default());

        let template_str = tokens
            .iter()
            .enumerate()
            .map(|(i, tok)| if mask[i] { "<*>" } else { tok.as_str() })
            .collect::<Vec<_>>()
            .join(" ");

        let template = parse_template(&template_str).expect("constructed template parses");
        let a = align(&template, &body).expect("constructed template aligns");

        prop_assert_eq!(reassemble(&template, &a), body.clone());

        let expected: Vec<&str> = tokens
            .iter()
            .zip(&mask)
            .filter(|(_, m)| **m)
            .map(|(tok, _)| tok.as_str())
            .collect();
        let captured: Vec<&str> = a.params.iter().map(|p| p.value).collect();
        prop_assert_eq!(captured, expected);
    }

    #[test]
    fn arbitrary_inputs_are_safe(
        template in ".{0,80}",
        body in ".{0,80}",
    ) {
        if let Ok(parsed) = parse_template(&template)
            && let Ok(a) = align(&parsed, &body)
        {
            prop_assert_eq!(reassemble(&parsed, &a), body.clone());
        }
    }

    /// RFC0050.4's property at the *ingest* level: rows emitted by
    /// the adopt path — params and separators from `align`, not
    /// from `mask` + `tokenize` — reconstruct byte for byte.
    #[test]
    fn adopted_rows_reconstruct_byte_for_byte(
        tokens in prop::collection::vec(literal_token(), 1..10),
        mask in prop::collection::vec(any::<bool>(), 10),
        seps in prop::collection::vec(whitespace(), 10),
        lead in prop::option::of(whitespace()),
        trail in prop::option::of(whitespace()),
    ) {
        let mut body = lead.unwrap_or_default();
        for (i, tok) in tokens.iter().enumerate() {
            if i > 0 {
                body.push_str(&seps[i]);
            }
            body.push_str(tok);
        }
        body.push_str(&trail.unwrap_or_default());
        let template_str = tokens
            .iter()
            .enumerate()
            .map(|(i, tok)| if mask[i] { "<*>" } else { tok.as_str() })
            .collect::<Vec<_>>()
            .join(" ");

        let tenant = TenantId::new("t");
        let sink = SharedRecordSink::new();
        let mut cluster = MinerCluster::new(
            MinerConfig::default().with_upstream_templates(UpstreamTemplates::Adopt),
        )
        .with_record_sink(Box::new(sink.clone()));
        let id = cluster.ingest(&annotated(&tenant, &body, &template_str));
        prop_assert_ne!(id, NO_TEMPLATE);

        let rows = sink.drain();
        prop_assert_eq!(rows.len(), 1);
        let row = &rows[0];
        prop_assert!(!row.lossy_flag);
        let template = parse_template(&template_str)
            .expect("constructed template parses")
            .to_owned_tokens();
        prop_assert_eq!(reconstruct(row, &template), body.into_bytes());
    }

    /// The whole-path safety net over *arbitrary* attribute and
    /// body strings: whatever route a record takes under `adopt`
    /// (adopted, mined, parse failure), the emitted row either
    /// reconstructs to the original bytes from its registry
    /// template or falls back to its retained body — never a
    /// silent in-between (CLAUDE.md §3.3).
    #[test]
    fn adopt_ingest_never_breaks_reconstruction(
        template in ".{1,60}",
        body in ".{0,120}",
    ) {
        let tenant = TenantId::new("t");
        let sink = SharedRecordSink::new();
        let mut cluster = MinerCluster::new(
            MinerConfig::default().with_upstream_templates(UpstreamTemplates::Adopt),
        )
        .with_record_sink(Box::new(sink.clone()));
        let _ = cluster.ingest(&annotated(&tenant, &body, &template));

        let rows = sink.drain();
        prop_assert_eq!(rows.len(), 1);
        let row = &rows[0];

        let mut registry: HashMap<(u64, u32), Vec<OwnedToken>> = cluster
            .templates_for(&tenant)
            .into_iter()
            .map(|l| ((l.template_id, l.template_version), l.template))
            .collect();
        for a in cluster.adopted_templates_for(&tenant) {
            registry
                .entry((a.template_id, a.template_version))
                .or_insert_with(|| ourios_miner::tree::parse_template(&a.canonical));
        }
        let empty = Vec::new();
        let tokens = registry
            .get(&(row.template_id, row.template_version))
            .unwrap_or(&empty);
        prop_assert_eq!(reconstruct(row, tokens), body.into_bytes());
    }
}

/// Test helper — a string record carrying a `log.record.template`
/// attribute.
fn annotated(tenant: &TenantId, body: &str, template: &str) -> OtlpLogRecord {
    let mut record = OtlpLogRecord {
        tenant_id: tenant.clone(),
        body: Some(Body::String(body.to_string())),
        ..Default::default()
    };
    record.attributes.push(KeyValue {
        key: LOG_RECORD_TEMPLATE_ATTR.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(template.to_string())),
        }),
        ..Default::default()
    });
    record
}
