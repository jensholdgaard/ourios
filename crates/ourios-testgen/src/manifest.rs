//! RFC 0024 §3.1 calibration manifests.
//!
//! A manifest is a small, committed distribution summary extracted
//! from a real corpus release (`testdata/calibration/<tag>.json`),
//! so [`crate::strategies::calibrated`] is shaped by measured
//! reality rather than guesses. It is a *measurement*: regenerating
//! it over the same corpus is deterministic (RFC0024.1 pins the
//! rerun byte-identical), which is why every map below is a
//! `BTreeMap` and serialization goes through [`CalibrationManifest::to_json_bytes`].

use std::collections::{BTreeMap, BTreeSet};

use ourios_core::otlp::{AnyValue, Body, KeyValue, OtlpLogRecord, any_value};
use serde::{Deserialize, Serialize};

/// Exact value → record-count histogram, for small-integer
/// quantities (per-record attribute counts) where every distinct
/// value is worth keeping.
pub type ExactHistogram = BTreeMap<u32, u64>;

/// Power-of-two bucketed histogram for wide-range quantities (body
/// byte length). Key is the bucket index: `0` holds the value `0`,
/// index `b > 0` holds values in `2^(b-1) ..= 2^b - 1` (i.e. the
/// value's bit length).
pub type Log2Histogram = BTreeMap<u32, u64>;

/// The bucket index of `value` in a [`Log2Histogram`].
#[must_use]
pub fn log2_bucket(value: u64) -> u32 {
    match value.checked_ilog2() {
        Some(b) => b + 1,
        None => 0,
    }
}

/// The inclusive value range a [`Log2Histogram`] bucket covers.
#[must_use]
pub fn log2_bucket_range(bucket: u32) -> (u64, u64) {
    match bucket {
        0 => (0, 0),
        64.. => (1 << 63, u64::MAX),
        b => (1 << (b - 1), (1 << b) - 1),
    }
}

/// How many records carried each body shape (`body_kind` mix plus
/// the absent case, which `body_kind` itself cannot represent).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyKindMix {
    pub string: u64,
    pub structured: u64,
    pub absent: u64,
}

/// One `(severity_number, severity_text)` combination and how many
/// records carried it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeverityBucket {
    pub number: u8,
    pub text: Option<String>,
    pub count: u64,
}

/// How often each `AnyValue` variant occurred across every value
/// tree observed (attribute values and structured bodies), counting
/// interior `array` / `kvlist` nodes as occurrences too.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnyValueShapes {
    /// Proto `oneof` unset — the empty `AnyValue`.
    pub empty: u64,
    pub string: u64,
    pub int: u64,
    pub double: u64,
    pub boolean: u64,
    pub bytes: u64,
    pub array: u64,
    pub kvlist: u64,
}

/// The RFC 0024 §3.1 distribution summary for one corpus release.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalibrationManifest {
    /// The corpus release this manifest summarises (e.g.
    /// `otel-demo-v7`). A measurement is meaningless without its
    /// subject.
    pub corpus_tag: String,
    /// Records observed.
    pub records: u64,
    /// Per-record log-attribute count distribution.
    pub log_attribute_count: ExactHistogram,
    /// Per-record resource-attribute count distribution.
    pub resource_attribute_count: ExactHistogram,
    /// Body-shape mix across records.
    pub body_kind: BodyKindMix,
    /// String-body byte-length distribution (power-of-two buckets).
    pub string_body_len: Log2Histogram,
    /// `(severity_number, severity_text)` distribution, ordered by
    /// `(number, text)`.
    pub severity: Vec<SeverityBucket>,
    /// `AnyValue` variant frequencies across all observed value trees.
    pub any_value_shapes: AnyValueShapes,
    /// Deepest `AnyValue` nesting observed (`1` = a bare leaf).
    pub any_value_max_depth: u32,
    /// Distinct attribute keys across log + resource attributes —
    /// the cardinality signal.
    pub distinct_attribute_keys: u64,
}

impl CalibrationManifest {
    /// Serialize to the committed-file form: pretty JSON plus a
    /// trailing newline. Field order is fixed by the struct and
    /// every map is ordered, so the same manifest always produces
    /// the same bytes (RFC0024.1).
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] — in principle never for this tree of
    /// maps, strings, and integers; kept for API honesty.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Parse the committed-file form.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] on malformed bytes.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// Streaming accumulator behind `--calibrate`: feed it every record
/// of a corpus, then [`finish`](Self::finish) into the manifest.
#[derive(Debug, Default)]
pub struct CalibrationAccumulator {
    records: u64,
    log_attribute_count: ExactHistogram,
    resource_attribute_count: ExactHistogram,
    body_kind: BodyKindMix,
    string_body_len: Log2Histogram,
    severity: BTreeMap<(u8, Option<String>), u64>,
    shapes: AnyValueShapes,
    max_depth: u32,
    attribute_keys: BTreeSet<String>,
}

impl CalibrationAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one record into the running distributions.
    pub fn observe(&mut self, record: &OtlpLogRecord) {
        self.records += 1;

        *self
            .log_attribute_count
            .entry(saturating_u32(record.attributes.len()))
            .or_insert(0) += 1;
        *self
            .resource_attribute_count
            .entry(saturating_u32(record.resource_attributes.len()))
            .or_insert(0) += 1;

        match &record.body {
            None => self.body_kind.absent += 1,
            Some(Body::String(s)) => {
                self.body_kind.string += 1;
                *self
                    .string_body_len
                    .entry(log2_bucket(saturating_u64(s.len())))
                    .or_insert(0) += 1;
            }
            Some(Body::Structured(av)) => {
                self.body_kind.structured += 1;
                self.observe_any_value(av, 1);
            }
        }

        *self
            .severity
            .entry((record.severity_number, record.severity_text.clone()))
            .or_insert(0) += 1;

        self.observe_attributes(&record.attributes);
        self.observe_attributes(&record.resource_attributes);
    }

    fn observe_attributes(&mut self, attrs: &[KeyValue]) {
        for kv in attrs {
            if !self.attribute_keys.contains(&kv.key) {
                self.attribute_keys.insert(kv.key.clone());
            }
            if let Some(av) = &kv.value {
                self.observe_any_value(av, 1);
            }
        }
    }

    fn observe_any_value(&mut self, av: &AnyValue, depth: u32) {
        self.max_depth = self.max_depth.max(depth);
        match &av.value {
            None => self.shapes.empty += 1,
            // `StringValueStrindex` is opentelemetry-proto's experimental
            // string-dictionary reference — still a string, shape-wise.
            Some(any_value::Value::StringValue(_) | any_value::Value::StringValueStrindex(_)) => {
                self.shapes.string += 1;
            }
            Some(any_value::Value::IntValue(_)) => self.shapes.int += 1,
            Some(any_value::Value::DoubleValue(_)) => self.shapes.double += 1,
            Some(any_value::Value::BoolValue(_)) => self.shapes.boolean += 1,
            Some(any_value::Value::BytesValue(_)) => self.shapes.bytes += 1,
            Some(any_value::Value::ArrayValue(array)) => {
                self.shapes.array += 1;
                for v in &array.values {
                    self.observe_any_value(v, depth + 1);
                }
            }
            Some(any_value::Value::KvlistValue(kvlist)) => {
                self.shapes.kvlist += 1;
                for kv in &kvlist.values {
                    if let Some(v) = &kv.value {
                        self.observe_any_value(v, depth + 1);
                    }
                }
            }
        }
    }

    /// Close the accumulation into the manifest for `corpus_tag`.
    #[must_use]
    pub fn finish(self, corpus_tag: &str) -> CalibrationManifest {
        CalibrationManifest {
            corpus_tag: corpus_tag.to_string(),
            records: self.records,
            log_attribute_count: self.log_attribute_count,
            resource_attribute_count: self.resource_attribute_count,
            body_kind: self.body_kind,
            string_body_len: self.string_body_len,
            severity: self
                .severity
                .into_iter()
                .map(|((number, text), count)| SeverityBucket {
                    number,
                    text,
                    count,
                })
                .collect(),
            any_value_shapes: self.shapes,
            any_value_max_depth: self.max_depth,
            distinct_attribute_keys: saturating_u64(self.attribute_keys.len()),
        }
    }
}

/// `usize → u32` clamped — an attribute count past `u32::MAX` is not
/// a distribution worth distinguishing further.
fn saturating_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `usize → u64` clamped (lossless on every supported target; the
/// clamp only exists so the conversion is total).
fn saturating_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ourios_core::otlp::{ArrayValue, KeyValueList};

    fn string_av(s: &str) -> AnyValue {
        AnyValue {
            value: Some(any_value::Value::StringValue(s.to_string())),
        }
    }

    fn kv(key: &str, value: AnyValue) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(value),
            ..Default::default()
        }
    }

    #[test]
    fn log2_buckets_partition_the_value_space() {
        assert_eq!(log2_bucket(0), 0);
        assert_eq!(log2_bucket(1), 1);
        assert_eq!(log2_bucket(2), 2);
        assert_eq!(log2_bucket(3), 2);
        assert_eq!(log2_bucket(4), 3);
        assert_eq!(log2_bucket(u64::MAX), 64);
        for bucket in [0, 1, 2, 3, 17, 63, 64] {
            let (lo, hi) = log2_bucket_range(bucket);
            assert_eq!(log2_bucket(lo), bucket, "lower bound of bucket {bucket}");
            assert_eq!(log2_bucket(hi), bucket, "upper bound of bucket {bucket}");
        }
    }

    #[test]
    fn accumulator_measures_the_documented_distributions() {
        let mut acc = CalibrationAccumulator::new();

        // Two INFO records with a string body and one attribute each
        // (same key), one ERROR record with a structured body nested
        // two deep and two resource attributes.
        for _ in 0..2 {
            acc.observe(&OtlpLogRecord {
                severity_number: 9,
                severity_text: Some("INFO".to_string()),
                attributes: vec![kv("shared.key", string_av("v"))],
                body: Some(Body::String("abcd".to_string())),
                ..Default::default()
            });
        }
        acc.observe(&OtlpLogRecord {
            severity_number: 17,
            severity_text: Some("ERROR".to_string()),
            resource_attributes: vec![
                kv("service.name", string_av("svc")),
                kv("host.name", string_av("h")),
            ],
            body: Some(Body::Structured(AnyValue {
                value: Some(any_value::Value::ArrayValue(ArrayValue {
                    values: vec![string_av("inner")],
                })),
            })),
            ..Default::default()
        });

        let m = acc.finish("unit-corpus");

        assert_eq!(m.corpus_tag, "unit-corpus");
        assert_eq!(m.records, 3);
        assert_eq!(m.log_attribute_count, BTreeMap::from([(0, 1), (1, 2)]));
        assert_eq!(m.resource_attribute_count, BTreeMap::from([(0, 2), (2, 1)]));
        assert_eq!(
            m.body_kind,
            BodyKindMix {
                string: 2,
                structured: 1,
                absent: 0,
            }
        );
        // "abcd" is 4 bytes → bucket 3 (values 4..=7).
        assert_eq!(m.string_body_len, BTreeMap::from([(3, 2)]));
        assert_eq!(
            m.severity,
            vec![
                SeverityBucket {
                    number: 9,
                    text: Some("INFO".to_string()),
                    count: 2,
                },
                SeverityBucket {
                    number: 17,
                    text: Some("ERROR".to_string()),
                    count: 1,
                },
            ]
        );
        // Attribute values: 2× shared.key string + 2 resource strings;
        // structured body: 1 array node + 1 string inside.
        assert_eq!(m.any_value_shapes.string, 5);
        assert_eq!(m.any_value_shapes.array, 1);
        assert_eq!(m.any_value_max_depth, 2);
        // shared.key (deduplicated) + service.name + host.name.
        assert_eq!(m.distinct_attribute_keys, 3);
    }

    #[test]
    fn accumulator_walks_kvlist_depth() {
        let mut acc = CalibrationAccumulator::new();
        acc.observe(&OtlpLogRecord {
            attributes: vec![kv(
                "outer",
                AnyValue {
                    value: Some(any_value::Value::KvlistValue(KeyValueList {
                        values: vec![KeyValue {
                            key: "inner".to_string(),
                            value: Some(AnyValue {
                                value: Some(any_value::Value::ArrayValue(ArrayValue {
                                    values: vec![string_av("leaf")],
                                })),
                            }),
                            ..Default::default()
                        }],
                    })),
                },
            )],
            ..Default::default()
        });
        let m = acc.finish("depth");
        assert_eq!(m.any_value_max_depth, 3);
        assert_eq!(m.any_value_shapes.kvlist, 1);
        assert_eq!(m.any_value_shapes.array, 1);
        assert_eq!(m.any_value_shapes.string, 1);
        // Only top-level attribute keys count toward cardinality —
        // kvlist-interior keys are values, not attribute keys.
        assert_eq!(m.distinct_attribute_keys, 1);
    }

    #[test]
    fn manifest_round_trips_and_serializes_deterministically() {
        let mut acc = CalibrationAccumulator::new();
        acc.observe(&OtlpLogRecord {
            severity_number: 13,
            severity_text: Some("WARN".to_string()),
            attributes: vec![kv("a", string_av("x")), kv("b", string_av("y"))],
            body: Some(Body::String("hello".to_string())),
            ..Default::default()
        });
        let m = acc.finish("round-trip");

        let first = m.to_json_bytes().expect("serialize");
        let second = m.to_json_bytes().expect("serialize again");
        assert_eq!(first, second, "same manifest must produce the same bytes");
        assert_eq!(
            first.last(),
            Some(&b'\n'),
            "committed-file form ends in a newline"
        );

        let back = CalibrationManifest::from_json_bytes(&first).expect("parse");
        assert_eq!(m, back);
    }
}
