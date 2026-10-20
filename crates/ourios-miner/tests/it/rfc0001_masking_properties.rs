//! RFC 0001 §8 property obligations for the §6.2 step 2 masking rules,
//! including the RFC0001.17/.19 `TS` and decimal shapes.
//!
//! 1. **Masking changes only the tokens a rule matches.** Every
//!    position `mask` leaves unmasked is byte-identical to its input
//!    token; every masked position yields one typed param whose value
//!    is the original token; generated well-formed timestamps classify
//!    as `TS` and decimals and integers as `NUM`.
//! 2. **Reconstruction covers the new shapes** (§3.3, H7.1). Lines
//!    mixing timestamps, decimals and the other masked shapes with
//!    arbitrary whitespace mine to records that rebuild byte for byte,
//!    or are flagged lossy with the body retained.

use std::collections::HashMap;

use ourios_config::MinerConfig;
use ourios_core::audit::ParamType;
use ourios_core::otlp::{Body, OtlpLogRecord};
use ourios_core::record::SharedRecordSink;
use ourios_core::tenant::TenantId;
use ourios_miner::cluster::MinerCluster;
use ourios_miner::mask::mask;
use ourios_miner::reconstruct::reconstruct;
use ourios_miner::tree::OwnedToken;
use proptest::prelude::*;

fn two_digits(range: std::ops::RangeInclusive<u8>) -> impl Strategy<Value = String> {
    range.prop_map(|n| format!("{n:02}"))
}

fn date() -> impl Strategy<Value = String> {
    (
        1000u16..=9999,
        prop_oneof![Just('-'), Just('/')],
        two_digits(1..=12),
        two_digits(1..=31),
    )
        .prop_map(|(y, s, m, d)| format!("{y}{s}{m}{s}{d}"))
}

fn time() -> impl Strategy<Value = String> {
    let fraction = prop_oneof![
        Just(String::new()),
        (prop_oneof![Just('.'), Just(',')], "[0-9]{1,9}").prop_map(|(s, d)| format!("{s}{d}")),
    ];
    let zone = prop_oneof![
        Just(String::new()),
        Just("Z".to_string()),
        (prop_oneof![Just('+'), Just('-')], two_digits(0..=23))
            .prop_map(|(s, h)| format!("{s}{h}")),
        (
            prop_oneof![Just('+'), Just('-')],
            two_digits(0..=23),
            two_digits(0..=59)
        )
            .prop_map(|(s, h, m)| format!("{s}{h}{m}")),
        (
            prop_oneof![Just('+'), Just('-')],
            two_digits(0..=23),
            two_digits(0..=59)
        )
            .prop_map(|(s, h, m)| format!("{s}{h}:{m}")),
    ];
    (
        two_digits(0..=23),
        two_digits(0..=59),
        two_digits(0..=60),
        fraction,
        zone,
    )
        .prop_map(|(h, m, s, f, z)| format!("{h}:{m}:{s}{f}{z}"))
}

/// A token and, when the generator guarantees one, the type `mask` must
/// give it.
fn token() -> impl Strategy<Value = (String, Option<ParamType>)> {
    prop_oneof![
        4 => "[a-z]{1,8}".prop_map(|s| (s, None)),
        2 => date().prop_map(|s| (s, Some(ParamType::Ts))),
        2 => time().prop_map(|s| (s, Some(ParamType::Ts))),
        2 => (date(), time()).prop_map(|(d, t)| (format!("{d}T{t}"), Some(ParamType::Ts))),
        2 => "-?[0-9]{1,6}\\.[0-9]{1,6}".prop_map(|s| (s, Some(ParamType::Num))),
        1 => "-?[0-9]{1,6}".prop_map(|s| (s, Some(ParamType::Num))),
        1 => (0u8..=255, 0u8..=255, 0u8..=255, 0u8..=255)
            .prop_map(|(a, b, c, d)| (format!("{a}.{b}.{c}.{d}"), Some(ParamType::Ip))),
        3 => "[A-Za-z0-9_.:/,+-]{1,12}".prop_map(|s| (s, None)),
    ]
}

fn whitespace() -> impl Strategy<Value = String> {
    prop::string::string_regex("[ \t]{1,3}").expect("valid regex")
}

/// A line of generated tokens joined by generated whitespace, with
/// optional leading and trailing whitespace.
fn line() -> impl Strategy<Value = String> {
    (
        prop::collection::vec((token(), whitespace()), 1..10),
        prop::option::of(whitespace()),
    )
        .prop_map(|(parts, lead)| {
            let mut out = lead.unwrap_or_default();
            for ((tok, _), sep) in parts {
                out.push_str(&tok);
                out.push_str(&sep);
            }
            out
        })
}

proptest! {
    #[test]
    fn mask_changes_only_the_tokens_a_rule_matches(
        tokens in prop::collection::vec(token(), 1..16),
    ) {
        let refs: Vec<&str> = tokens.iter().map(|(t, _)| t.as_str()).collect();

        let m = mask(&refs);

        prop_assert_eq!(m.tokens.len(), refs.len());
        prop_assert_eq!(m.typed_params.len(), m.wildcard_positions.len());
        let mut params = m.typed_params.iter();
        for (i, (original, expected)) in tokens.iter().enumerate() {
            if m.wildcard_positions.contains(&i) {
                let param = params.next().expect("one param per masked position");
                prop_assert_eq!(param.value, original.as_str(), "param {} is its own bytes", i);
                if let Some(expected) = expected {
                    prop_assert_eq!(param.type_tag, *expected, "token {:?}", original);
                }
            } else {
                prop_assert_eq!(m.tokens[i], original.as_str(), "unmasked token {} unchanged", i);
                prop_assert!(expected.is_none(), "{:?} must mask as {:?}", original, expected);
            }
        }
    }

    #[test]
    fn masked_lines_reconstruct_byte_for_byte(lines in prop::collection::vec(line(), 1..8)) {
        let records = SharedRecordSink::new();
        let mut cluster =
            MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(records.clone()));
        let t = TenantId::new("tenant-x");
        // As H7.1: seal each `(id, version)`'s tokens when first seen, so a
        // record reconstructs against the template it was emitted under.
        let mut versions: HashMap<(u64, u32), Vec<OwnedToken>> = HashMap::new();
        for line in &lines {
            cluster.ingest(&OtlpLogRecord {
                tenant_id: t.clone(),
                body: Some(Body::String(line.clone())),
                ..Default::default()
            });
            for leaf in cluster.templates_for(&t) {
                versions
                    .entry((leaf.template_id, leaf.template_version))
                    .or_insert(leaf.template);
            }
        }

        let emitted = records.drain();
        prop_assert_eq!(emitted.len(), lines.len());
        for (line, record) in lines.iter().zip(&emitted) {
            if record.lossy_flag {
                prop_assert_eq!(record.body.as_deref(), Some(line.as_str()), "lossy keeps the body");
                continue;
            }
            let template = versions
                .get(&(record.template_id, record.template_version))
                .map_or(&[][..], Vec::as_slice);
            prop_assert_eq!(reconstruct(record, template), line.as_bytes());
        }
    }
}
