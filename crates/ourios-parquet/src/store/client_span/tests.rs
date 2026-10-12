use std::sync::Arc;

use object_store::client::{HttpRequest, HttpRequestBody};
use opentelemetry::global::BoxedTracer;
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceContextExt as _, TraceFlags, TraceId, TraceState,
    Tracer as _, TracerProvider as _,
};
use opentelemetry::{Context, Value};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

use super::{QueryPhase, S3Operation, S3Target, Tracing, object_key};
use crate::store::metrics::Metrics;
use crate::store::test_support::{fake_s3, s3_config, serve};
use crate::store::{Caller, Store};

fn request(method: &str, uri: &str, header: Option<&'static str>) -> HttpRequest {
    let mut req = HttpRequest::new(HttpRequestBody::empty());
    *req.method_mut() = method.parse().expect("method");
    *req.uri_mut() = uri.parse().expect("uri");
    if let Some(name) = header {
        req.headers_mut()
            .insert(name, object_store::HeaderValue::from_static("bucket/src"));
    }
    req
}

#[test]
fn each_request_shape_names_its_s3_operation() {
    let cases = [
        (
            "GET",
            "http://h/b?list-type=2&prefix=p",
            None,
            "S3.ListObjectsV2",
        ),
        ("GET", "http://h/b/k", None, "S3.GetObject"),
        ("HEAD", "http://h/b/k", None, "S3.HeadObject"),
        (
            "PUT",
            "http://h/b/k",
            Some("x-amz-copy-source"),
            "S3.CopyObject",
        ),
        (
            "PUT",
            "http://h/b/k?partNumber=1&uploadId=u",
            None,
            "S3.UploadPart",
        ),
        ("PUT", "http://h/b/k", None, "S3.PutObject"),
        (
            "POST",
            "http://h/b/k?uploads",
            None,
            "S3.CreateMultipartUpload",
        ),
        (
            "POST",
            "http://h/b/k?uploadId=u",
            None,
            "S3.CompleteMultipartUpload",
        ),
        ("POST", "http://h/b?delete", None, "S3.DeleteObjects"),
        ("POST", "http://h/b/k", None, "S3"),
        (
            "DELETE",
            "http://h/b/k?uploadId=u",
            None,
            "S3.AbortMultipartUpload",
        ),
        ("DELETE", "http://h/b/k", None, "S3.DeleteObject"),
        ("PATCH", "http://h/b/k", None, "S3"),
    ];
    for (method, uri, header, name) in cases {
        let operation = S3Operation::of(&request(method, uri, header));
        assert_eq!(operation.span_name(), name, "{method} {uri}");
    }
}

fn target(path_prefix: &str) -> S3Target {
    S3Target {
        bucket: "bucket".to_owned(),
        region: None,
        endpoint_host: Some("h".to_owned()),
        path_prefix: path_prefix.to_owned(),
        tracing: Tracing::Global,
    }
}

fn key(uri: &str, path_prefix: &str) -> Option<String> {
    object_key(&request("GET", uri, None), &target(path_prefix))
}

#[test]
fn the_key_is_read_path_style_or_virtual_hosted_and_only_for_the_bucket() {
    assert_eq!(
        key("http://h/bucket/data/a.parquet", ""),
        Some("data/a.parquet".into())
    );
    assert_eq!(key("http://h/bucket?list-type=2", ""), Some(String::new()));
    assert_eq!(
        key("http://bucket.h/data/a.parquet", ""),
        Some("data/a.parquet".into())
    );
    assert_eq!(key("http://h/bucket2/k", ""), None);
    assert_eq!(key("http://169.254.169.254/latest/api/token", ""), None);
}

/// Only the configured S3 host addresses the bucket: with a bucket named
/// like the instance-metadata path, a metadata request is still not an S3
/// request.
#[test]
fn a_request_to_another_host_is_not_an_s3_request() {
    let latest = S3Target {
        bucket: "latest".to_owned(),
        ..target("")
    };
    let request = request("GET", "http://169.254.169.254/latest/api/token", None);
    assert_eq!(object_key(&request, &latest), None);
}

/// A virtual-hosted request keeps its whole path as the key, even when the
/// key's first segment is the bucket's name.
#[test]
fn a_virtual_hosted_key_may_start_with_the_bucket_name() {
    assert_eq!(
        key("http://bucket.h/bucket/foo", ""),
        Some("bucket/foo".into())
    );
}

/// The key is recorded as stored, not as escaped on the wire: a literal
/// `%2F` in a tenant component travels as `%252F`.
#[test]
fn the_key_is_percent_decoded_once() {
    assert_eq!(
        key("http://h/bucket/data/tenant_id=a%252Fb/x.parquet", ""),
        Some("data/tenant_id=a%2Fb/x.parquet".into())
    );
}

/// An endpoint configured below a path prefixes every path-style request
/// with it; the prefix comes off at a segment boundary only.
#[test]
fn an_endpoint_path_prefix_is_removed_before_the_bucket() {
    assert_eq!(
        key("http://h/s3/bucket/data/a.parquet", "/s3"),
        Some("data/a.parquet".into())
    );
    assert_eq!(key("http://h/s3/bucket", "/s3"), Some(String::new()));
    assert_eq!(key("http://h/s3x/bucket/k", "/s3"), None);
    assert_eq!(key("http://h/other/bucket/k", "/s3"), None);
    assert_eq!(key("http://bucket.h/s3/k", "/s3"), Some("k".into()));
}

/// A tracer of the test's own over an in-memory exporter, so nothing
/// global is installed.
struct Traced {
    exporter: InMemorySpanExporter,
    provider: SdkTracerProvider,
}

impl Traced {
    fn new() -> Self {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        Self { exporter, provider }
    }

    /// A store against the fake endpoint on `port`, tracing here.
    fn store(&self, port: u16) -> Store {
        let tracer = self
            .provider
            .tracer_with_scope(ourios_core::telemetry::scope("ourios.storage"));
        let tracing = Tracing::Owned(Arc::new(BoxedTracer::new(Box::new(tracer))));
        Store::s3_with(s3_config(port), Metrics::default(), tracing)
            .expect("s3")
            .with_caller(Caller::TemplateMap)
    }

    /// A fresh sampled parent span's context, with the phase marker.
    fn phase(&self) -> Context {
        let tracer = self
            .provider
            .tracer_with_scope(ourios_core::telemetry::scope("test"));
        Context::current_with_span(tracer.start("load template_map")).with_value(QueryPhase)
    }

    fn client_spans(&self) -> Vec<SpanData> {
        self.exporter
            .get_finished_spans()
            .expect("spans")
            .into_iter()
            .filter(|s| s.span_kind == SpanKind::Client)
            .collect()
    }
}

/// Run `work` with `cx` attached.
fn under(cx: Context, work: impl FnOnce()) {
    let _attached = cx.attach();
    work();
}

fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

const NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nx-amz-request-id: R1\r\nConnection: close\r\n\r\n";

#[test]
fn a_failed_request_in_a_query_phase_is_an_error_client_span() {
    let traced = Traced::new();
    let port = fake_s3(vec![NOT_FOUND]);
    let store = traced.store(port);
    let phase = traced.phase();
    let parent = phase.span().span_context().span_id();
    under(phase, || {
        let err = store.get_blocking("data/a.parquet").expect_err("404");
        assert!(err.is_not_found(), "{err:?}");
    });

    let spans = traced.client_spans();
    let [span] = spans.as_slice() else {
        panic!("one CLIENT span, got {spans:?}");
    };
    assert_eq!(span.name.as_ref(), "S3.GetObject");
    assert_eq!(span.parent_span_id, parent, "parented to the phase span");
    assert!(
        matches!(span.status, Status::Error { .. }),
        "{:?}",
        span.status
    );
    let text = |key| attribute(span, key).map(|v| v.as_str().into_owned());
    assert_eq!(text("error.type").as_deref(), Some("404"));
    assert_eq!(text("rpc.system").as_deref(), Some("aws-api"));
    assert_eq!(text("rpc.service").as_deref(), Some("S3"));
    assert_eq!(text("rpc.method").as_deref(), Some("GetObject"));
    assert_eq!(text("aws.s3.bucket").as_deref(), Some("bucket"));
    assert_eq!(text("aws.s3.key").as_deref(), Some("data/a.parquet"));
    assert_eq!(text("aws.request_id").as_deref(), Some("R1"));
    assert_eq!(text("cloud.region").as_deref(), Some("us-east-1"));
    assert_eq!(text("server.address").as_deref(), Some("127.0.0.1"));
    assert_eq!(text("http.request.method").as_deref(), Some("GET"));
    assert_eq!(
        attribute(span, "server.port"),
        Some(Value::I64(i64::from(port)))
    );
    assert_eq!(
        attribute(span, "http.response.status_code"),
        Some(Value::I64(404))
    );
}

#[test]
fn a_successful_request_leaves_its_client_span_unset() {
    let traced = Traced::new();
    let port = fake_s3(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"abc\"\r\n\
         Last-Modified: Wed, 07 Oct 2026 11:00:00 GMT\r\nConnection: close\r\n\r\nhello",
    ]);
    let store = traced.store(port);
    under(traced.phase(), || {
        assert_eq!(store.get_blocking("k").expect("get"), b"hello");
    });
    let spans = traced.client_spans();
    let [span] = spans.as_slice() else {
        panic!("one CLIENT span, got {spans:?}");
    };
    assert_eq!(span.status, Status::Unset);
    assert_eq!(attribute(span, "error.type"), None);
}

/// A remote parent span context with `flags`, valid or not.
fn remote_parent(valid: bool, flags: TraceFlags) -> Context {
    let (trace_id, span_id) = if valid {
        (TraceId::from(7_u128), SpanId::from(7_u64))
    } else {
        (TraceId::INVALID, SpanId::INVALID)
    };
    let parent = SpanContext::new(trace_id, span_id, flags, true, TraceState::default());
    Context::new().with_remote_span_context(parent)
}

/// RFC0038.10 at the connector: no `CLIENT` span without the phase marker,
/// under a sampled-out parent, or under an invalid parent that carries the
/// sampled flag, where it would be a root.
#[test]
fn no_client_span_outside_a_traced_query_phase() {
    let parents = [
        ("no phase marker", remote_parent(true, TraceFlags::SAMPLED)),
        (
            "sampled-out parent",
            remote_parent(true, TraceFlags::default()).with_value(QueryPhase),
        ),
        (
            "invalid but sampled parent",
            remote_parent(false, TraceFlags::SAMPLED).with_value(QueryPhase),
        ),
    ];
    for (case, cx) in parents {
        let traced = Traced::new();
        let store = traced.store(fake_s3(vec![NOT_FOUND]));
        under(cx, || {
            store.get_blocking("k").expect_err("404");
        });
        let spans = traced.client_spans();
        assert!(spans.is_empty(), "{case}: {spans:?}");
    }
}

/// A request its caller abandons before the response ends its span
/// `Unset`, with no `error.type`: semconv does not count an intentional
/// cancellation as an error.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn an_abandoned_request_leaves_its_client_span_unset() {
    use object_store::ObjectStoreExt as _;
    use opentelemetry::context::FutureExt as _;

    let traced = Traced::new();
    let backend = traced.store(serve(1, |_| None)).object_store();
    let request = async move { backend.get(&object_store::path::Path::from("k")).await };
    let abandoned = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        request.with_context(traced.phase()),
    )
    .await;
    assert!(abandoned.is_err(), "the fake never answers");

    let spans = traced.client_spans();
    let [span] = spans.as_slice() else {
        panic!("one CLIENT span, got {spans:?}");
    };
    assert_eq!(span.name.as_ref(), "S3.GetObject");
    assert_eq!(span.status, Status::Unset);
    assert_eq!(attribute(span, "error.type"), None);
}
