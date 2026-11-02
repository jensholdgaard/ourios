// Shared by `rfc0061_request_size_limit.rs` and the harness-exempt
// `rfc0061_telemetry.rs`; each binary uses only some of the helpers, so
// `dead_code` here is the expected shared-module shape.
#![allow(dead_code)]

//! RFC 0061 test support: a served gRPC listener, a raw-bytes gRPC client
//! (a typed client would re-encode the request and so could not send a
//! non-minimal encoding or an exact byte count), and Exports of an exact
//! encoded size.

use std::net::SocketAddr;

use bytes::BufMut as _;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::AnyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
use ourios_ingester::receiver::SharedPipeline;
use ourios_ingester::receiver::grpc::logs_service;
use prost::Message;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::{Code, Status};
use tonic_types::StatusExt as _;

/// The OTLP/gRPC logs Export method.
const EXPORT_PATH: &str = "/opentelemetry.proto.collector.logs.v1.LogsService/Export";

/// Serve the production gRPC logs service (`logs_service`) over `pipeline`
/// with request limit `limit` on a loopback port.
pub fn serve(
    pipeline: SharedPipeline,
    limit: usize,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
) {
    let incoming =
        tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().expect("addr"))
            .expect("bind");
    let addr = incoming.local_addr().expect("local addr");
    let service = logs_service(pipeline, limit);
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
    });
    (addr, server)
}

/// Send `body` verbatim as one Export to `addr` under `tenant`, gzip
/// compressed when `gzip` is set.
pub async fn export_raw(
    addr: SocketAddr,
    tenant: &str,
    body: Vec<u8>,
    gzip: bool,
) -> Result<(), Status> {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut grpc = tonic::client::Grpc::new(channel);
    if gzip {
        grpc = grpc.send_compressed(tonic::codec::CompressionEncoding::Gzip);
    }
    grpc.ready().await.expect("ready");
    let mut request = tonic::Request::new(body);
    request
        .metadata_mut()
        .insert("x-ourios-tenant", tenant.parse().expect("ascii tenant"));
    grpc.unary(
        request,
        http::uri::PathAndQuery::from_static(EXPORT_PATH),
        RawCodec,
    )
    .await
    .map(|_| ())
}

/// Assert `result` is the RFC 0061 non-retryable over-limit rejection:
/// `RESOURCE_EXHAUSTED` (never `OUT_OF_RANGE`) with no `RetryInfo`.
pub fn assert_too_large(result: Result<(), Status>, arm: &str) {
    let status = result.expect_err(arm);
    assert_eq!(status.code(), Code::ResourceExhausted, "{arm}: {status:?}");
    assert!(
        status.get_details_retry_info().is_none(),
        "{arm}: no RetryInfo, so the client does not retry"
    );
}

/// Writes the request bytes as given; decodes the response with prost.
#[derive(Clone, Copy)]
struct RawCodec;

struct RawEncoder;

struct ResponseDecoder;

impl Codec for RawCodec {
    type Encode = Vec<u8>;
    type Decode = ExportLogsServiceResponse;
    type Encoder = RawEncoder;
    type Decoder = ResponseDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        RawEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        ResponseDecoder
    }
}

impl Encoder for RawEncoder {
    type Item = Vec<u8>;
    type Error = Status;

    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for ResponseDecoder {
    type Item = ExportLogsServiceResponse;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Status> {
        ExportLogsServiceResponse::decode(src)
            .map(Some)
            .map_err(|e| Status::internal(e.to_string()))
    }
}

/// A record whose body is `len` copies of `x`.
fn record(len: usize) -> LogRecord {
    LogRecord {
        body: Some(AnyValue {
            value: Some(Value::StringValue("x".repeat(len))),
        }),
        ..Default::default()
    }
}

fn wrap(log_records: Vec<LogRecord>) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            scope_logs: vec![ScopeLogs {
                log_records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Grow the last of `records` bodies until `encode(records)` is exactly
/// `target` bytes. A varint length prefix can step past the target; the
/// loop then shrinks a different record to land on it.
fn sized<F: Fn(&[LogRecord]) -> usize>(
    mut records: Vec<LogRecord>,
    target: usize,
    encode: F,
) -> Vec<LogRecord> {
    let body_len = |r: &LogRecord| match r.body.as_ref().and_then(|b| b.value.as_ref()) {
        Some(Value::StringValue(s)) => s.len(),
        _ => 0,
    };
    for attempt in 0..16 {
        let len = encode(&records);
        if len == target {
            return records;
        }
        // Adjust the last record, or the first one when the last keeps
        // straddling a varint boundary.
        let index = if attempt < 8 { records.len() - 1 } else { 0 };
        let current = body_len(&records[index]);
        let wanted = if len < target {
            current + (target - len)
        } else {
            current.saturating_sub(len - target)
        };
        records[index] = record(wanted);
    }
    panic!("could not size the export to {target} bytes");
}

/// A minimally encoded Export of exactly `size` bytes: `records` records
/// of roughly equal string bodies.
pub fn export_of_size(size: usize, records: usize) -> Vec<u8> {
    let each = size / records;
    let start = (0..records).map(|_| record(each)).collect();
    let records = sized(start, size, |r| wrap(r.to_vec()).encoded_len());
    let bytes = wrap(records).encode_to_vec();
    assert_eq!(bytes.len(), size);
    bytes
}

/// `tag`, the varint length of `payload`, then `payload`.
fn length_delimited(tag: u8, payload: &[u8], out: &mut Vec<u8>) {
    out.push(tag);
    prost::encoding::encode_varint(payload.len() as u64, out);
    out.extend_from_slice(payload);
}

/// `severity_number` (field 2, varint) = -1 in five bytes. prost reads it
/// as -1 and writes it back sign-extended, in ten.
const SEVERITY_MINUS_ONE_IN_FIVE_BYTES: [u8; 6] = [0x10, 0xff, 0xff, 0xff, 0xff, 0x0f];

fn grown_bytes(records: &[LogRecord]) -> Vec<u8> {
    let mut scope = Vec::new();
    for record in records {
        let mut encoded = record.encode_to_vec();
        encoded.extend_from_slice(&SEVERITY_MINUS_ONE_IN_FIVE_BYTES);
        length_delimited(0x12, &encoded, &mut scope);
    }
    let mut resource = Vec::new();
    length_delimited(0x12, &scope, &mut resource);
    let mut request = Vec::new();
    length_delimited(0x0a, &resource, &mut request);
    request
}

/// RFC0061.6 — an Export of exactly `size` bytes whose 1 000 records each
/// carry `severity_number = -1` in five bytes, so prost re-encodes it about
/// 5 000 bytes larger.
pub fn export_that_grows(size: usize) -> Vec<u8> {
    let start = (0..1000).map(|_| record(size / 1000 - 32)).collect();
    let records = sized(start, size, |r| grown_bytes(r).len());
    let bytes = grown_bytes(&records);
    assert_eq!(bytes.len(), size);
    bytes
}

/// Bytes that gzip cannot shrink below their own length.
pub fn incompressible(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}
