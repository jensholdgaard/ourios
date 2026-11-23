//! Scenario RFC0069.1 — every depth ingest accepts reads back.
//! See `docs/rfcs/0069-nested-value-read-depth.md` §5.
//!
//! The depth matrix of #1136: a map chain and an array chain at every depth
//! the receiver admits, over protobuf and OTLP/JSON, as the body, a log
//! attribute, a resource attribute and a scope attribute. Each request goes
//! through the receiver's own decoders, is mined into Parquet, and is read
//! back by a query.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList,
};
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use ourios_config::MinerConfig;
use ourios_core::tenant::TenantId;
use ourios_ingester::receiver::decode::{decode_json, decode_protobuf};
use ourios_ingester::receiver::tenant::assign;
use ourios_ingester::record_sink::{FlushConfig, ParquetRecordSink, SharedParquetSink};
use ourios_miner::cluster::MinerCluster;
use ourios_parquet::Store;
use ourios_querier::{LogBody, Querier, QueryRequest};
use prost::Message;

const TENANT: &str = "acme";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Body,
    Log,
    Resource,
    Scope,
}

#[derive(Clone, Copy, Debug)]
enum Transport {
    Protobuf,
    Json,
}

fn int(n: i64) -> AnyValue {
    AnyValue {
        value: Some(Value::IntValue(n)),
    }
}

fn array_chain(layers: usize) -> AnyValue {
    (0..layers).fold(int(1), |inner, _| AnyValue {
        value: Some(Value::ArrayValue(ArrayValue {
            values: vec![inner],
        })),
    })
}

fn map_chain(layers: usize) -> AnyValue {
    (0..layers).fold(int(1), |inner, _| AnyValue {
        value: Some(Value::KvlistValue(KeyValueList {
            values: vec![attribute(inner)],
        })),
    })
}

fn attribute(value: AnyValue) -> KeyValue {
    KeyValue {
        key: "k".to_owned(),
        value: Some(value),
        key_strindex: 0,
    }
}

/// One record stamped `time`, carrying `value` at `place`; every record's
/// body is structured, so it reads back without a template registry.
fn request(value: &AnyValue, place: Place, time: u64) -> ExportLogsServiceRequest {
    let attrs = |at: Place| {
        if at == place {
            vec![attribute(value.clone())]
        } else {
            Vec::new()
        }
    };
    let body = match place {
        Place::Body => value.clone(),
        _ => int(0),
    };
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: attrs(Place::Resource),
                ..Resource::default()
            }),
            scope_logs: vec![ScopeLogs {
                scope: Some(InstrumentationScope {
                    attributes: attrs(Place::Scope),
                    ..InstrumentationScope::default()
                }),
                log_records: vec![LogRecord {
                    time_unix_nano: time,
                    body: Some(body),
                    attributes: attrs(Place::Log),
                    ..LogRecord::default()
                }],
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }],
    }
}

/// `request` as the receiver decodes it over `transport`, if it admits it.
fn decoded(
    request: &ExportLogsServiceRequest,
    transport: Transport,
) -> Option<ExportLogsServiceRequest> {
    match transport {
        Transport::Protobuf => decode_protobuf(&request.encode_to_vec()).ok(),
        Transport::Json => {
            let json = serde_json::to_vec(request).expect("encode OTLP/JSON");
            decode_json(&json).ok().map(|(decoded, _)| decoded)
        }
    }
}

/// One sent value: where it went, at which depth, and the request the
/// receiver decoded.
struct Sent {
    time: u64,
    place: Place,
    value: AnyValue,
    request: ExportLogsServiceRequest,
}

/// Every chain at every depth the receiver admits, over both transports.
fn matrix() -> Vec<Sent> {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let mut time = u64::try_from(base).expect("fits");
    let chains = [array_chain as fn(usize) -> AnyValue, map_chain];
    let places = [Place::Body, Place::Log, Place::Resource, Place::Scope];
    let mut sent = Vec::new();
    for (transport, place, chain) in
        [Transport::Protobuf, Transport::Json]
            .into_iter()
            .flat_map(|t| {
                places
                    .into_iter()
                    .flat_map(move |p| chains.map(|c| (t, p, c)))
            })
    {
        sent.extend(admitted(transport, place, chain, &mut time));
    }
    sent
}

/// `chain` at every depth `transport` admits at `place`, stamped from
/// `time` onward.
fn admitted(
    transport: Transport,
    place: Place,
    chain: fn(usize) -> AnyValue,
    time: &mut u64,
) -> Vec<Sent> {
    let mut sent = Vec::new();
    for layers in 1.. {
        *time += 1;
        let value = chain(layers);
        let Some(request) = decoded(&request(&value, place, *time), transport) else {
            assert!(layers > 1, "{transport:?} {place:?} admits one layer");
            break;
        };
        sent.push(Sent {
            time: *time,
            place,
            value,
            request,
        });
    }
    sent
}

/// The value a queried row holds at `place`.
fn read_back(row: &ourios_querier::LogRow, place: Place) -> Option<AnyValue> {
    let only = |attrs: &[KeyValue]| attrs.first().and_then(|kv| kv.value.clone());
    match place {
        Place::Body => match &row.body {
            LogBody::Structured(value) => Some(value.clone()),
            _ => None,
        },
        Place::Log => only(&row.attributes),
        Place::Resource => only(&row.resource_attributes),
        Place::Scope => only(&row.scope_attributes),
    }
}

/// Scenario RFC0069.1 — every depth ingest accepts reads back.
/// See `docs/rfcs/0069-nested-value-read-depth.md` §5.
#[tokio::test]
async fn rfc0069_1_every_depth_ingest_accepts_reads_back() {
    let bucket = tempfile::TempDir::new().expect("temp");
    let sink = SharedParquetSink::new(ParquetRecordSink::new(
        Store::local(bucket.path()).expect("store"),
        FlushConfig {
            target_bytes: usize::MAX,
            max_buffer_age: std::time::Duration::from_secs(3_600),
            ceiling_bytes: usize::MAX,
        },
    ));
    let mut miner =
        MinerCluster::new(MinerConfig::default()).with_record_sink(Box::new(sink.clone()));
    let sent = matrix();
    for each in &sent {
        for record in assign(each.request.clone(), &TenantId::new(TENANT)) {
            miner.ingest(&record);
        }
    }
    sink.flush_all();

    let result = Querier::new(bucket.path())
        .run(QueryRequest {
            tenant: TenantId::new(TENANT),
            time_range: None,
            template_id: None,
            severity_text: None,
            limit: Some(sent.len() + 1),
        })
        .await
        .expect("the query reads every row back");

    assert_eq!(result.records.len(), sent.len(), "one row per sent value");
    for each in &sent {
        let row = result
            .records
            .iter()
            .find(|row| row.time_unix_nano == each.time)
            .unwrap_or_else(|| panic!("the row at {} is returned", each.time));
        assert_eq!(
            read_back(row, each.place),
            Some(each.value.clone()),
            "{:?} at {}",
            each.place,
            each.time
        );
    }
}
