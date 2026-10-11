//! Object-store `CLIENT` spans (RFC 0038 §3.7.3): one `S3.{Operation}` span
//! per HTTP request the S3 backend makes inside a query phase.
//!
//! A span opens only when the request's `OTel` context carries both a sampled
//! parent span and the [`QueryPhase`] marker, so ingest, compaction and the
//! `DataFusion` scan's own reads stay span-free and no span is ever a root.
//! Attributes are the upstream AWS SDK / S3 client-span conventions; the
//! status follows *Recording errors* (a `>= 400` response or a transport
//! failure is `Error`, a request its caller abandoned is left unset).

use object_store::client::{HttpRequest, HttpResponse};
use opentelemetry::trace::{Span as _, SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue, global::BoxedSpan};

use super::metrics::Failure;

/// The `OTel` [`Context`] value that marks a query phase (`load template_map`
/// and the spans under it): object-store requests made under it open `CLIENT`
/// spans. The querier attaches it with the phase span's context.
#[derive(Clone, Copy, Debug)]
pub struct QueryPhase;

/// The configured bucket the S3 client addresses.
#[derive(Clone, Debug)]
pub(super) struct S3Target {
    pub(super) bucket: String,
    pub(super) region: Option<String>,
}

/// One request's `CLIENT` span, ended by [`Self::end`] or on drop.
pub(super) struct ClientSpan(BoxedSpan);

impl ClientSpan {
    /// Open the span for `req`, or `None` outside a sampled query phase or
    /// for a request not addressed to the bucket (a credential endpoint).
    /// `request_attributes` are the HTTP client attributes known up front.
    pub(super) fn start(
        req: &HttpRequest,
        target: &S3Target,
        request_attributes: &[KeyValue],
    ) -> Option<Self> {
        let cx = Context::current();
        if cx.get::<QueryPhase>().is_none() || !cx.span().span_context().is_sampled() {
            return None;
        }
        let key = object_key(req, &target.bucket)?;
        let operation = S3Operation::of(req);
        let mut attributes = Vec::with_capacity(request_attributes.len() + 6);
        attributes.extend_from_slice(request_attributes);
        attributes.push(KeyValue::new("rpc.system", "aws-api"));
        attributes.push(KeyValue::new("rpc.service", "S3"));
        attributes.push(KeyValue::new("aws.s3.bucket", target.bucket.clone()));
        if let Some(method) = operation.method() {
            attributes.push(KeyValue::new("rpc.method", method));
        }
        if operation.on_object() && !key.is_empty() {
            attributes.push(KeyValue::new("aws.s3.key", key.to_owned()));
        }
        if let Some(region) = &target.region {
            attributes.push(KeyValue::new("cloud.region", region.clone()));
        }
        let tracer = ourios_core::telemetry::tracer("ourios.storage");
        let span = tracer
            .span_builder(operation.span_name())
            .with_kind(SpanKind::Client)
            .with_attributes(attributes)
            .start_with_context(&tracer, &cx);
        Some(Self(span))
    }

    /// Record what the response says: its status code and request ids.
    pub(super) fn response(&mut self, response: &HttpResponse) {
        self.0.set_attribute(KeyValue::new(
            "http.response.status_code",
            i64::from(response.status().as_u16()),
        ));
        let headers = response.headers();
        for (header, attribute) in [
            ("x-amz-request-id", "aws.request_id"),
            ("x-amz-id-2", "aws.extended_request_id"),
        ] {
            if let Some(value) = headers.get(header).and_then(|v| v.to_str().ok()) {
                self.0
                    .set_attribute(KeyValue::new(attribute, value.to_owned()));
            }
        }
    }

    /// End the span, `failure` saying how the request failed.
    pub(super) fn end(mut self, failure: Option<Failure>) {
        match failure {
            // Semconv: a request its caller abandoned is not an error.
            None | Some(Failure::Cancelled) => {}
            Some(failure) => {
                self.0.set_attribute(KeyValue::new(
                    ourios_core::telemetry::ERROR_TYPE,
                    failure.error_type(),
                ));
                self.0.set_status(Status::error(""));
            }
        }
        self.0.end();
    }
}

/// The object key `req` addresses in `bucket`, path-style (`/bucket/key`)
/// or virtual-hosted (`bucket.host/key`); empty for a bucket-level request,
/// `None` for a request to anything but the bucket.
fn object_key<'r>(req: &'r HttpRequest, bucket: &str) -> Option<&'r str> {
    let path = req.uri().path();
    let virtual_hosted = req
        .uri()
        .host()
        .and_then(|host| host.strip_prefix(bucket))
        .is_some_and(|rest| rest.starts_with('.'));
    let path_style = path
        .strip_prefix('/')
        .and_then(|p| p.strip_prefix(bucket))
        .and_then(|rest| match rest {
            "" => Some(rest),
            _ => rest.strip_prefix('/'),
        });
    path_style.or_else(|| virtual_hosted.then(|| path.trim_start_matches('/')))
}

/// The S3 API operation a request performs (RFC 0038 §3.7.3's table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum S3Operation {
    ListObjectsV2,
    GetObject,
    HeadObject,
    CopyObject,
    UploadPart,
    PutObject,
    CreateMultipartUpload,
    CompleteMultipartUpload,
    DeleteObjects,
    AbortMultipartUpload,
    DeleteObject,
    Unknown,
}

impl S3Operation {
    fn of(req: &HttpRequest) -> Self {
        let query = req.uri().query().unwrap_or_default();
        let has = |name: &str| {
            query
                .split('&')
                .any(|pair| pair.split('=').next() == Some(name))
        };
        match req.method().as_str() {
            "GET" if has("list-type") => Self::ListObjectsV2,
            "GET" => Self::GetObject,
            "HEAD" => Self::HeadObject,
            "PUT" if req.headers().contains_key("x-amz-copy-source") => Self::CopyObject,
            "PUT" if has("partNumber") => Self::UploadPart,
            "PUT" => Self::PutObject,
            "POST" if has("uploads") => Self::CreateMultipartUpload,
            "POST" if has("uploadId") => Self::CompleteMultipartUpload,
            "POST" if has("delete") => Self::DeleteObjects,
            "DELETE" if has("uploadId") => Self::AbortMultipartUpload,
            "DELETE" => Self::DeleteObject,
            _ => Self::Unknown,
        }
    }

    /// The `rpc.method`, `None` for a request no operation matches.
    fn method(self) -> Option<&'static str> {
        Some(match self {
            Self::ListObjectsV2 => "ListObjectsV2",
            Self::GetObject => "GetObject",
            Self::HeadObject => "HeadObject",
            Self::CopyObject => "CopyObject",
            Self::UploadPart => "UploadPart",
            Self::PutObject => "PutObject",
            Self::CreateMultipartUpload => "CreateMultipartUpload",
            Self::CompleteMultipartUpload => "CompleteMultipartUpload",
            Self::DeleteObjects => "DeleteObjects",
            Self::AbortMultipartUpload => "AbortMultipartUpload",
            Self::DeleteObject => "DeleteObject",
            Self::Unknown => return None,
        })
    }

    /// `Service.Operation`, or the service alone when no operation matches.
    fn span_name(self) -> &'static str {
        match self {
            Self::ListObjectsV2 => "S3.ListObjectsV2",
            Self::GetObject => "S3.GetObject",
            Self::HeadObject => "S3.HeadObject",
            Self::CopyObject => "S3.CopyObject",
            Self::UploadPart => "S3.UploadPart",
            Self::PutObject => "S3.PutObject",
            Self::CreateMultipartUpload => "S3.CreateMultipartUpload",
            Self::CompleteMultipartUpload => "S3.CompleteMultipartUpload",
            Self::DeleteObjects => "S3.DeleteObjects",
            Self::AbortMultipartUpload => "S3.AbortMultipartUpload",
            Self::DeleteObject => "S3.DeleteObject",
            Self::Unknown => "S3",
        }
    }

    /// Whether the operation names one object, so `aws.s3.key` applies.
    fn on_object(self) -> bool {
        !matches!(
            self,
            Self::ListObjectsV2 | Self::DeleteObjects | Self::Unknown
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use object_store::client::{HttpRequest, HttpRequestBody};
    use opentelemetry::trace::{
        Span as _, SpanKind, Status, TraceContextExt as _, TraceId, Tracer as _,
        TracerProvider as _,
    };
    use opentelemetry::{Context, Value};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

    use super::{QueryPhase, S3Operation, object_key};
    use crate::store::Caller;
    use crate::store::metrics::tests::{Harness, fake_s3, s3_store, serve};

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

    #[test]
    fn the_key_is_read_path_style_or_virtual_hosted_and_only_for_the_bucket() {
        let key = |uri| object_key(&request("GET", uri, None), "bucket").map(str::to_owned);
        assert_eq!(
            key("http://h/bucket/data/a.parquet"),
            Some("data/a.parquet".into())
        );
        assert_eq!(key("http://h/bucket?list-type=2"), Some(String::new()));
        assert_eq!(
            key("http://bucket.h/data/a.parquet"),
            Some("data/a.parquet".into())
        );
        assert_eq!(key("http://h/bucket2/k"), None);
        assert_eq!(key("http://169.254.169.254/latest/api/token"), None);
    }

    /// The process-global tracer provider the `CLIENT` spans resolve, over
    /// one in-memory exporter. Shared because the global is per process;
    /// each test reads only its own trace.
    fn exporter() -> &'static InMemorySpanExporter {
        static EXPORTER: OnceLock<InMemorySpanExporter> = OnceLock::new();
        EXPORTER.get_or_init(|| {
            let exporter = InMemorySpanExporter::default();
            let provider = SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build();
            opentelemetry::global::set_tracer_provider(provider);
            exporter
        })
    }

    /// Run `work` under a fresh sampled parent span, the phase marker
    /// attached when `in_phase`; the `CLIENT` spans of its trace.
    fn client_spans_of(in_phase: bool, work: impl FnOnce()) -> (TraceId, Vec<SpanData>) {
        let (trace_id, cx) = parent_context(in_phase);
        {
            let _attached = cx.attach();
            work();
        }
        (trace_id, client_spans(trace_id))
    }

    /// A fresh sampled parent span's context, with the phase marker when
    /// `in_phase`, and its trace id.
    fn parent_context(in_phase: bool) -> (TraceId, Context) {
        exporter();
        let tracer = opentelemetry::global::tracer_provider()
            .tracer_with_scope(ourios_core::telemetry::scope("test"));
        let parent = tracer.start("load template_map");
        let trace_id = parent.span_context().trace_id();
        let cx = Context::current_with_span(parent);
        let cx = if in_phase {
            cx.with_value(QueryPhase)
        } else {
            cx
        };
        (trace_id, cx)
    }

    /// The exported `CLIENT` spans of `trace_id`.
    fn client_spans(trace_id: TraceId) -> Vec<SpanData> {
        exporter()
            .get_finished_spans()
            .expect("spans")
            .into_iter()
            .filter(|s| s.span_context.trace_id() == trace_id && s.span_kind == SpanKind::Client)
            .collect()
    }

    fn attribute(span: &SpanData, key: &str) -> Option<Value> {
        span.attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.clone())
    }

    #[test]
    fn a_failed_request_in_a_query_phase_is_an_error_client_span() {
        let port = fake_s3(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nx-amz-request-id: R1\r\n\
             Connection: close\r\n\r\n",
        ]);
        let harness = Harness::new();
        let store = s3_store(&harness, port, Caller::TemplateMap);
        let (_, spans) = client_spans_of(true, || {
            let err = store.get_blocking("data/a.parquet").expect_err("404");
            assert!(err.is_not_found(), "{err:?}");
        });

        let [span] = spans.as_slice() else {
            panic!("one CLIENT span, got {spans:?}");
        };
        assert_eq!(span.name.as_ref(), "S3.GetObject");
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
        let port = fake_s3(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"abc\"\r\n\
             Last-Modified: Wed, 07 Oct 2026 11:00:00 GMT\r\nConnection: close\r\n\r\nhello",
        ]);
        let harness = Harness::new();
        let store = s3_store(&harness, port, Caller::TemplateMap);
        let (_, spans) = client_spans_of(true, || {
            assert_eq!(store.get_blocking("k").expect("get"), b"hello");
        });
        let [span] = spans.as_slice() else {
            panic!("one CLIENT span, got {spans:?}");
        };
        assert_eq!(span.status, Status::Unset);
        assert_eq!(attribute(span, "error.type"), None);
    }

    #[test]
    fn a_request_outside_a_query_phase_opens_no_client_span() {
        let port = fake_s3(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ]);
        let harness = Harness::new();
        let store = s3_store(&harness, port, Caller::Sink);
        let (_, spans) = client_spans_of(false, || {
            store.get_blocking("k").expect_err("404");
        });
        assert!(spans.is_empty(), "{spans:?}");
    }

    /// A request its caller abandons before the response ends its span
    /// `Unset`, with no `error.type`: semconv does not count an
    /// intentional cancellation as an error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn an_abandoned_request_leaves_its_client_span_unset() {
        use object_store::ObjectStoreExt as _;
        use opentelemetry::context::FutureExt as _;

        let port = serve(1, |_| None);
        let harness = Harness::new();
        let store = s3_store(&harness, port, Caller::TemplateMap);
        let backend = store.object_store();
        let (trace_id, cx) = parent_context(true);
        let request = async move { backend.get(&object_store::path::Path::from("k")).await };
        let abandoned = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            request.with_context(cx),
        )
        .await;
        assert!(abandoned.is_err(), "the fake never answers");

        let spans = client_spans(trace_id);
        let [span] = spans.as_slice() else {
            panic!("one CLIENT span, got {spans:?}");
        };
        assert_eq!(span.name.as_ref(), "S3.GetObject");
        assert_eq!(span.status, Status::Unset);
        assert_eq!(attribute(span, "error.type"), None);
    }
}
