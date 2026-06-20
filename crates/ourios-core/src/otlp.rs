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
pub use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, KeyValue, KeyValueList, any_value,
};

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
    /// `InstrumentationScope.attributes` — the scope's own attribute set
    /// (RFC 0018 §3.1). Distinct from the log record's `attributes`.
    pub scope_attributes: Vec<KeyValue>,
    /// `ResourceLogs.schema_url` — the Telemetry Schema URL of the resource
    /// group, when set (RFC 0018 §3.1).
    pub resource_schema_url: Option<String>,
    /// `ScopeLogs.schema_url` — the Telemetry Schema URL of the scope group,
    /// when set (RFC 0018 §3.1).
    pub scope_schema_url: Option<String>,
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
/// than its Ourios-canonical JSON encoding — the receiver hands the
/// miner the tree verbatim, and canonicalisation happens once, at
/// ingest, inside the miner (RFC 0003 §6.4). Keeping the tree on the
/// in-memory record is what preserves a future "mine inner field"
/// mode (RFC 0001 §6.1): any such hook runs before the encode.
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
    /// The on-disk encoding for `MinedRecord.body` on `Structured`
    /// rows is the Ourios-canonical JSON per RFC 0005 §3.3, produced
    /// by `ourios-miner::cluster::ingest_structured` via this
    /// module's `canonical::encode_any_value`. The future OTLP
    /// exporter MUST
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

/// RFC 0005 §3.3 Ourios-canonical-JSON encoding for the columns
/// the writer stores as `BYTE_ARRAY`: `attributes`,
/// `resource_attributes`, and the `body` column for
/// `body_kind = Structured`.
///
/// The "canonical" rule is the proto3 JSON mapping plus OTLP's
/// specific overrides (camelCase fields, `int64`s as decimal
/// strings, base64 for `bytes`, etc.). The
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
/// **`f64` exactness (#130).** The encode side already emits
/// shortest-round-trip digits (doubles go through `serde_json`'s
/// Ryu `f64` serializer). The lossy half was **decode**:
/// `serde_json`'s default float parsing is approximate, drifting
/// `decode(encode(x))` by 1–2 ULP for ~12% of arbitrary finite
/// `f64`. This crate therefore requires `serde_json`'s
/// `float_roundtrip` feature (declared in `Cargo.toml`), which
/// makes float parsing correctly rounded and the finite-`f64`
/// round-trip bit-exact — the RFC 0001 §6.1 faithfulness
/// guarantee (`lossy_flag = false`) rests on it. Pinned by the
/// `finite_doubles_round_trip_bit_exact` property test below.
/// Non-finite doubles (`NaN`, ±∞) have no JSON-number form, so
/// `serde_json` alone would render them as `null` (lossy). The
/// canonical codec detects a non-finite double and routes it
/// through a guarded path that emits **and** decodes the proto3-
/// JSON string forms `"NaN"`/`"Infinity"`/`"-Infinity"`, so they
/// round-trip (RFC 0018 §3.4); the finite case stays on the exact
/// serde above. Pinned by the `nonfinite_doubles_round_trip_via_proto3_strings`
/// test below.
///
/// **Note on "canonical".** RFC 0005 §3.3 uses "canonical" to
/// mean "the single normative encoding the writer / reader
/// agree on," **not** the RFC 8785 canonical-JSON form (sorted
/// keys, normalised numbers). The two are compatible for our
/// purposes because struct field order is fixed by proto and
/// the encode → store → decode round-trip is asserted at the
/// `AnyValue` / `Vec<KeyValue>` level (not on bytes).
pub mod canonical {
    use super::{AnyValue, ArrayValue, KeyValue, KeyValueList, any_value};

    /// Error returned by the canonical encoders / decoders.
    /// Encoders are infallible on every `AnyValue` /
    /// `Vec<KeyValue>` the type system admits —
    /// `opentelemetry-proto`'s `with-serde` ships custom
    /// serializers for the proto3-JSON oddities (`i64` →
    /// decimal string, `bytes` → base64), and the canonical codec
    /// handles non-finite `f64` itself via a guarded path emitting
    /// the proto3 `"NaN"`/`"Infinity"`/`"-Infinity"` string forms
    /// (RFC 0018 §3.4), so the recursive primitives never
    /// panic or return an error. The `Encode` arm exists only
    /// for `Result`-symmetry with `Decode` and as a defence-
    /// in-depth surface if a future `opentelemetry-proto`
    /// release changes that contract. Decoders fan in
    /// malformed-bytes errors from disk.
    #[derive(Debug)]
    pub enum CanonicalJsonError {
        Encode(serde_json::Error),
        Decode(serde_json::Error),
    }

    impl core::fmt::Display for CanonicalJsonError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Self::Encode(e) => write!(f, "Ourios-canonical JSON encode: {e}"),
                Self::Decode(e) => write!(f, "Ourios-canonical JSON decode: {e}"),
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

    /// Encode one `AnyValue` to its Ourios-canonical JSON bytes.
    /// Used by the writer / miner for `body_kind = Structured`
    /// rows (the `body` column stores these bytes).
    ///
    /// # Errors
    ///
    /// In principle never; see the
    /// [`CanonicalJsonError`] doc. The `Result` is kept for
    /// API symmetry with [`decode_any_value`] and as a
    /// forward-compat surface if a future
    /// `opentelemetry-proto` release adds a fallible
    /// serializer.
    pub fn encode_any_value(value: &AnyValue) -> Result<Vec<u8>, CanonicalJsonError> {
        // Fast path (the common case): `opentelemetry-proto`'s `with-serde`
        // is exact for finite `AnyValue`s. Only when a non-finite double is
        // present do we route through the robust encoder, which emits the
        // proto3-JSON string forms (`"NaN"`/`"Infinity"`/`"-Infinity"`) that
        // `serde_json` would otherwise lossily render as `null` (RFC 0018
        // §3.4).
        if has_nonfinite(value) {
            serde_json::to_vec(&av_to_json(value)).map_err(CanonicalJsonError::Encode)
        } else {
            serde_json::to_vec(value).map_err(CanonicalJsonError::Encode)
        }
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
        match serde_json::from_slice::<AnyValue>(bytes) {
            Ok(av) => Ok(av),
            // `opentelemetry-proto`'s deserializer rejects the proto3-JSON
            // non-finite string forms ("invalid type: string, expected
            // f64"). Re-parse as a generic JSON tree and convert ourselves,
            // honouring `"NaN"`/`"Infinity"`/`"-Infinity"` (RFC 0018 §3.4).
            // Genuinely corrupt bytes fail the generic parse too → original
            // error.
            Err(fast) => {
                let json = serde_json::from_slice::<serde_json::Value>(bytes)
                    .map_err(|_| CanonicalJsonError::Decode(fast))?;
                json_to_av(&json).map_err(CanonicalJsonError::Decode)
            }
        }
    }

    /// Encode a `Vec<KeyValue>` (the in-memory shape of
    /// `attributes` / `resource_attributes`) to its
    /// Ourios-canonical JSON bytes. Stored verbatim in the
    /// matching `BYTE_ARRAY` column by the writer.
    ///
    /// # Errors
    ///
    /// In principle never; the recursive primitives bottom out
    /// at infallible serializers (see [`encode_any_value`] /
    /// the [`CanonicalJsonError`] doc).
    pub fn encode_attributes(attrs: &[KeyValue]) -> Result<Vec<u8>, CanonicalJsonError> {
        if attrs
            .iter()
            .any(|kv| kv.value.as_ref().is_some_and(has_nonfinite))
        {
            let arr = serde_json::Value::Array(attrs.iter().map(kv_to_json).collect());
            serde_json::to_vec(&arr).map_err(CanonicalJsonError::Encode)
        } else {
            serde_json::to_vec(attrs).map_err(CanonicalJsonError::Encode)
        }
    }

    /// Inverse of [`encode_attributes`]. The reader uses this
    /// to recover the `Vec<KeyValue>` from a non-empty stored
    /// column.
    ///
    /// # Errors
    ///
    /// [`CanonicalJsonError::Decode`] on malformed bytes.
    pub fn decode_attributes(bytes: &[u8]) -> Result<Vec<KeyValue>, CanonicalJsonError> {
        match serde_json::from_slice::<Vec<KeyValue>>(bytes) {
            Ok(kvs) => Ok(kvs),
            Err(fast) => {
                let json = serde_json::from_slice::<serde_json::Value>(bytes)
                    .map_err(|_| CanonicalJsonError::Decode(fast))?;
                let arr = json.as_array().ok_or_else(|| {
                    CanonicalJsonError::Decode(de_err("attributes must be a JSON array"))
                })?;
                arr.iter()
                    .map(json_to_kv)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(CanonicalJsonError::Decode)
            }
        }
    }

    // ---- RFC 0018 §3.4 non-finite-double support ----------------------
    //
    // `serde_json` renders a non-finite `f64` as `null` (lossy: the three
    // non-finite values collapse to one shape, and `opentelemetry-proto`'s
    // deserializer rejects both `null` and the proto3 string form for an
    // `f64` field). When — and only when — a non-finite double is present,
    // we route through these hand-built converters, which use the proto3-JSON
    // string forms `"NaN"`/`"Infinity"`/`"-Infinity"` and round-trip them.
    // The finite case stays on `opentelemetry-proto`'s exact serde (above).

    /// A canonical-decode error with `msg` (constructed without a direct
    /// `serde` dependency — `serde_json::Error::io` is the public escape
    /// hatch for "the bytes were valid JSON but not a valid `AnyValue`").
    fn de_err(msg: impl Into<String>) -> serde_json::Error {
        serde_json::Error::io(std::io::Error::other(msg.into()))
    }

    /// Whether `av` contains a non-finite double anywhere in its tree.
    fn has_nonfinite(av: &AnyValue) -> bool {
        match &av.value {
            Some(any_value::Value::DoubleValue(d)) => !d.is_finite(),
            Some(any_value::Value::ArrayValue(a)) => a.values.iter().any(has_nonfinite),
            Some(any_value::Value::KvlistValue(kv)) => kv
                .values
                .iter()
                .any(|k| k.value.as_ref().is_some_and(has_nonfinite)),
            _ => false,
        }
    }

    /// The proto3-JSON string form for a non-finite double.
    fn nonfinite_token(d: f64) -> &'static str {
        if d.is_nan() {
            "NaN"
        } else if d > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
    }

    /// `AnyValue` → JSON, emitting the proto3 string form for non-finite
    /// doubles. Subtrees with no non-finite double delegate to
    /// `opentelemetry-proto`'s exact serde (correct shapes / escaping /
    /// int-decimal-string / base64), so only the non-finite spine is
    /// hand-built.
    fn av_to_json(av: &AnyValue) -> serde_json::Value {
        use serde_json::json;
        match &av.value {
            Some(any_value::Value::DoubleValue(d)) if !d.is_finite() => {
                json!({ "doubleValue": nonfinite_token(*d) })
            }
            Some(any_value::Value::ArrayValue(a)) if a.values.iter().any(has_nonfinite) => {
                json!({ "arrayValue": { "values": a.values.iter().map(av_to_json).collect::<Vec<_>>() } })
            }
            Some(any_value::Value::KvlistValue(kv))
                if kv
                    .values
                    .iter()
                    .any(|k| k.value.as_ref().is_some_and(has_nonfinite)) =>
            {
                json!({ "kvlistValue": { "values": kv.values.iter().map(kv_to_json).collect::<Vec<_>>() } })
            }
            // Finite-only subtree (incl. finite double, string, int, bool,
            // bytes, empty) — `opentelemetry-proto` serde is exact + infallible.
            _ => serde_json::to_value(av).expect("opentelemetry-proto serde is infallible here"),
        }
    }

    /// `KeyValue` → `{"key": …, "value": …}` (value omitted when absent),
    /// matching `opentelemetry-proto`'s shape.
    fn kv_to_json(kv: &KeyValue) -> serde_json::Value {
        use serde_json::json;
        match &kv.value {
            Some(v) => json!({ "key": kv.key, "value": av_to_json(v) }),
            None => json!({ "key": kv.key }),
        }
    }

    /// Inverse of [`av_to_json`]: a generic JSON tree → `AnyValue`, decoding
    /// the proto3 non-finite string forms; finite leaves delegate to
    /// `opentelemetry-proto`'s serde.
    fn json_to_av(j: &serde_json::Value) -> Result<AnyValue, serde_json::Error> {
        if let Some(obj) = j.as_object() {
            if let Some(serde_json::Value::String(s)) = obj.get("doubleValue") {
                let d = match s.as_str() {
                    "NaN" => f64::NAN,
                    "Infinity" => f64::INFINITY,
                    "-Infinity" => f64::NEG_INFINITY,
                    other => {
                        return Err(de_err(format!("invalid doubleValue string {other:?}")));
                    }
                };
                return Ok(AnyValue {
                    value: Some(any_value::Value::DoubleValue(d)),
                });
            }
            if let Some(arr) = obj.get("arrayValue").and_then(|a| a.get("values")) {
                let values = arr
                    .as_array()
                    .ok_or_else(|| de_err("arrayValue.values must be an array"))?
                    .iter()
                    .map(json_to_av)
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(AnyValue {
                    value: Some(any_value::Value::ArrayValue(ArrayValue { values })),
                });
            }
            if let Some(vals) = obj.get("kvlistValue").and_then(|k| k.get("values")) {
                let values = vals
                    .as_array()
                    .ok_or_else(|| de_err("kvlistValue.values must be an array"))?
                    .iter()
                    .map(json_to_kv)
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(AnyValue {
                    value: Some(any_value::Value::KvlistValue(KeyValueList { values })),
                });
            }
        }
        // No non-finite-double / structured spine here → exact serde.
        serde_json::from_value(j.clone())
    }

    /// Inverse of [`kv_to_json`].
    fn json_to_kv(j: &serde_json::Value) -> Result<KeyValue, serde_json::Error> {
        let obj = j
            .as_object()
            .ok_or_else(|| de_err("KeyValue must be a JSON object"))?;
        let key = obj
            .get("key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| de_err("KeyValue.key must be a string"))?
            .to_string();
        let value = match obj.get("value") {
            Some(v) => Some(json_to_av(v)?),
            None => None,
        };
        Ok(KeyValue {
            key,
            value,
            ..Default::default()
        })
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

        /// Pins the **exact bytes** the Ourios canonical body
        /// encoding emits per variant — not just struct round-trip.
        /// This locks the RFC 0001 §6.1 / RFC 0005 §3.3 wire shape
        /// the `body` column depends on: `lowerCamelCase` field
        /// names, `int64`/`uint64` as decimal **strings**, `bytes`
        /// as base64, and — load-bearing — `KvlistValue` /
        /// `ArrayValue` element order **preserved as received, not
        /// sorted** (explicitly NOT RFC 8785 / JCS). The kvlist case
        /// keys `zeta, alpha, mid` are deliberately out of sorted
        /// order so a future serialiser swap that started sorting
        /// would fail here.
        #[test]
        fn encoder_emits_exact_canonical_bytes_per_variant() {
            let cases: [(AnyValue, &str); 5] = [
                (int_av(-42), r#"{"intValue":"-42"}"#),
                (
                    AnyValue {
                        value: Some(any_value::Value::DoubleValue(2.71_f64)),
                    },
                    r#"{"doubleValue":2.71}"#,
                ),
                (
                    AnyValue {
                        value: Some(any_value::Value::BoolValue(true)),
                    },
                    r#"{"boolValue":true}"#,
                ),
                (
                    AnyValue {
                        value: Some(any_value::Value::BytesValue(b"raw\x00bytes".to_vec())),
                    },
                    r#"{"bytesValue":"cmF3AGJ5dGVz"}"#,
                ),
                (
                    AnyValue {
                        value: Some(any_value::Value::ArrayValue(
                            opentelemetry_proto::tonic::common::v1::ArrayValue {
                                values: vec![string_av("a"), int_av(1)],
                            },
                        )),
                    },
                    r#"{"arrayValue":{"values":[{"stringValue":"a"},{"intValue":"1"}]}}"#,
                ),
            ];
            for (av, expected) in cases {
                let bytes = encode_any_value(&av).expect("encode");
                let got = String::from_utf8(bytes).expect("serde_json emits UTF-8");
                assert_eq!(got, expected, "canonical bytes drift for {av:?}");
            }

            // KvlistValue keys in insertion order `zeta, alpha, mid` —
            // the encoder must preserve that order, not sort it.
            let kv = |k: &str, n: i64| KeyValue {
                key: k.to_string(),
                value: Some(int_av(n)),
                ..Default::default()
            };
            let kvlist = AnyValue {
                value: Some(any_value::Value::KvlistValue(
                    opentelemetry_proto::tonic::common::v1::KeyValueList {
                        values: vec![kv("zeta", 1), kv("alpha", 2), kv("mid", 3)],
                    },
                )),
            };
            let bytes = encode_any_value(&kvlist).expect("encode");
            let got = String::from_utf8(bytes).expect("serde_json emits UTF-8");
            assert_eq!(
                got,
                concat!(
                    r#"{"kvlistValue":{"values":["#,
                    r#"{"key":"zeta","value":{"intValue":"1"}},"#,
                    r#"{"key":"alpha","value":{"intValue":"2"}},"#,
                    r#"{"key":"mid","value":{"intValue":"3"}}"#,
                    "]}}",
                ),
                "kvlist must preserve received key order (zeta, alpha, mid) — NOT sorted",
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

        fn double_av(x: f64) -> AnyValue {
            AnyValue {
                value: Some(any_value::Value::DoubleValue(x)),
            }
        }

        /// #130 regression: without `serde_json`'s
        /// `float_roundtrip` feature, decoding the repro value's
        /// stored bytes came back one ULP off (`…255e99` →
        /// `…257e99`) — the default float parser is approximate.
        /// Every finite `f64` must round-trip bit-exactly, the
        /// sign of `-0.0` included.
        #[test]
        fn doubles_round_trip_bit_exact_including_issue_130_repro() {
            for x in [
                -1.537_408_465_042_525_5e99,
                -0.0,
                0.0,
                f64::MAX,
                f64::MIN_POSITIVE,
                5e-324,
            ] {
                let bytes = encode_any_value(&double_av(x)).expect("encode");
                let back = decode_any_value(&bytes).expect("decode");
                let Some(any_value::Value::DoubleValue(y)) = back.value else {
                    panic!("decoded variant drifted for {x:?}");
                };
                assert_eq!(
                    x.to_bits(),
                    y.to_bits(),
                    "f64 round-trip not bit-exact for {x:?}: got {y:?} via {}",
                    String::from_utf8_lossy(&bytes),
                );
            }
        }

        /// RFC 0018 §3.4 overturns the prior clamp-to-`null` gap: a
        /// non-finite double now encodes to the proto3-JSON string form
        /// (`"NaN"`/`"Infinity"`/`"-Infinity"`) and round-trips. (Was: the
        /// `with-serde` encoder emitted `{"doubleValue":null}`, which did not
        /// decode — silently dropping the value.)
        #[test]
        // `x == y` on the ±Infinity arms is intentional exact equality — the
        // round-trip contract is value-identity, and Inf == Inf is well-defined.
        #[allow(clippy::float_cmp)]
        fn nonfinite_doubles_round_trip_via_proto3_strings() {
            for (x, token) in [
                (f64::NAN, "NaN"),
                (f64::INFINITY, "Infinity"),
                (f64::NEG_INFINITY, "-Infinity"),
            ] {
                let bytes = encode_any_value(&double_av(x)).expect("encode");
                assert_eq!(
                    bytes,
                    format!(r#"{{"doubleValue":"{token}"}}"#).into_bytes(),
                    "non-finite double encodes to the proto3 string form",
                );
                let back = decode_any_value(&bytes).expect("decode");
                let Some(any_value::Value::DoubleValue(y)) = back.value else {
                    panic!("decoded variant drifted for {x:?}");
                };
                assert!(
                    (x.is_nan() && y.is_nan()) || x == y,
                    "non-finite double round-trips: {x:?} -> {y:?}",
                );
            }
        }

        /// Walks to the single double planted by the round-trip
        /// property below, whatever nesting it sits at.
        fn planted_double_bits(av: &AnyValue) -> u64 {
            match &av.value {
                Some(any_value::Value::DoubleValue(x)) => x.to_bits(),
                Some(any_value::Value::ArrayValue(array)) => planted_double_bits(&array.values[0]),
                Some(any_value::Value::KvlistValue(kvlist)) => planted_double_bits(
                    kvlist.values[0]
                        .value
                        .as_ref()
                        .expect("kv carries the double"),
                ),
                other => panic!("expected the planted double, got {other:?}"),
            }
        }

        proptest::proptest! {
            /// RFC 0001 §6.1 faithfulness for arbitrary finite
            /// doubles (#130): `decode(encode(x))` is bit-exact at
            /// top level and nested inside array / kvlist.
            #[test]
            fn finite_doubles_round_trip_bit_exact(bits in proptest::prelude::any::<u64>()) {
                let x = f64::from_bits(bits);
                proptest::prop_assume!(x.is_finite());

                let nestings = [
                    double_av(x),
                    AnyValue {
                        value: Some(any_value::Value::ArrayValue(
                            opentelemetry_proto::tonic::common::v1::ArrayValue {
                                values: vec![double_av(x)],
                            },
                        )),
                    },
                    AnyValue {
                        value: Some(any_value::Value::KvlistValue(
                            opentelemetry_proto::tonic::common::v1::KeyValueList {
                                values: vec![KeyValue {
                                    key: "k".to_string(),
                                    value: Some(double_av(x)),
                                    ..Default::default()
                                }],
                            },
                        )),
                    },
                ];
                for av in nestings {
                    let bytes = encode_any_value(&av).expect("encode");
                    let back = decode_any_value(&bytes).expect("decode");
                    proptest::prop_assert_eq!(
                        planted_double_bits(&back),
                        x.to_bits(),
                        "not bit-exact for {:?} via {}",
                        x,
                        String::from_utf8_lossy(&bytes),
                    );
                }
            }
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
