//! RFC 0024 §3.2 generation strategies over [`OtlpLogRecord`].
//!
//! [`calibrated`] draws every field distribution from a
//! [`CalibrationManifest`] so batches sit in the realistic centre of
//! a measured corpus; [`adversarial`] roams the envelope's legal
//! extremes, bounded only by the documented limits below.

use ourios_core::config::MinerConfig;
use ourios_core::otlp::{
    AnyValue, ArrayValue, Body, KeyValue, KeyValueList, OtlpLogRecord, any_value,
};
use ourios_core::tenant::TenantId;
use proptest::prelude::*;
use proptest::strategy::Union;

use crate::manifest::{
    AnyValueShapes, CalibrationManifest, ExactHistogram, Log2Histogram, log2_bucket_range,
};

/// Tenant every generated record carries. Single-tenant by design —
/// tenant isolation has its own suites (RFC0007.5); the RFC 0024
/// properties hold the tenant fixed so failures shrink cleanly.
pub const TESTGEN_TENANT: &str = "rfc0024-testgen";

/// Upper bound on `AnyValue` nesting depth in generated trees (a
/// bare leaf is depth `1`). RFC 0024 §3.2's "canonical-JSON depth
/// bound": each `AnyValue` layer costs at most four levels of JSON
/// nesting (`{"kvlistValue":{"values":[{"key":…,"value":…}]}}`), so
/// depth 16 stays at ≤ 64 JSON levels — half of `serde_json`'s
/// 128-level recursion limit the canonical codec parses under.
pub const MAX_ANY_VALUE_DEPTH: u32 = 16;

/// Largest attribute map [`adversarial`] generates — RFC 0024
/// §3.2's "a few thousand entries".
pub const ADVERSARIAL_MAX_ATTRIBUTES: usize = 2048;

/// Cap on generated string-body length in [`calibrated`] mode. A
/// corpus histogram may have tail buckets in the megabytes; sampling
/// those verbatim would dominate generation wall-clock for no extra
/// property coverage (the §3.2 hazard lives in the *miner's* limits,
/// far below this).
pub const CALIBRATED_BODY_LEN_CAP: usize = 1 << 16;

/// 2026-01-01T00:00:00Z — fixed epoch for plausible timestamps, so
/// generation is reproducible (no wall clock).
const TIME_BASE_UNIX_NANO: u64 = 1_767_225_600_000_000_000;

/// Span of the plausible-timestamp window (one week).
const TIME_WINDOW_NANO: u64 = 7 * 24 * 3_600_000_000_000;

/// Recursion budget handed to `prop_recursive` when growing
/// containers: two below [`MAX_ANY_VALUE_DEPTH`] so the leaf level
/// (and any off-by-one in the budget's accounting) can never carry a
/// generated tree past the documented bound.
const RECURSION_LEVELS: u32 = MAX_ANY_VALUE_DEPTH - 2;

/// A generation weight from a manifest count: clamped into `u32`
/// (proptest's weight type) and floored at `1` so a bucket present
/// in the manifest is never unreachable.
fn weight(count: u64) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX).max(1)
}

fn string_value(s: String) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::StringValue(s)),
    }
}

fn int_value(i: i64) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::IntValue(i)),
    }
}

fn double_value(d: f64) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::DoubleValue(d)),
    }
}

fn bool_value(b: bool) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::BoolValue(b)),
    }
}

fn bytes_value(b: Vec<u8>) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::BytesValue(b)),
    }
}

fn array_value(values: Vec<AnyValue>) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::ArrayValue(ArrayValue { values })),
    }
}

fn kvlist_value(entries: Vec<(String, Option<AnyValue>)>) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::KvlistValue(KeyValueList {
            values: entries.into_iter().map(|(k, v)| key_value(k, v)).collect(),
        })),
    }
}

fn key_value(key: String, value: Option<AnyValue>) -> KeyValue {
    KeyValue {
        key,
        value,
        ..Default::default()
    }
}

/// Exactly `len` characters of word-ish ASCII text (letters, digits,
/// spaces) — enough lexical variety for the miner's tokenizer
/// without modelling any particular log dialect.
fn ascii_text(len: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            18 => proptest::char::range('a', 'z'),
            3 => proptest::char::range('0', '9'),
            4 => Just(' '),
        ],
        len..=len,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

// ---- calibrated mode (RFC 0024 §3.2) -------------------------------

/// Records whose field distributions are weighted by `manifest` —
/// the realistic centre of the corpus the manifest measured.
/// Statistical, not exact: RFC0024.2 checks gross moments.
///
/// An empty (default) manifest still generates: every distribution
/// falls back to a small documented default rather than an empty
/// weight set.
pub fn calibrated(manifest: &CalibrationManifest) -> impl Strategy<Value = OtlpLogRecord> {
    (
        calibrated_severity(manifest),
        calibrated_body(manifest),
        calibrated_attributes(
            &manifest.log_attribute_count,
            &manifest.any_value_shapes,
            manifest.distinct_attribute_keys,
            "log",
        ),
        calibrated_attributes(
            &manifest.resource_attribute_count,
            &manifest.any_value_shapes,
            manifest.distinct_attribute_keys,
            "resource",
        ),
        0..TIME_WINDOW_NANO,
    )
        .prop_map(
            |((severity_number, severity_text), body, attributes, resource_attributes, offset)| {
                OtlpLogRecord {
                    tenant_id: TenantId::new(TESTGEN_TENANT),
                    time_unix_nano: TIME_BASE_UNIX_NANO + offset,
                    severity_number,
                    severity_text,
                    attributes,
                    resource_attributes,
                    body,
                    ..Default::default()
                }
            },
        )
}

fn calibrated_severity(manifest: &CalibrationManifest) -> BoxedStrategy<(u8, Option<String>)> {
    if manifest.severity.is_empty() {
        return Just((9, Some("INFO".to_string()))).boxed();
    }
    Union::new_weighted(
        manifest
            .severity
            .iter()
            .map(|bucket| {
                (
                    weight(bucket.count),
                    Just((bucket.number, bucket.text.clone())).boxed(),
                )
            })
            .collect(),
    )
    .boxed()
}

fn calibrated_body(manifest: &CalibrationManifest) -> BoxedStrategy<Option<Body>> {
    let kinds = &manifest.body_kind;
    let mut arms: Vec<(u32, BoxedStrategy<Option<Body>>)> = Vec::new();
    if kinds.absent > 0 {
        arms.push((weight(kinds.absent), Just(None).boxed()));
    }
    if kinds.string > 0 {
        arms.push((
            weight(kinds.string),
            calibrated_string_body(&manifest.string_body_len)
                .prop_map(Some)
                .boxed(),
        ));
    }
    if kinds.structured > 0 {
        arms.push((
            weight(kinds.structured),
            calibrated_structured_body(&manifest.any_value_shapes)
                .prop_map(Some)
                .boxed(),
        ));
    }
    if arms.is_empty() {
        // Unmeasured manifest: default to short string bodies.
        return calibrated_string_body(&manifest.string_body_len)
            .prop_map(Some)
            .boxed();
    }
    Union::new_weighted(arms).boxed()
}

fn calibrated_string_body(histogram: &Log2Histogram) -> BoxedStrategy<Body> {
    let length_arms: Vec<(u32, BoxedStrategy<usize>)> = if histogram.is_empty() {
        vec![(1, (8usize..64).boxed())]
    } else {
        histogram
            .iter()
            .map(|(&bucket, &count)| {
                let (lo, hi) = log2_bucket_range(bucket);
                let lo = clamp_len(lo);
                let hi = clamp_len(hi);
                (weight(count), (lo..=hi).boxed())
            })
            .collect()
    };
    Union::new_weighted(length_arms)
        .prop_flat_map(ascii_text)
        .prop_map(Body::String)
        .boxed()
}

fn clamp_len(n: u64) -> usize {
    usize::try_from(n)
        .unwrap_or(CALIBRATED_BODY_LEN_CAP)
        .min(CALIBRATED_BODY_LEN_CAP)
}

/// A leaf `AnyValue` weighted by the manifest's shape counts.
/// Container shapes are handled by the callers that can afford them;
/// an all-zero shape set falls back to short strings.
fn calibrated_leaf(shapes: &AnyValueShapes) -> BoxedStrategy<AnyValue> {
    let mut arms: Vec<(u32, BoxedStrategy<AnyValue>)> = Vec::new();
    if shapes.empty > 0 {
        arms.push((weight(shapes.empty), Just(AnyValue { value: None }).boxed()));
    }
    if shapes.string > 0 {
        arms.push((
            weight(shapes.string),
            (4usize..24)
                .prop_flat_map(ascii_text)
                .prop_map(string_value)
                .boxed(),
        ));
    }
    if shapes.int > 0 {
        arms.push((weight(shapes.int), any::<i64>().prop_map(int_value).boxed()));
    }
    if shapes.double > 0 {
        // Finite by construction — the calibrated centre; non-finite
        // doubles are adversarial-mode territory.
        arms.push((
            weight(shapes.double),
            (-1.0e12..1.0e12).prop_map(double_value).boxed(),
        ));
    }
    if shapes.boolean > 0 {
        arms.push((
            weight(shapes.boolean),
            any::<bool>().prop_map(bool_value).boxed(),
        ));
    }
    if shapes.bytes > 0 {
        arms.push((
            weight(shapes.bytes),
            proptest::collection::vec(any::<u8>(), 0..32)
                .prop_map(bytes_value)
                .boxed(),
        ));
    }
    if arms.is_empty() {
        return (4usize..24)
            .prop_flat_map(ascii_text)
            .prop_map(string_value)
            .boxed();
    }
    Union::new_weighted(arms).boxed()
}

/// A structured body whose *top level* is never a string and never
/// empty — those would fork to `Body::String` / no body at the
/// receiver, silently shifting the generated `body_kind` mix away
/// from the manifest's.
fn calibrated_structured_body(shapes: &AnyValueShapes) -> BoxedStrategy<Body> {
    let leaf = calibrated_leaf(shapes);
    let mut arms: Vec<(u32, BoxedStrategy<AnyValue>)> = Vec::new();
    if shapes.int > 0 {
        arms.push((weight(shapes.int), any::<i64>().prop_map(int_value).boxed()));
    }
    if shapes.double > 0 {
        arms.push((
            weight(shapes.double),
            (-1.0e12..1.0e12).prop_map(double_value).boxed(),
        ));
    }
    if shapes.boolean > 0 {
        arms.push((
            weight(shapes.boolean),
            any::<bool>().prop_map(bool_value).boxed(),
        ));
    }
    if shapes.bytes > 0 {
        arms.push((
            weight(shapes.bytes),
            proptest::collection::vec(any::<u8>(), 0..32)
                .prop_map(bytes_value)
                .boxed(),
        ));
    }
    if shapes.array > 0 {
        arms.push((
            weight(shapes.array),
            proptest::collection::vec(leaf.clone(), 1..8)
                .prop_map(array_value)
                .boxed(),
        ));
    }
    if shapes.kvlist > 0 {
        arms.push((
            weight(shapes.kvlist),
            proptest::collection::vec(
                ((4usize..12).prop_flat_map(ascii_text), leaf.prop_map(Some)),
                1..8,
            )
            .prop_map(kvlist_value)
            .boxed(),
        ));
    }
    if arms.is_empty() {
        arms.push((1, any::<i64>().prop_map(int_value).boxed()));
    }
    Union::new_weighted(arms).prop_map(Body::Structured).boxed()
}

fn calibrated_attributes(
    count_histogram: &ExactHistogram,
    shapes: &AnyValueShapes,
    distinct_keys: u64,
    prefix: &'static str,
) -> BoxedStrategy<Vec<KeyValue>> {
    let count_arms: Vec<(u32, BoxedStrategy<usize>)> = if count_histogram.is_empty() {
        vec![(1, Just(0usize).boxed())]
    } else {
        count_histogram
            .iter()
            .map(|(&count, &records)| {
                // Clamp to the adversarial bound: a malformed or
                // saturated manifest must not turn into pathological
                // allocations in calibrated runs.
                let count = usize::try_from(count)
                    .unwrap_or(ADVERSARIAL_MAX_ATTRIBUTES)
                    .min(ADVERSARIAL_MAX_ATTRIBUTES);
                (weight(records), Just(count).boxed())
            })
            .collect()
    };
    // The key pool's size carries the manifest's cardinality signal,
    // clamped to keep generated stores dictionary-friendly.
    let pool = usize::try_from(distinct_keys).unwrap_or(64).clamp(1, 64);
    let value = calibrated_leaf(shapes);
    Union::new_weighted(count_arms)
        .prop_flat_map(move |count| {
            proptest::collection::vec(
                (0..pool, value.clone())
                    .prop_map(move |(i, v)| key_value(format!("{prefix}.k{i:02}"), Some(v))),
                count..=count,
            )
        })
        .boxed()
}

// ---- adversarial mode (RFC 0024 §3.2) -------------------------------

/// Records roaming the envelope's legal extremes: deep `AnyValue`
/// nesting (to [`MAX_ANY_VALUE_DEPTH`]), attribute maps to
/// [`ADVERSARIAL_MAX_ATTRIBUTES`] entries, empty/absent everything,
/// zero and `u64::MAX` timestamps, non-ASCII and confusable keys,
/// non-finite doubles, and text bodies past the miner's
/// `max_line_tokens` (the Collector-fronted-legacy shape).
pub fn adversarial() -> impl Strategy<Value = OtlpLogRecord> {
    let envelope = (
        adversarial_time(),
        proptest::option::of(adversarial_time()),
        0u8..=24,
        proptest::option::of(small_unicode_text()),
        proptest::option::of(small_unicode_text()),
        any::<u32>(),
        any::<u32>(),
    );
    let scope = (
        proptest::option::of(small_unicode_text()),
        proptest::option::of(small_unicode_text()),
        proptest::collection::vec(adversarial_key_value(), 0..4),
        proptest::option::of(small_unicode_text()),
        proptest::option::of(small_unicode_text()),
    );
    let payload = (
        adversarial_attributes(),
        adversarial_attributes(),
        proptest::option::of(correlation_id::<16>()),
        proptest::option::of(correlation_id::<8>()),
        adversarial_body(),
    );
    (envelope, scope, payload).prop_map(
        |(
            (time, observed, severity_number, severity_text, event_name, flags, dropped),
            (scope_name, scope_version, scope_attributes, resource_schema_url, scope_schema_url),
            (attributes, resource_attributes, trace_id, span_id, body),
        )| OtlpLogRecord {
            tenant_id: TenantId::new(TESTGEN_TENANT),
            time_unix_nano: time,
            observed_time_unix_nano: observed,
            severity_number,
            severity_text,
            scope_name,
            scope_version,
            scope_attributes,
            resource_schema_url,
            scope_schema_url,
            attributes,
            dropped_attributes_count: dropped,
            resource_attributes,
            trace_id,
            span_id,
            flags,
            event_name,
            body,
        },
    )
}

fn adversarial_time() -> BoxedStrategy<u64> {
    prop_oneof![
        1 => Just(0u64),
        1 => Just(u64::MAX),
        4 => TIME_BASE_UNIX_NANO..TIME_BASE_UNIX_NANO + TIME_WINDOW_NANO,
        2 => any::<u64>(),
    ]
    .boxed()
}

/// Attribute keys spanning plain identifiers, the empty string,
/// Unicode confusables, over-long keys, and arbitrary Unicode.
fn adversarial_key() -> BoxedStrategy<String> {
    prop_oneof![
        8 => (1usize..24).prop_flat_map(ascii_key),
        1 => Just(String::new()),
        // Leading Cyrillic 'а' — renders identically to the ASCII
        // "admin.key" but is a distinct key.
        1 => Just("\u{430}dmin.key".to_string()),
        1 => (200usize..300).prop_flat_map(ascii_key),
        2 => small_unicode_text(),
    ]
    .boxed()
}

fn ascii_key(len: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            20 => proptest::char::range('a', 'z'),
            3 => proptest::char::range('0', '9'),
            2 => Just('.'),
            1 => Just('_'),
        ],
        len..=len,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn small_unicode_text() -> BoxedStrategy<String> {
    proptest::collection::vec(any::<char>(), 0..16)
        .prop_map(|chars| chars.into_iter().collect())
        .boxed()
}

/// Scalar `AnyValue`s including the empty value, arbitrary Unicode
/// strings, the full `i64` range, non-finite doubles, and raw bytes.
fn adversarial_leaf() -> BoxedStrategy<AnyValue> {
    prop_oneof![
        1 => Just(AnyValue { value: None }),
        4 => proptest::collection::vec(any::<char>(), 0..32)
            .prop_map(|chars| string_value(chars.into_iter().collect())),
        3 => any::<i64>().prop_map(int_value),
        // `any::<f64>()` includes ±∞ and NaN — the canonical codec's
        // RFC 0018 §3.4 path must hold under generated trees too.
        3 => any::<f64>().prop_map(double_value),
        2 => any::<bool>().prop_map(bool_value),
        2 => proptest::collection::vec(any::<u8>(), 0..64).prop_map(bytes_value),
    ]
    .boxed()
}

/// Arbitrary `AnyValue` trees nested to the documented depth bound.
fn adversarial_any_value() -> BoxedStrategy<AnyValue> {
    adversarial_leaf()
        .prop_recursive(RECURSION_LEVELS, 96, 6, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..6).prop_map(array_value),
                proptest::collection::vec((adversarial_key(), proptest::option::of(inner)), 0..6,)
                    .prop_map(kvlist_value),
            ]
        })
        .boxed()
}

fn adversarial_key_value() -> BoxedStrategy<KeyValue> {
    (
        adversarial_key(),
        proptest::option::of(adversarial_any_value()),
    )
        .prop_map(|(k, v)| key_value(k, v))
        .boxed()
}

/// Attribute maps: mostly small, sometimes wide, occasionally the
/// full [`ADVERSARIAL_MAX_ATTRIBUTES`]-entry blowup (with cheap leaf
/// values — the blowup arm probes map *size*, not tree depth).
fn adversarial_attributes() -> BoxedStrategy<Vec<KeyValue>> {
    let cheap = (adversarial_key(), proptest::option::of(adversarial_leaf()))
        .prop_map(|(k, v)| key_value(k, v));
    prop_oneof![
        6 => proptest::collection::vec(adversarial_key_value(), 0..8),
        2 => proptest::collection::vec(adversarial_key_value(), 8..64),
        1 => proptest::collection::vec(cheap, 1024..=ADVERSARIAL_MAX_ATTRIBUTES),
    ]
    .boxed()
}

fn correlation_id<const N: usize>() -> BoxedStrategy<[u8; N]> {
    prop_oneof![
        // All-zero is the spec's "invalid id" sentinel — the pipeline
        // must carry it without treating it as absent.
        1 => Just([0u8; N]),
        8 => any::<[u8; N]>(),
    ]
    .boxed()
}

fn adversarial_body() -> BoxedStrategy<Option<Body>> {
    prop_oneof![
        1 => Just(None),
        1 => Just(Some(Body::String(String::new()))),
        // `Body::from_any_value` applies the receiver's §6.2 step-0
        // fork, so generated bodies never carry the illegal
        // `Structured(StringValue)` / `Structured(empty)` states.
        6 => adversarial_any_value().prop_map(Body::from_any_value),
        2 => over_long_text_body(),
    ]
    .boxed()
}

/// A text body with more whitespace-separated tokens than the
/// miner's default `max_line_tokens` — the shape RFC 0023 §3.2
/// diverts to the §6.3 parse-failure path.
fn over_long_text_body() -> BoxedStrategy<Option<Body>> {
    let floor = usize::from(MinerConfig::default().max_line_tokens) + 1;
    (floor..floor * 2, 0usize..97)
        .prop_map(|(tokens, seed)| {
            let words: Vec<String> = (0..tokens)
                .map(|i| format!("w{}", i.wrapping_mul(seed + 1)))
                .collect();
            Some(Body::String(words.join(" ")))
        })
        .boxed()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ourios_core::otlp::canonical;
    use proptest::test_runner::{Config, TestRunner};

    use super::*;
    use crate::manifest::{BodyKindMix, CalibrationAccumulator, SeverityBucket};

    fn run<S: Strategy>(
        cases: u32,
        strategy: &S,
        check: impl Fn(S::Value) -> Result<(), proptest::test_runner::TestCaseError>,
    ) {
        let mut runner = TestRunner::new(Config {
            cases,
            ..Config::default()
        });
        runner.run(strategy, check).unwrap();
    }

    /// The generator's own envelope: every documented bound in this
    /// module holds for every generated record, and the canonical
    /// codec accepts whatever the trees contain (non-finite doubles
    /// included).
    #[test]
    fn adversarial_records_stay_inside_the_documented_envelope() {
        run(96, &adversarial(), |record| {
            prop_assert!(record.severity_number <= 24);
            prop_assert!(record.attributes.len() <= ADVERSARIAL_MAX_ATTRIBUTES);
            prop_assert!(record.resource_attributes.len() <= ADVERSARIAL_MAX_ATTRIBUTES);
            prop_assert_eq!(record.tenant_id.as_str(), TESTGEN_TENANT);

            let mut probe = CalibrationAccumulator::new();
            probe.observe(&record);
            let depth = probe.finish("probe").any_value_max_depth;
            prop_assert!(
                depth <= MAX_ANY_VALUE_DEPTH,
                "generated depth {} exceeds the documented bound",
                depth
            );

            if let Some(Body::Structured(av)) = &record.body {
                prop_assert!(
                    !matches!(av.value, Some(any_value::Value::StringValue(_)) | None),
                    "structured bodies must not carry the receiver-forked states"
                );
                prop_assert!(canonical::encode_any_value(av).is_ok());
            }
            prop_assert!(canonical::encode_attributes(&record.attributes).is_ok());
            prop_assert!(canonical::encode_attributes(&record.resource_attributes).is_ok());
            Ok(())
        });
    }

    /// Calibrated generation draws only from the manifest's measured
    /// support: severity pairs, attribute counts, body kinds, and
    /// string-body length buckets. (RFC0024.2's *moment* tolerance
    /// runs over a real extracted manifest in `ourios-bench`.)
    #[test]
    fn calibrated_records_draw_from_the_manifest_support() {
        let manifest = CalibrationManifest {
            corpus_tag: "synthetic".to_string(),
            records: 10,
            log_attribute_count: BTreeMap::from([(1, 5), (3, 5)]),
            resource_attribute_count: BTreeMap::from([(2, 10)]),
            body_kind: BodyKindMix {
                string: 9,
                structured: 1,
                absent: 0,
            },
            // Bucket 5 = lengths 16..=31.
            string_body_len: BTreeMap::from([(5, 9)]),
            severity: vec![
                SeverityBucket {
                    number: 9,
                    text: Some("INFO".to_string()),
                    count: 8,
                },
                SeverityBucket {
                    number: 17,
                    text: Some("ERROR".to_string()),
                    count: 2,
                },
            ],
            any_value_shapes: AnyValueShapes {
                string: 20,
                int: 5,
                ..Default::default()
            },
            any_value_max_depth: 1,
            distinct_attribute_keys: 4,
        };
        run(128, &calibrated(&manifest), |record| {
            let text = record.severity_text.as_deref();
            prop_assert!(
                matches!(
                    (record.severity_number, text),
                    (9, Some("INFO")) | (17, Some("ERROR"))
                ),
                "severity ({}, {:?}) is outside the manifest's support",
                record.severity_number,
                text
            );
            prop_assert!(matches!(record.attributes.len(), 1 | 3));
            prop_assert_eq!(record.resource_attributes.len(), 2);
            match &record.body {
                Some(Body::String(s)) => {
                    prop_assert!(
                        (16..=31).contains(&s.len()),
                        "string body length {} outside the manifest's bucket",
                        s.len()
                    );
                }
                Some(Body::Structured(_)) => {}
                None => prop_assert!(false, "manifest has no absent bodies"),
            }
            Ok(())
        });
    }

    /// Structured-body scalars follow the manifest's shape weights:
    /// a bytes-only manifest must produce bytes-valued bodies, never
    /// the other scalars.
    #[test]
    fn calibrated_structured_scalars_respect_the_shape_support() {
        let manifest = CalibrationManifest {
            corpus_tag: "bytes-only".to_string(),
            records: 5,
            body_kind: BodyKindMix {
                string: 0,
                structured: 5,
                absent: 0,
            },
            any_value_shapes: AnyValueShapes {
                bytes: 5,
                ..Default::default()
            },
            any_value_max_depth: 1,
            ..Default::default()
        };
        run(64, &calibrated(&manifest), |record| {
            let Some(Body::Structured(av)) = &record.body else {
                return Err(proptest::test_runner::TestCaseError::fail(
                    "manifest admits only structured bodies",
                ));
            };
            prop_assert!(
                matches!(av.value, Some(any_value::Value::BytesValue(_))),
                "bytes-only manifest generated {:?}",
                av.value
            );
            Ok(())
        });
    }

    /// A saturated (or malformed) attribute-count histogram is
    /// clamped to the adversarial bound rather than driving
    /// pathological allocations.
    #[test]
    fn calibrated_attribute_counts_are_clamped() {
        let manifest = CalibrationManifest {
            corpus_tag: "saturated".to_string(),
            records: 1,
            log_attribute_count: BTreeMap::from([(u32::MAX, 1)]),
            ..Default::default()
        };
        run(4, &calibrated(&manifest), |record| {
            prop_assert_eq!(record.attributes.len(), ADVERSARIAL_MAX_ATTRIBUTES);
            Ok(())
        });
    }

    /// An empty (default) manifest still generates records — every
    /// calibrated distribution has a documented fallback.
    #[test]
    fn calibrated_tolerates_an_empty_manifest() {
        run(32, &calibrated(&CalibrationManifest::default()), |record| {
            prop_assert!(record.body.is_some());
            prop_assert!(record.attributes.is_empty());
            Ok(())
        });
    }

    /// The over-long-text arm really crosses the miner's default
    /// token cap, and the blowup arm really reaches four-digit maps.
    #[test]
    fn adversarial_extremes_are_reachable() {
        let cap = usize::from(MinerConfig::default().max_line_tokens);
        let saw_over_long = std::cell::Cell::new(false);
        let saw_wide_map = std::cell::Cell::new(false);
        run(256, &adversarial(), |record| {
            if let Some(Body::String(s)) = &record.body
                && s.split_whitespace().count() > cap
            {
                saw_over_long.set(true);
            }
            if record.attributes.len() >= 1024 {
                saw_wide_map.set(true);
            }
            Ok(())
        });
        assert!(
            saw_over_long.get(),
            "no generated body crossed max_line_tokens"
        );
        assert!(
            saw_wide_map.get(),
            "no generated attribute map reached the blowup arm"
        );
    }
}
