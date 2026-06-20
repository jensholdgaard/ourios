//! RFC 0018 — OTLP log-spec compliance acceptance scenario (§5), the
//! canonical-encoding arm (`.5`).
//!
//! Non-finite doubles (`NaN`/`Infinity`/`-Infinity`) round-trip through the
//! Ourios-canonical JSON via the proto3-JSON string forms — at the top level,
//! nested inside a structured `Body` (`arrayValue`/`kvlistValue`), and as an
//! attribute value. The finite case stays on the exact (`#130`-bit-exact)
//! fast path; this exercises the non-finite robust path.
//!
//! See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5/§6.

use ourios_core::otlp::canonical::{
    decode_any_value, decode_attributes, encode_any_value, encode_attributes,
};
use ourios_core::otlp::{AnyValue, ArrayValue, KeyValue, KeyValueList, any_value};

fn double(x: f64) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::DoubleValue(x)),
    }
}

fn string(s: &str) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::StringValue(s.to_owned())),
    }
}

fn kv(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(value),
        ..Default::default()
    }
}

/// True when `a` and `b` are equal treating any two NaNs as equal (proto3-JSON
/// canonicalises NaN, so a NaN payload is not preserved — value equality is
/// the contract, not bit-identity).
// `x == y` is intentional exact equality (round-trip is value-identity; the
// NaN case is handled explicitly, and Inf == Inf is well-defined).
#[allow(clippy::float_cmp)]
fn nan_eq(a: &AnyValue, b: &AnyValue) -> bool {
    use any_value::Value::{ArrayValue as Arr, DoubleValue as D, KvlistValue as Kv};
    match (&a.value, &b.value) {
        (Some(D(x)), Some(D(y))) => (x.is_nan() && y.is_nan()) || x == y,
        (Some(Arr(x)), Some(Arr(y))) => {
            x.values.len() == y.values.len()
                && x.values.iter().zip(&y.values).all(|(p, q)| nan_eq(p, q))
        }
        (Some(Kv(x)), Some(Kv(y))) => {
            x.values.len() == y.values.len()
                && x.values.iter().zip(&y.values).all(|(p, q)| {
                    p.key == q.key
                        && match (&p.value, &q.value) {
                            (Some(pv), Some(qv)) => nan_eq(pv, qv),
                            (None, None) => true,
                            _ => false,
                        }
                })
        }
        _ => a == b,
    }
}

/// Scenario RFC0018.5 — non-finite doubles round-trip through canonical JSON,
/// at the top level, nested in a structured body, and as an attribute value.
/// See `docs/rfcs/0018-otlp-log-spec-compliance.md` §5.
#[test]
fn rfc0018_5_non_finite_doubles_round_trip() {
    // Top level: each of the three non-finite values.
    for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let av = double(x);
        let back = decode_any_value(&encode_any_value(&av).expect("encode")).expect("decode");
        assert!(
            nan_eq(&av, &back),
            "top-level non-finite double round-trips"
        );
    }

    // Structured body: a kvlist + nested array mixing the three non-finite
    // values with finite doubles and a string — the robust path must walk the
    // whole tree and leave the finite/string leaves intact.
    let body = AnyValue {
        value: Some(any_value::Value::KvlistValue(KeyValueList {
            values: vec![
                kv("nan", double(f64::NAN)),
                kv("inf", double(f64::INFINITY)),
                kv("neg_inf", double(f64::NEG_INFINITY)),
                kv("finite", double(3.5)),
                kv("label", string("ok")),
                kv(
                    "mixed",
                    AnyValue {
                        value: Some(any_value::Value::ArrayValue(ArrayValue {
                            values: vec![double(f64::INFINITY), double(-2.0), string("x")],
                        })),
                    },
                ),
            ],
        })),
    };
    let back = decode_any_value(&encode_any_value(&body).expect("encode")).expect("decode");
    assert!(
        nan_eq(&body, &back),
        "structured body with non-finite doubles round-trips"
    );

    // Attribute value: a non-finite double in a `Vec<KeyValue>` (the
    // attributes / resource_attributes / scope_attributes encoding).
    let attrs = vec![
        kv("rate", double(f64::NEG_INFINITY)),
        kv("name", string("svc")),
    ];
    let decoded = decode_attributes(&encode_attributes(&attrs).expect("encode")).expect("decode");
    assert_eq!(decoded.len(), attrs.len());
    for (a, b) in attrs.iter().zip(&decoded) {
        assert_eq!(a.key, b.key);
        assert!(nan_eq(a.value.as_ref().unwrap(), b.value.as_ref().unwrap()));
    }
}
