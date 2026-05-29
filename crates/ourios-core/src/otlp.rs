//! In-memory representation of an OTLP log record as Ourios sees it
//! after the receiver has decoded the wire bytes and derived the
//! per-`ResourceLogs` `tenant_id`. Faithful to RFC 0001 §6.1's
//! amended record schema and RFC 0003 §6.6's struct sketch.
//!
//! The proto types — `AnyValue`, `KeyValue` — live faithfully on this
//! struct (RFC 0003 §4.1 pins `opentelemetry-proto` as the
//! wire-stack default; this crate is just where they enter the
//! Ourios codebase). The wrapping struct exists to attach
//! `tenant_id` (an Ourios-derived field that has no spec-compliant
//! home on the OTLP wire — see RFC 0003 §6.3) and to flatten the
//! `ResourceLogs → ScopeLogs → LogRecord` nesting into the shape
//! the miner consumes.
//!
//! `tenant_id` is deliberately a sibling field, **not** folded into
//! `resource_attributes`. Per the `OTel` `Resource` spec, resource
//! attributes describe the *observed entity* (`service.name`,
//! `host.*`, `k8s.*`, …), not ingest-routing decisions; the
//! `otel.*` namespace is also reserved. Synthesising a
//! `tenant_id` attribute into `resource_attributes` would violate
//! both contracts. The receiver derives `tenant_id` from `Resource`
//! attributes per RFC 0003 §6.3 and attaches it here as a separate
//! field — leaving `resource_attributes` faithful to what the wire
//! delivered. (Auth-context-driven tenant binding is currently a
//! RFC 0003 §9 open question; if it lands, it joins the same
//! derivation rule rather than displacing it.)

use crate::tenant::TenantId;
// Re-export the proto types this module exposes on its public
// surface so downstream crates (the miner, the future receiver,
// the future Parquet writer) can use them without taking a
// direct `opentelemetry-proto` dependency. The proto types are
// still the canonical definitions — this is just a single import
// path through `ourios_core::otlp::*` so the dep graph stays one
// crate deep. `any_value` is also re-exported because callers
// constructing a non-string `AnyValue` need its `Value` enum to
// fill the `value: Option<any_value::Value>` field.
pub use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};

/// One OTLP `LogRecord` after wire decode and tenant derivation.
///
/// Fields mirror RFC 0001 §6.1's record schema. Optionality follows
/// Rust idiom (`Option<T>` for absence) rather than proto idiom
/// (empty string / empty `Vec<u8>` as sentinel for absence) — the
/// receiver narrows proto's any-of-many-sentinels into a single
/// `None` at the wire-decode boundary so downstream code never
/// has to re-derive presence.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OtlpLogRecord {
    /// Receiver-derived per RFC 0001 §6.1 *Tenant derivation* (one
    /// rule run per `ResourceLogs.resource`).
    pub tenant_id: TenantId,

    // OTLP-derived (per RFC 0001 §6.1 record schema)
    /// Source event time. `0` = unknown per the OTLP spec.
    pub time_unix_nano: u64,
    /// Collector observation time, when set.
    pub observed_time_unix_nano: Option<u64>,
    /// OTLP `SeverityNumber`: `0` = `UNSPECIFIED`, `1..=24` =
    /// TRACE..FATAL with sub-levels. Narrowed from proto's
    /// unbounded `i32` at the receiver boundary.
    pub severity_number: u8,
    /// Source's original severity string.
    pub severity_text: Option<String>,
    /// `InstrumentationScope.name` — emitter library/module.
    pub scope_name: Option<String>,
    /// `InstrumentationScope.version`.
    pub scope_version: Option<String>,
    /// Per-occurrence structured context.
    pub attributes: Vec<KeyValue>,
    /// Truncation indicator from the wire.
    pub dropped_attributes_count: u32,
    /// Source identity (`service.name`, `host.*`, `k8s.*`, …) —
    /// inherited from `Resource.attributes` and copied onto every
    /// record under that `ResourceLogs` group.
    pub resource_attributes: Vec<KeyValue>,
    /// Trace correlation, when set.
    pub trace_id: Option<[u8; 16]>,
    /// Span correlation, when set.
    pub span_id: Option<[u8; 8]>,
    /// Lower 8 bits = W3C trace flags.
    pub flags: u32,
    /// Identifier for structured-event records.
    pub event_name: Option<String>,

    /// `LogRecord.body` discriminated by the §6.2 step-0 fork.
    /// `None` when the wire delivered an absent body.
    pub body: Option<Body>,
}

/// The `body.kind` fork from RFC 0001 §6.2 step 0.
///
/// The `Structured` variant carries the decoded `AnyValue` rather
/// than its OTLP-canonical JSON encoding — canonicalisation is
/// deferred to the storage layer so the in-memory record stays
/// optionality-rich (a future "mine inner field" mode per
/// RFC 0001 §6.1 needs the structured tree, not pre-cached
/// bytes). RFC 0003 §6.4 carries the corresponding amendment.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// `LogRecord.body` was `AnyValue::String` — the unwrapped
    /// string is what the §6.2 algorithm tokenizes / masks /
    /// descends.
    String(String),
    /// `LogRecord.body` was any other `AnyValue` variant
    /// (kvlist, array, int, double, bool, bytes). Skips §6.2;
    /// the miner allocates or reuses the
    /// `(severity_number, scope_name, BodyKind::Structured)`
    /// sentinel template id per §6.1 *Template-key composition*.
    ///
    /// **Wire-export round-trip rule (RFC 0003 implementer note).**
    /// The *target* on-disk encoding for `MinedRecord.body` on
    /// `Structured` rows is OTLP-canonical JSON per RFC 0005 §3.3.
    /// The *current* miner implementation (in
    /// `ourios-miner::cluster::ingest_structured`) writes the
    /// `Debug` rendering of the decoded `AnyValue` (`format!(
    /// "{any_value:?}")`) as an interim placeholder — the
    /// canonicalisation PR replaces it before any wire-export
    /// path lands. Either way, the future OTLP exporter MUST
    /// decode the stored bytes back into the matching `AnyValue`
    /// variant (the `opentelemetry_proto::tonic::common::v1::
    /// any_value::Value` enum — `KvlistValue`, `ArrayValue`,
    /// `IntValue`, `DoubleValue`, `BoolValue`, `BytesValue`,
    /// `StringValue`) — *not* emit the stored bytes as
    /// `AnyValue::StringValue` carrying the raw text. The latter
    /// shortcut is lossy: receivers (e.g. Grafana / Loki) render
    /// `StringValue` as text rather than walking the structured
    /// tree, and "Body MUST support `AnyValue` to preserve the
    /// semantics of structured logs" (OpenTelemetry Logs Data
    /// Model §Body) is then violated end-to-end. RFC 0003 will
    /// pin this as part of the exporter contract.
    Structured(AnyValue),
}

/// Cheap routing flag the miner and (eventually) the query
/// planner use to decide whether reconstruction is defined for
/// this row. RFC 0001 §6.1 pins the variant set to two — not the
/// full `AnyValue` discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyKind {
    String,
    Structured,
}

impl OtlpLogRecord {
    /// Derive `BodyKind` from the body payload, returning `None`
    /// when the wire delivered no body. Cheap, allocation-free.
    #[must_use]
    pub fn body_kind(&self) -> Option<BodyKind> {
        self.body.as_ref().map(Body::kind)
    }
}

impl Body {
    /// The body's two-variant discriminator per RFC 0001 §6.1.
    #[must_use]
    pub fn kind(&self) -> BodyKind {
        match self {
            Self::String(_) => BodyKind::String,
            Self::Structured(_) => BodyKind::Structured,
        }
    }
}

/// Sugar for constructing a `Body` from a proto `AnyValue` —
/// returns `None` if the value is empty/absent, otherwise picks
/// the appropriate variant. Lives here (not in the receiver
/// crate) so the String/Structured fork is defined exactly once
/// and miner-side tests can construct the enum without hand-rolling
/// the same match.
impl Body {
    /// Build a `Body` from a proto `AnyValue`.
    ///
    /// Returns `None` when the `AnyValue` carries no inner value
    /// (proto's `oneof` is unset). The String / Structured fork
    /// is exactly the §6.2 step-0 split: only `string_value`
    /// takes the mining path; everything else takes the
    /// short-circuit path. The inner `oneof` is *moved* into the
    /// chosen variant — no deep clone of arrays / kvlists / bytes
    /// trees, satisfying the amended §6.4 commitment that the
    /// structured branch does not allocate on the miner-facing
    /// path.
    #[must_use]
    pub fn from_any_value(value: AnyValue) -> Option<Self> {
        let inner = value.value?;
        match inner {
            any_value::Value::StringValue(s) => Some(Self::String(s)),
            other => Some(Self::Structured(AnyValue { value: Some(other) })),
        }
    }
}

/// RFC 0005 §3.3 OTLP-canonical-JSON encoding for the columns
/// the writer stores as `BYTE_ARRAY`: `attributes`,
/// `resource_attributes`, and the `body` column for
/// `body_kind = Structured`.
///
/// The "canonical" rule is the proto3 JSON mapping plus OTLP's
/// specific overrides (camelCase fields, `string`-encoded
/// `uint64`s, base64 for `bytes`, etc.). The
/// `opentelemetry-proto` crate's `with-serde` feature already
/// implements that spec on its proto types — these helpers are
/// thin wrappers so callers don't reach for `serde_json`
/// directly and the spec mapping stays single-sourced through
/// `opentelemetry-proto`. The same pattern rotel's OTLP HTTP
/// receiver uses on `ExportLogsServiceRequest`.
///
/// Encoders are stable per-`AnyValue`-tree: serde derives have
/// a fixed field order and `serde_json` is deterministic, so
/// re-encoding the same in-memory tree produces byte-identical
/// output across runs — required by RFC0006.7 reproducibility.
///
/// **Note on "canonical".** RFC 0005 §3.3 uses "canonical" to
/// mean "the single normative encoding the writer / reader
/// agree on," **not** the RFC 8785 canonical-JSON form (sorted
/// keys, normalised numbers). The two are compatible for our
/// purposes because struct field order is fixed by proto and
/// the encode → store → decode round-trip is asserted at the
/// `AnyValue` / `Vec<KeyValue>` level (not on bytes).
pub mod canonical {
    use super::{AnyValue, KeyValue};

    /// Error returned by the canonical encoders / decoders.
    /// Encoders are infallible in practice on values the
    /// receiver produces; the `Encode` arm survives for
    /// pathological inputs (e.g. a `DoubleValue` carrying
    /// `f64::NAN` — proto allows it, `serde_json` rejects it).
    /// Decoders fan in malformed-bytes errors from disk.
    #[derive(Debug)]
    pub enum CanonicalJsonError {
        Encode(serde_json::Error),
        Decode(serde_json::Error),
    }

    impl core::fmt::Display for CanonicalJsonError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::Encode(e) => write!(f, "OTLP-canonical JSON encode: {e}"),
                Self::Decode(e) => write!(f, "OTLP-canonical JSON decode: {e}"),
            }
        }
    }

    impl std::error::Error for CanonicalJsonError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Self::Encode(e) | Self::Decode(e) => Some(e),
            }
        }
    }

    /// Encode one `AnyValue` to its OTLP-canonical JSON bytes.
    /// Used by the writer / miner for `body_kind = Structured`
    /// rows (the `body` column stores these bytes).
    ///
    /// # Errors
    ///
    /// [`CanonicalJsonError::Encode`] on pathological inputs the
    /// receiver should narrow at wire-decode (e.g. a
    /// `DoubleValue` carrying `f64::NAN`).
    pub fn encode_any_value(value: &AnyValue) -> Result<Vec<u8>, CanonicalJsonError> {
        serde_json::to_vec(value).map_err(CanonicalJsonError::Encode)
    }

    /// Inverse of [`encode_any_value`]. Used by the reader to
    /// recover the structured `AnyValue` from its stored bytes.
    ///
    /// # Errors
    ///
    /// [`CanonicalJsonError::Decode`] on malformed bytes (file
    /// corruption or a foreign producer that doesn't honour the
    /// §3.3 spec).
    pub fn decode_any_value(bytes: &[u8]) -> Result<AnyValue, CanonicalJsonError> {
        serde_json::from_slice(bytes).map_err(CanonicalJsonError::Decode)
    }

    /// Encode a `Vec<KeyValue>` (the in-memory shape of
    /// `attributes` / `resource_attributes`) to its
    /// OTLP-canonical JSON bytes. Stored verbatim in the
    /// matching `BYTE_ARRAY` column by the writer.
    ///
    /// # Errors
    ///
    /// [`CanonicalJsonError::Encode`] under the same conditions
    /// as [`encode_any_value`] — a `KeyValue.value`'s underlying
    /// `AnyValue` rejecting JSON serialisation.
    pub fn encode_attributes(attrs: &[KeyValue]) -> Result<Vec<u8>, CanonicalJsonError> {
        serde_json::to_vec(attrs).map_err(CanonicalJsonError::Encode)
    }

    /// Inverse of [`encode_attributes`]. The reader uses this
    /// to recover the `Vec<KeyValue>` from a non-empty stored
    /// column.
    ///
    /// # Errors
    ///
    /// [`CanonicalJsonError::Decode`] on malformed bytes.
    pub fn decode_attributes(bytes: &[u8]) -> Result<Vec<KeyValue>, CanonicalJsonError> {
        serde_json::from_slice(bytes).map_err(CanonicalJsonError::Decode)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::otlp::any_value;

        fn string_av(s: &str) -> AnyValue {
            AnyValue {
                value: Some(any_value::Value::StringValue(s.to_string())),
            }
        }

        fn int_av(n: i64) -> AnyValue {
            AnyValue {
                value: Some(any_value::Value::IntValue(n)),
            }
        }

        /// Encode → decode round-trips an `AnyValue` for every
        /// `Value` variant the receiver might hand the storage
        /// layer. The serde derives are spec-compliant; this
        /// pins the round-trip property the §3.3 reconstruction
        /// guarantee depends on.
        #[test]
        fn any_value_round_trips_across_variants() {
            for av in [
                string_av("hello world"),
                int_av(-42),
                AnyValue {
                    value: Some(any_value::Value::DoubleValue(2.71_f64)),
                },
                AnyValue {
                    value: Some(any_value::Value::BoolValue(true)),
                },
                AnyValue {
                    value: Some(any_value::Value::BytesValue(b"raw\x00bytes".to_vec())),
                },
                AnyValue {
                    value: Some(any_value::Value::ArrayValue(
                        opentelemetry_proto::tonic::common::v1::ArrayValue {
                            values: vec![string_av("a"), int_av(1)],
                        },
                    )),
                },
                AnyValue {
                    value: Some(any_value::Value::KvlistValue(
                        opentelemetry_proto::tonic::common::v1::KeyValueList {
                            values: vec![KeyValue {
                                key: "k".to_string(),
                                value: Some(string_av("v")),
                                ..Default::default()
                            }],
                        },
                    )),
                },
            ] {
                let bytes = encode_any_value(&av).expect("encode");
                let back = decode_any_value(&bytes).expect("decode");
                assert_eq!(
                    av,
                    back,
                    "round-trip failed for {av:?}; bytes = {}",
                    String::from_utf8_lossy(&bytes),
                );
            }
        }

        /// `Vec<KeyValue>` round-trips at the
        /// `attributes` / `resource_attributes` column boundary.
        #[test]
        fn attributes_round_trip() {
            let attrs = vec![
                KeyValue {
                    key: "service.name".to_string(),
                    value: Some(string_av("bench-app")),
                    ..Default::default()
                },
                KeyValue {
                    key: "user.id".to_string(),
                    value: Some(int_av(42)),
                    ..Default::default()
                },
            ];
            let bytes = encode_attributes(&attrs).expect("encode");
            let back = decode_attributes(&bytes).expect("decode");
            assert_eq!(attrs, back);
        }

        /// Re-encoding the same in-memory tree must produce
        /// byte-identical bytes — RFC0006.7's reproducibility
        /// requirement carries through the canonicalisation
        /// boundary.
        #[test]
        fn encoder_is_deterministic_across_calls() {
            let av = AnyValue {
                value: Some(any_value::Value::KvlistValue(
                    opentelemetry_proto::tonic::common::v1::KeyValueList {
                        values: vec![
                            KeyValue {
                                key: "alpha".to_string(),
                                value: Some(int_av(1)),
                                ..Default::default()
                            },
                            KeyValue {
                                key: "beta".to_string(),
                                value: Some(string_av("two")),
                                ..Default::default()
                            },
                        ],
                    },
                )),
            };
            let a = encode_any_value(&av).expect("first encode");
            let b = encode_any_value(&av).expect("second encode");
            assert_eq!(
                a, b,
                "encoder must be byte-deterministic for the same input"
            );
        }

        /// Empty attribute lists encode to a sentinel that
        /// decodes back to an empty `Vec`. The writer special-
        /// cases the empty case (no row-level allocation), but
        /// the helper itself round-trips on the trivial input
        /// for symmetry.
        #[test]
        fn empty_attributes_round_trip() {
            let bytes = encode_attributes(&[]).expect("encode");
            assert_eq!(bytes, b"[]");
            let back = decode_attributes(&bytes).expect("decode");
            assert!(back.is_empty());
        }
    }
}

impl Default for TenantId {
    /// Empty-tenant default exists so `OtlpLogRecord::default()`
    /// works in tests; production receivers always derive a
    /// tenant per RFC 0001 §6.1 and overwrite this.
    fn default() -> Self {
        Self::new(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::any_value::Value as AvValue;

    fn av_string(s: &str) -> AnyValue {
        AnyValue {
            value: Some(AvValue::StringValue(s.to_string())),
        }
    }

    fn av_int(i: i64) -> AnyValue {
        AnyValue {
            value: Some(AvValue::IntValue(i)),
        }
    }

    fn av_empty() -> AnyValue {
        AnyValue { value: None }
    }

    #[test]
    fn body_from_any_value_string_takes_mining_path() {
        // Arrange
        let av = av_string("hello world");

        // Act
        let body = Body::from_any_value(av);

        // Assert — String variant unwrapped, kind == String.
        assert_eq!(body, Some(Body::String("hello world".to_string())));
        assert_eq!(body.unwrap().kind(), BodyKind::String);
    }

    #[test]
    fn body_from_any_value_non_string_takes_structured_path() {
        // Arrange — int is the smallest non-String AnyValue.
        let av = av_int(42);

        // Act
        let body = Body::from_any_value(av.clone());

        // Assert — Structured variant carries the original AnyValue
        // verbatim; the §6.2 step-0 fork goes to the structured
        // template id.
        assert_eq!(body, Some(Body::Structured(av)));
        assert_eq!(body.unwrap().kind(), BodyKind::Structured);
    }

    #[test]
    fn body_from_any_value_returns_none_for_empty_value() {
        // Arrange — proto's oneof is unset (the AnyValue exists
        // but carries no inner value). Treated as "no body" by the
        // §6.2 fork.

        // Act
        let body = Body::from_any_value(av_empty());

        // Assert
        assert_eq!(body, None);
    }

    #[test]
    fn record_body_kind_returns_none_when_body_absent() {
        // Arrange — record with no body at all (the wire delivered
        // `LogRecord.body = None`).
        let r = OtlpLogRecord::default();

        // Act
        let kind = r.body_kind();

        // Assert
        assert_eq!(r.body, None);
        assert_eq!(kind, None);
    }

    #[test]
    fn record_body_kind_classifies_string_body() {
        // Arrange
        let r = OtlpLogRecord {
            body: Some(Body::String("x".to_string())),
            ..Default::default()
        };

        // Act
        let kind = r.body_kind();

        // Assert
        assert_eq!(kind, Some(BodyKind::String));
    }

    #[test]
    fn record_body_kind_classifies_structured_body() {
        // Arrange
        let r = OtlpLogRecord {
            body: Some(Body::Structured(av_int(1))),
            ..Default::default()
        };

        // Act
        let kind = r.body_kind();

        // Assert
        assert_eq!(kind, Some(BodyKind::Structured));
    }

    #[test]
    fn record_default_constructs_empty_tenant_and_zeroed_otlp_fields() {
        // Arrange + Act
        let r = OtlpLogRecord::default();

        // Assert — Default is *only* for test ergonomics; pin the
        // important defaults so a refactor can't silently change
        // them out from under tests.
        assert_eq!(r.tenant_id.as_str(), "");
        assert_eq!(r.time_unix_nano, 0);
        assert_eq!(r.observed_time_unix_nano, None);
        assert_eq!(r.severity_number, 0); // UNSPECIFIED — distinct key bucket per §6.1
        assert_eq!(r.severity_text, None);
        assert_eq!(r.scope_name, None);
        assert_eq!(r.scope_version, None);
        assert!(r.attributes.is_empty());
        assert_eq!(r.dropped_attributes_count, 0);
        assert!(r.resource_attributes.is_empty());
        assert_eq!(r.trace_id, None);
        assert_eq!(r.span_id, None);
        assert_eq!(r.flags, 0);
        assert_eq!(r.event_name, None);
        assert_eq!(r.body, None);
    }
}
