//! RFC 0069 §3.1: the canonical decoders read every depth ingest accepts.
//! `serde_json`'s fixed recursion limit is lifted, and a scan bounds the
//! nesting before the parse instead.

use serde::de::DeserializeOwned;

/// The deepest JSON nesting a canonical cell may hold (RFC 0069 §3.1). The
/// deepest value ingest admits, bounded by `prost`'s 100-message and
/// `serde_json`'s 128-level OTLP/JSON limits, is about 150 levels; RFC0069.2
/// pins that it stays under this.
pub const MAX_READ_DEPTH: usize = 256;

/// How deep `bytes` nests, counting brackets outside strings only.
pub(super) fn json_depth(bytes: &[u8]) -> usize {
    let mut scan = Scan::default();
    for &byte in bytes {
        scan.step(byte);
    }
    scan.deepest
}

/// [`json_depth`]'s state: where in a string the scan is, and how deep.
#[derive(Default)]
struct Scan {
    depth: usize,
    deepest: usize,
    in_string: bool,
    escaped: bool,
}

impl Scan {
    fn step(&mut self, byte: u8) {
        if self.in_string {
            self.step_in_string(byte);
            return;
        }
        match byte {
            b'"' => self.in_string = true,
            b'[' | b'{' => {
                self.depth += 1;
                self.deepest = self.deepest.max(self.depth);
            }
            b']' | b'}' => self.depth = self.depth.saturating_sub(1),
            _ => {}
        }
    }

    fn step_in_string(&mut self, byte: u8) {
        match (self.escaped, byte) {
            (true, _) => self.escaped = false,
            (false, b'\\') => self.escaped = true,
            (false, b'"') => self.in_string = false,
            (false, _) => {}
        }
    }
}

/// `serde_json::from_slice` without its fixed recursion limit, for bytes
/// whose depth [`json_depth`] has already bounded.
pub(super) fn from_slice_bounded<T: DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, serde_json::Error> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    de.disable_recursion_limit();
    let value = T::deserialize(&mut de)?;
    de.end()?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
    use opentelemetry_proto::tonic::common::v1::{ArrayValue, KeyValue, KeyValueList};
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use prost::Message;

    use super::super::*;
    use crate::otlp::any_value::Value;

    fn string_av() -> AnyValue {
        AnyValue {
            value: Some(Value::StringValue("x".to_owned())),
        }
    }

    /// `layers` arrays, each holding the next, around a string.
    fn array_chain(layers: usize) -> AnyValue {
        array_around(string_av(), layers)
    }

    /// `layers` arrays, each holding the next, around `leaf`.
    fn array_around(leaf: AnyValue, layers: usize) -> AnyValue {
        (0..layers).fold(leaf, |inner, _| AnyValue {
            value: Some(Value::ArrayValue(ArrayValue {
                values: vec![inner],
            })),
        })
    }

    /// `layers` maps, each holding the next under key `a`, around a string.
    fn map_chain(layers: usize) -> AnyValue {
        map_around(string_av(), layers)
    }

    /// `layers` maps, each holding the next under key `a`, around `leaf`.
    fn map_around(leaf: AnyValue, layers: usize) -> AnyValue {
        (0..layers).fold(leaf, |inner, _| AnyValue {
            value: Some(Value::KvlistValue(KeyValueList {
                values: vec![KeyValue {
                    key: "a".to_owned(),
                    value: Some(inner),
                    key_strindex: 0,
                }],
            })),
        })
    }

    fn request(record: LogRecord) -> ExportLogsServiceRequest {
        ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![record],
                    ..ScopeLogs::default()
                }],
                ..ResourceLogs::default()
            }],
        }
    }

    fn as_body(value: AnyValue) -> ExportLogsServiceRequest {
        request(LogRecord {
            body: Some(value),
            ..LogRecord::default()
        })
    }

    fn as_attribute(value: AnyValue) -> ExportLogsServiceRequest {
        request(LogRecord {
            attributes: vec![KeyValue {
                key: "k".to_owned(),
                value: Some(value),
                key_strindex: 0,
            }],
            ..LogRecord::default()
        })
    }

    /// Whether the receiver's protobuf decode (`prost`) admits `request`.
    fn protobuf_admits(request: &ExportLogsServiceRequest) -> bool {
        ExportLogsServiceRequest::decode(request.encode_to_vec().as_slice()).is_ok()
    }

    /// Whether the receiver's OTLP/JSON decode admits `request`.
    fn json_admits(request: &ExportLogsServiceRequest) -> bool {
        let json = serde_json::to_vec(request).expect("encode OTLP/JSON");
        crate::otlp::lenient_json::from_slice::<ExportLogsServiceRequest>(&json).is_ok()
    }

    /// The deepest `chain` a decode admits as placed by `place`, found by
    /// probing the decode itself rather than restating its limit.
    fn deepest(
        chain: fn(usize) -> AnyValue,
        place: fn(AnyValue) -> ExportLogsServiceRequest,
        admits: fn(&ExportLogsServiceRequest) -> bool,
    ) -> AnyValue {
        let layers = (1..1_000)
            .take_while(|&n| admits(&place(chain(n))))
            .last()
            .expect("one layer is admitted");
        chain(layers)
    }

    /// Scenario RFC0069.2 — the read bound covers the ingest bound.
    /// See `docs/rfcs/0069-nested-value-read-depth.md` §5.
    #[test]
    fn rfc0069_2_the_read_bound_covers_the_ingest_bound() {
        for chain in [array_chain as fn(usize) -> AnyValue, map_chain] {
            for admits in [
                protobuf_admits as fn(&ExportLogsServiceRequest) -> bool,
                json_admits,
            ] {
                let body = deepest(chain, as_body, admits);
                let cell = encode_any_value(&body).expect("encode body");
                assert!(
                    json_depth(&cell) <= MAX_READ_DEPTH,
                    "body {}",
                    json_depth(&cell)
                );
                assert!(
                    decode_any_value(&cell).is_ok(),
                    "the deepest body reads back"
                );

                let attribute = deepest(chain, as_attribute, admits);
                let kv = KeyValue {
                    key: "k".to_owned(),
                    value: Some(attribute),
                    key_strindex: 0,
                };
                let cell = encode_attributes(&[kv]).expect("encode attributes");
                assert!(
                    json_depth(&cell) <= MAX_READ_DEPTH,
                    "attributes {}",
                    json_depth(&cell)
                );
                assert!(
                    decode_attributes(&cell).is_ok(),
                    "the deepest attribute reads back"
                );
            }
        }
    }

    /// RFC0069.2: a value at `MAX_READ_DEPTH` decodes on a thread with the
    /// smallest stack the query and compaction paths run on (Tokio's and
    /// `std`'s 2 MiB default), and one level past it is refused, not
    /// overflowed.
    #[test]
    fn rfc0069_2_the_bound_decodes_on_the_smallest_stack_and_refuses_past_it() {
        // An array layer is three levels and the innermost string one.
        let layers = (MAX_READ_DEPTH - 1) / 3;
        let at_bound = encode_any_value(&array_chain(layers)).expect("encode");
        assert_eq!(json_depth(&at_bound), MAX_READ_DEPTH);
        let past = format!(
            "{}{}",
            "[".repeat(MAX_READ_DEPTH + 1),
            "]".repeat(MAX_READ_DEPTH + 1)
        );

        let decoded = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                (
                    decode_any_value(&at_bound).is_ok(),
                    decode_any_value(past.as_bytes()),
                    decode_attributes(past.as_bytes()),
                )
            })
            .expect("spawn")
            .join()
            .expect("no stack overflow");

        assert!(decoded.0, "a value at the bound decodes");
        assert!(
            matches!(decoded.1, Err(CanonicalJsonError::TooDeep { .. })),
            "{:?}",
            decoded.1
        );
        assert!(
            matches!(decoded.2, Err(CanonicalJsonError::TooDeep { .. })),
            "{:?}",
            decoded.2
        );
    }

    /// The deepest cell `build` makes within `MAX_READ_DEPTH`, at most one
    /// map layer short of it.
    fn deepest_within_bound(build: impl Fn(usize) -> Vec<u8>) -> Vec<u8> {
        let cell = (1..MAX_READ_DEPTH)
            .map(build)
            .take_while(|cell| json_depth(cell) <= MAX_READ_DEPTH)
            .last()
            .expect("one layer fits");
        assert!(
            json_depth(&cell) > MAX_READ_DEPTH - 4,
            "{}",
            json_depth(&cell)
        );
        cell
    }

    /// Body and attribute cells near `MAX_READ_DEPTH` for every chain and
    /// leaf. A non-finite double fails the typed decode, so its cells take
    /// the generic-JSON fallback and its recursive conversion.
    fn near_bound_cells() -> Vec<(String, Vec<u8>, Vec<u8>)> {
        let leaves = [
            ("a string", string_av()),
            ("an empty value", AnyValue { value: None }),
            (
                "NaN",
                AnyValue {
                    value: Some(Value::DoubleValue(f64::NAN)),
                },
            ),
        ];
        let chains = [
            ("arrays", array_around as fn(AnyValue, usize) -> AnyValue),
            ("maps", map_around),
        ];
        let mut cells = Vec::new();
        for (chain_name, chain) in chains {
            for (leaf_name, leaf) in &leaves {
                let body = deepest_within_bound(|n| {
                    encode_any_value(&chain(leaf.clone(), n)).expect("encode body")
                });
                let attributes = deepest_within_bound(|n| {
                    let kv = KeyValue {
                        key: "k".to_owned(),
                        value: Some(chain(leaf.clone(), n)),
                        key_strindex: 0,
                    };
                    encode_attributes(&[kv]).expect("encode attributes")
                });
                cells.push((format!("{chain_name} around {leaf_name}"), body, attributes));
            }
        }
        cells
    }

    /// RFC0069.2: both decoders, on the typed path and on the generic-JSON
    /// fallback, read a value near the bound back unchanged on the smallest
    /// stack.
    #[test]
    fn rfc0069_2_every_decode_path_fits_the_smallest_stack_near_the_bound() {
        let cells = near_bound_cells();
        let outcomes = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                cells
                    .into_iter()
                    .map(|(name, body, attributes)| {
                        let body_back = decode_any_value(&body)
                            .ok()
                            .and_then(|av| encode_any_value(&av).ok());
                        let attributes_back = decode_attributes(&attributes)
                            .ok()
                            .and_then(|kvs| encode_attributes(&kvs).ok());
                        (
                            name,
                            body_back.as_ref() == Some(&body),
                            attributes_back.as_ref() == Some(&attributes),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .expect("spawn")
            .join()
            .expect("no stack overflow");

        for (name, body, attributes) in outcomes {
            assert!(body, "body of {name} reads back");
            assert!(attributes, "attributes of {name} read back");
        }
    }

    /// The scan counts nesting outside strings only.
    #[test]
    fn brackets_inside_strings_do_not_count() {
        assert_eq!(json_depth(br#"{"a":"[[[{{{","b":["]"]}"#), 2);
        assert_eq!(json_depth(br#"["\"[["]"#), 1);
        assert_eq!(json_depth(b"7"), 0);
    }
}
