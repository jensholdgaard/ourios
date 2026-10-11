//! Object-store client and blocking-bridge metrics.
//!
//! Three layers, each recorded where it is observable:
//!
//! - **Logical operations** (`ourios.storage.operation.*`,
//!   `ourios.storage.list.objects`): one sample per call on the
//!   [`ObjectStore`](object_store::ObjectStore) a [`Store`](super::Store) hands
//!   out, so `DataFusion`'s reads through [`Store::object_store`](super::Store::object_store)
//!   are measured with the `Store`'s own calls.
//! - **Transport** (the upstream `http.client.request.duration`): one sample per
//!   HTTP attempt the S3 client makes, retries included.
//! - **The bridge** (`ourios.storage.bridge.*`): the sync `*_blocking` calls
//!   parked on the bridge runtime, and the threads that runtime starts.
//!
//! `object_store`'s retry loop exposes no retry count, so retries are
//! counted at the transport: inside one logical operation, an HTTP request
//! repeating an earlier one (same method, URI and `Range`) is a retry of it,
//! and the earlier attempt's outcome is what triggered it. Pagination,
//! multipart parts and the ranges of one `get_ranges` differ in URI or
//! `Range`, so they are not mistaken for retries.

use std::borrow::Cow;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::{BoxStream, Stream, StreamExt};
use object_store::ClientOptions;
use object_store::client::{
    HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpResponse, HttpService,
};
use opentelemetry::metrics::{Counter, Histogram, Meter, UpDownCounter};
use opentelemetry::{KeyValue, global};
use ourios_semconv as semconv;

use super::StoreError;

/// The instrumentation scope the storage metrics are recorded under.
const SCOPE: &str = "ourios.storage";

const ERROR_TYPE: &str = "error.type";
/// The `error.type` fallback when no more specific class is known.
const OTHER: &str = "_OTHER";
/// `error.type` for a call dropped before it completed.
const CANCELLED: &str = "cancelled";

/// The bucket boundaries the HTTP semantic conventions advise for
/// `http.client.request.duration`; the storage durations use them too, so
/// a logical call and its attempts histogram alike.
const DURATION_BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Decades up to a million keys: the `#853` whole-prefix listing lands far
/// above a windowed one.
const LIST_OBJECT_BUCKETS: [f64; 8] = [
    0.0,
    1.0,
    10.0,
    100.0,
    1_000.0,
    10_000.0,
    100_000.0,
    1_000_000.0,
];

/// Threads the bridge runtime has started, workers and blocking-pool
/// threads alike. A process-wide count, since the runtime is.
static BRIDGE_THREADS_CREATED: AtomicU64 = AtomicU64::new(0);

/// Count one thread the bridge runtime started.
pub(super) fn bridge_thread_started() {
    BRIDGE_THREADS_CREATED.fetch_add(1, Ordering::Relaxed);
}

/// The Ourios subsystem a [`Store`](super::Store) handle issues calls for,
/// the `ourios.storage.caller` attribute. Set with
/// [`Store::with_caller`](super::Store::with_caller) where the subsystem
/// takes its handle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Caller {
    /// The query path: file listing, manifests, footers and scans.
    Querier,
    /// The background compactor.
    Compaction,
    /// The ingest record sink writing data files.
    Sink,
    /// An audit sink writing audit events.
    AuditSink,
    /// The querier's cached template map.
    TemplateMap,
    /// The template-id high-water mark.
    TemplateIds,
    /// A handle no subsystem has claimed (tools, tests).
    #[default]
    Other,
}

impl Caller {
    /// The attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Querier => "querier",
            Self::Compaction => "compaction",
            Self::Sink => "sink",
            Self::AuditSink => "audit_sink",
            Self::TemplateMap => "template_map",
            Self::TemplateIds => "template_ids",
            Self::Other => "other",
        }
    }
}

/// The `ourios.storage.operation` attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Get,
    GetRange,
    Put,
    List,
    ListDelimited,
    Head,
    Delete,
    Copy,
}

impl Operation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::GetRange => "get_range",
            Self::Put => "put",
            Self::List => "list",
            Self::ListDelimited => "list_delimited",
            Self::Head => "head",
            Self::Delete => "delete",
            Self::Copy => "copy",
        }
    }
}

/// The storage instruments.
pub(crate) struct StoreMetrics {
    operation_duration: Histogram<f64>,
    retries: Counter<u64>,
    list_objects: Histogram<u64>,
    bridge_active: UpDownCounter<i64>,
    bridge_duration: Histogram<f64>,
    http_duration: Histogram<f64>,
}

impl std::fmt::Debug for StoreMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoreMetrics")
    }
}

impl StoreMetrics {
    pub(crate) fn from_meter(meter: &Meter) -> Self {
        meter
            .u64_observable_counter(semconv::OURIOS_STORAGE_BRIDGE_THREAD_CREATED)
            .with_unit("{thread}")
            .with_callback(|observer| {
                observer.observe(BRIDGE_THREADS_CREATED.load(Ordering::Relaxed), &[]);
            })
            .build();
        Self {
            operation_duration: meter
                .f64_histogram(semconv::OURIOS_STORAGE_OPERATION_DURATION)
                .with_unit("s")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            retries: meter
                .u64_counter(semconv::OURIOS_STORAGE_OPERATION_RETRIES)
                .with_unit("{retry}")
                .build(),
            list_objects: meter
                .u64_histogram(semconv::OURIOS_STORAGE_LIST_OBJECTS)
                .with_unit("{object}")
                .with_boundaries(LIST_OBJECT_BUCKETS.to_vec())
                .build(),
            bridge_active: meter
                .i64_up_down_counter(semconv::OURIOS_STORAGE_BRIDGE_CALL_ACTIVE)
                .with_unit("{call}")
                .build(),
            bridge_duration: meter
                .f64_histogram(semconv::OURIOS_STORAGE_BRIDGE_DURATION)
                .with_unit("s")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            http_duration: meter
                .f64_histogram("http.client.request.duration")
                .with_unit("s")
                .with_description("Duration of HTTP client requests.")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
        }
    }

    fn record_operation(&self, labels: Labels, took: Duration, error: Option<&'static str>) {
        let took = took.as_secs_f64();
        match error {
            None => self.operation_duration.record(took, &labels.attributes()),
            Some(class) => {
                let [operation, caller] = labels.attributes();
                self.operation_duration
                    .record(took, &[operation, caller, KeyValue::new(ERROR_TYPE, class)]);
            }
        }
    }

    fn record_listing(&self, caller: Caller, objects: u64) {
        self.list_objects.record(
            objects,
            &[KeyValue::new(
                semconv::OURIOS_STORAGE_CALLER,
                caller.as_str(),
            )],
        );
    }

    /// Mark one bridged call in flight; the guard ends it.
    pub(super) fn bridge_call(&self, op: Operation) -> BridgeCall<'_> {
        self.bridge_active.add(1, &[]);
        BridgeCall {
            metrics: self,
            op,
            started: Instant::now(),
        }
    }
}

/// A bridged call in flight, from [`StoreMetrics::bridge_call`].
pub(super) struct BridgeCall<'m> {
    metrics: &'m StoreMetrics,
    op: Operation,
    started: Instant,
}

impl BridgeCall<'_> {
    /// End the call, `error` classifying a failure.
    pub(super) fn end(self, error: Option<&'static str>) {
        let operation = KeyValue::new(semconv::OURIOS_STORAGE_OPERATION, self.op.as_str());
        let took = self.started.elapsed().as_secs_f64();
        match error {
            None => self.metrics.bridge_duration.record(took, &[operation]),
            Some(class) => self
                .metrics
                .bridge_duration
                .record(took, &[operation, KeyValue::new(ERROR_TYPE, class)]),
        }
    }
}

impl Drop for BridgeCall<'_> {
    fn drop(&mut self) {
        self.metrics.bridge_active.add(-1, &[]);
    }
}

/// Where a handle records: the process-global meter, resolved on first use
/// (so a handle opened before telemetry starts still reports once it has),
/// or instruments of its own.
#[derive(Clone, Debug, Default)]
pub(crate) enum Metrics {
    #[default]
    Global,
    #[cfg_attr(not(test), allow(dead_code))] // only tests own instruments today
    Owned(Arc<StoreMetrics>),
}

impl Metrics {
    pub(crate) fn get(&self) -> &StoreMetrics {
        static GLOBAL: LazyLock<StoreMetrics> =
            LazyLock::new(|| StoreMetrics::from_meter(&global::meter(SCOPE)));
        match self {
            Self::Global => &GLOBAL,
            Self::Owned(metrics) => metrics,
        }
    }
}

/// `error.type` for an `object_store` failure: the variant, or for a
/// transport failure the kind of HTTP error underneath it.
pub(crate) fn error_type(e: &object_store::Error) -> &'static str {
    use object_store::Error;
    match e {
        Error::NotFound { .. } => "not_found",
        Error::AlreadyExists { .. } => "already_exists",
        Error::Precondition { .. } => "precondition",
        Error::NotModified { .. } => "not_modified",
        Error::PermissionDenied { .. } => "permission_denied",
        Error::Unauthenticated { .. } => "unauthenticated",
        Error::InvalidPath { .. } => "invalid_path",
        Error::NotSupported { .. } => "not_supported",
        Error::NotImplemented { .. } => "not_implemented",
        _ => transport_kind(e).map_or(OTHER, http_error_kind),
    }
}

/// `error.type` for a [`StoreError`].
pub(crate) fn store_error_type(e: &StoreError) -> &'static str {
    match e {
        StoreError::Backend(e) => error_type(e),
        StoreError::Runtime(_) => "runtime",
        StoreError::Config(_) => "config",
    }
}

fn transport_kind(e: &(dyn std::error::Error + 'static)) -> Option<HttpErrorKind> {
    let mut source = Some(e);
    while let Some(e) = source {
        if let Some(http) = e.downcast_ref::<HttpError>() {
            return Some(http.kind());
        }
        source = e.source();
    }
    None
}

fn http_error_kind(kind: HttpErrorKind) -> &'static str {
    match kind {
        HttpErrorKind::Connect => "connect",
        HttpErrorKind::Request => "request",
        HttpErrorKind::Timeout => "timeout",
        HttpErrorKind::Interrupted => "interrupted",
        HttpErrorKind::Decode => "decode",
        _ => OTHER,
    }
}

/// The attributes every sample of one logical call carries.
#[derive(Clone, Copy, Debug)]
struct Labels {
    op: Operation,
    caller: Caller,
}

impl Labels {
    fn attributes(self) -> [KeyValue; 2] {
        [
            KeyValue::new(semconv::OURIOS_STORAGE_OPERATION, self.op.as_str()),
            KeyValue::new(semconv::OURIOS_STORAGE_CALLER, self.caller.as_str()),
        ]
    }
}

/// How one HTTP attempt failed: its `error.type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Status(u16),
    Transport(&'static str),
    Cancelled,
}

impl Failure {
    fn error_type(self) -> Cow<'static, str> {
        match self {
            Self::Status(code) => Cow::Owned(code.to_string()),
            Self::Transport(kind) => Cow::Borrowed(kind),
            Self::Cancelled => Cow::Borrowed(CANCELLED),
        }
    }
}

tokio::task_local! {
    /// The logical operation the HTTP attempts polled in this task belong to.
    static OPERATION: Arc<Attempts>;
}

/// The HTTP attempts one logical operation made, by request identity, with
/// how the latest attempt of each ended (`None`: it got a response under 400).
struct Attempts {
    metrics: Metrics,
    labels: Labels,
    seen: Mutex<Vec<(u64, Option<Failure>)>>,
}

impl Attempts {
    /// Note an attempt at `identity`; when it repeats an earlier one, count
    /// a retry classified by how that earlier attempt ended.
    fn attempt(&self, identity: u64) {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((_, earlier)) = seen.iter().find(|(id, _)| *id == identity) else {
            return;
        };
        // The earlier attempt got a response under 400, so what failed it is
        // not visible here.
        let class = earlier.map_or(Cow::Borrowed(OTHER), Failure::error_type);
        let [operation, caller] = self.labels.attributes();
        self.metrics
            .get()
            .retries
            .add(1, &[operation, caller, KeyValue::new(ERROR_TYPE, class)]);
    }

    fn ended(&self, identity: u64, failure: Option<Failure>) {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        match seen.iter_mut().find(|(id, _)| *id == identity) {
            Some(entry) => entry.1 = failure,
            None => seen.push((identity, failure)),
        }
    }
}

/// One logical operation, recorded when it ends. Dropped before it ends,
/// it records as cancelled.
pub(super) struct Call {
    metrics: Metrics,
    labels: Labels,
    started: Instant,
    attempts: Option<Arc<Attempts>>,
    /// Objects listed so far, for a LIST.
    listed: Option<u64>,
    done: bool,
}

impl Call {
    /// Start a call; `http` when its backend is reached over HTTP, whose
    /// attempts are then tracked for retries.
    pub(super) fn start(metrics: &Metrics, op: Operation, caller: Caller, http: bool) -> Self {
        let labels = Labels { op, caller };
        let attempts = http.then(|| {
            Arc::new(Attempts {
                metrics: metrics.clone(),
                labels,
                seen: Mutex::new(Vec::new()),
            })
        });
        Self {
            metrics: metrics.clone(),
            labels,
            started: Instant::now(),
            attempts,
            listed: matches!(op, Operation::List | Operation::ListDelimited).then_some(0),
            done: false,
        }
    }

    /// Drive `fut` as part of this call.
    pub(super) async fn run<F: Future>(&self, fut: F) -> F::Output {
        match &self.attempts {
            Some(attempts) => OPERATION.scope(Arc::clone(attempts), fut).await,
            None => fut.await,
        }
    }

    /// Count `n` listed objects.
    pub(super) fn listed(&mut self, n: usize) {
        if let Some(listed) = &mut self.listed {
            *listed = listed.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        }
    }

    /// End the call with `result`.
    pub(super) fn end<T>(mut self, result: &object_store::Result<T>) {
        self.finish(result.as_ref().err().map(error_type));
    }

    fn finish(&mut self, error: Option<&'static str>) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let metrics = self.metrics.get();
        metrics.record_operation(self.labels, self.started.elapsed(), error);
        if let (None, Some(listed)) = (error, self.listed) {
            metrics.record_listing(self.labels.caller, listed);
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.finish(Some(CANCELLED));
    }
}

/// A stream answering one logical call (a LIST, a GET's body, a DELETE
/// batch): the call ends when the stream does, and the stream is polled
/// inside the call so its HTTP attempts count toward it.
pub(super) struct Observed<T> {
    inner: BoxStream<'static, object_store::Result<T>>,
    call: Option<Call>,
}

impl<T: Send + 'static> Observed<T> {
    pub(super) fn boxed(
        inner: BoxStream<'static, object_store::Result<T>>,
        call: Call,
    ) -> BoxStream<'static, object_store::Result<T>> {
        Box::pin(Self {
            inner,
            call: Some(call),
        })
    }
}

impl<T> Stream for Observed<T> {
    type Item = object_store::Result<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let polled = match this.call.as_ref().and_then(|call| call.attempts.clone()) {
            Some(attempts) => OPERATION.sync_scope(attempts, || this.inner.poll_next_unpin(cx)),
            None => this.inner.poll_next_unpin(cx),
        };
        match &polled {
            Poll::Ready(Some(Ok(_))) => {
                if let Some(call) = &mut this.call {
                    call.listed(1);
                }
            }
            Poll::Ready(Some(Err(e))) => {
                if let Some(mut call) = this.call.take() {
                    call.finish(Some(error_type(e)));
                }
            }
            Poll::Ready(None) => {
                if let Some(mut call) = this.call.take() {
                    call.finish(None);
                }
            }
            Poll::Pending => {}
        }
        polled
    }
}

/// The S3 client's connector, timing every HTTP attempt.
#[derive(Debug)]
pub(super) struct MeteredConnector<C> {
    pub(super) inner: C,
    pub(super) metrics: Metrics,
}

impl<C: HttpConnector> HttpConnector for MeteredConnector<C> {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        Ok(HttpClient::new(MeteredHttp {
            inner: self.inner.connect(options)?,
            metrics: self.metrics.clone(),
        }))
    }
}

#[derive(Debug)]
struct MeteredHttp {
    inner: HttpClient,
    metrics: Metrics,
}

#[async_trait]
impl HttpService for MeteredHttp {
    async fn call(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let scope = OPERATION.try_with(Arc::clone).ok().map(|attempts| {
            let identity = request_identity(&req);
            attempts.attempt(identity);
            (attempts, identity)
        });
        let mut attempt = Attempt {
            metrics: self.metrics.clone(),
            attributes: request_attributes(&req),
            started: Instant::now(),
            scope,
            done: false,
        };
        let response = self.inner.execute(req).await;
        match &response {
            Ok(response) => {
                let status = response.status().as_u16();
                attempt.finish(
                    Some(status),
                    (status >= 400).then_some(Failure::Status(status)),
                );
            }
            Err(e) => attempt.finish(None, Some(Failure::Transport(http_error_kind(e.kind())))),
        }
        response
    }
}

/// One HTTP attempt, recorded when it ends. Dropped before a response, as
/// when its caller gives up on the request, it records as cancelled.
struct Attempt {
    metrics: Metrics,
    attributes: Vec<KeyValue>,
    started: Instant,
    /// The logical operation it belongs to, and its request identity.
    scope: Option<(Arc<Attempts>, u64)>,
    done: bool,
}

impl Attempt {
    fn finish(&mut self, status: Option<u16>, failure: Option<Failure>) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        let took = self.started.elapsed().as_secs_f64();
        let mut attributes = std::mem::take(&mut self.attributes);
        if let Some(status) = status {
            attributes.push(KeyValue::new(
                "http.response.status_code",
                i64::from(status),
            ));
        }
        if let Some(failure) = failure {
            attributes.push(KeyValue::new(ERROR_TYPE, failure.error_type()));
        }
        self.metrics.get().http_duration.record(took, &attributes);
        if let Some((attempts, identity)) = &self.scope {
            attempts.ended(*identity, failure);
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.finish(None, Some(Failure::Cancelled));
    }
}

/// What makes two requests the same request: method, URI and `Range`, so
/// the parallel ranged GETs of one `get_ranges` stay distinct. Other headers
/// are left out, since a retry re-signs. A body retry resumes with a
/// narrower `Range`, so it is not counted.
fn request_identity(req: &HttpRequest) -> u64 {
    let mut hasher = DefaultHasher::new();
    req.method().hash(&mut hasher);
    req.uri().hash(&mut hasher);
    req.headers()
        .get("range")
        .map(object_store::HeaderValue::as_bytes)
        .hash(&mut hasher);
    hasher.finish()
}

/// The HTTP client attributes known before the response.
fn request_attributes(req: &HttpRequest) -> Vec<KeyValue> {
    // Spelled out because `as_str` borrows the request and the attribute
    // needs a `'static` value.
    let method = match req.method().as_str() {
        "GET" => "GET",
        "HEAD" => "HEAD",
        "PUT" => "PUT",
        "POST" => "POST",
        "DELETE" => "DELETE",
        "PATCH" => "PATCH",
        "OPTIONS" => "OPTIONS",
        _ => OTHER,
    };
    let uri = req.uri();
    let mut attributes = Vec::with_capacity(5);
    attributes.push(KeyValue::new("http.request.method", method));
    if let Some(host) = uri.host() {
        attributes.push(KeyValue::new("server.address", host.to_owned()));
    }
    let port = uri.port_u16().or(match uri.scheme_str() {
        Some("https") => Some(443),
        Some("http") => Some(80),
        _ => None,
    });
    if let Some(port) = port {
        attributes.push(KeyValue::new("server.port", i64::from(port)));
    }
    attributes
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{
        AggregatedMetrics, MetricData, ResourceMetrics, ScopeMetrics,
    };
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
    use ourios_semconv as semconv;

    use super::{Caller, Metrics, Operation, StoreMetrics};
    use crate::store::{MAX_BRIDGE_WORKERS, S3Config, Store, StoreError, block_on_off_runtime};

    /// Storage instruments of their own, exporting in memory.
    struct Harness {
        provider: SdkMeterProvider,
        exporter: InMemoryMetricExporter,
        metrics: Metrics,
    }

    /// One data point: its attributes, and its count and sum (a sum's
    /// value is its `sum`, with a zero `count`).
    #[derive(Debug)]
    struct Point {
        attributes: BTreeMap<String, String>,
        count: u64,
        sum: f64,
    }

    impl Point {
        fn attribute(&self, key: &str) -> Option<&str> {
            self.attributes.get(key).map(String::as_str)
        }
    }

    impl Harness {
        fn new() -> Self {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_periodic_exporter(exporter.clone())
                .build();
            let metrics = Metrics::Owned(Arc::new(StoreMetrics::from_meter(
                &provider.meter("ourios.storage"),
            )));
            Self {
                provider,
                exporter,
                metrics,
            }
        }

        fn in_memory(&self, caller: Caller) -> Store {
            Store::in_memory()
                .with_metrics(self.metrics.clone())
                .with_caller(caller)
        }

        fn collect(&self) -> Vec<ResourceMetrics> {
            self.exporter.reset();
            self.provider.force_flush().expect("flush");
            self.exporter.get_finished_metrics().expect("exported")
        }

        fn points(&self, name: &str) -> Vec<Point> {
            let collected = self.collect();
            let mut points = Vec::new();
            for metric in collected
                .iter()
                .flat_map(ResourceMetrics::scope_metrics)
                .flat_map(ScopeMetrics::metrics)
                .filter(|metric| metric.name() == name)
            {
                let attributes = |kvs: &mut dyn Iterator<Item = &opentelemetry::KeyValue>| {
                    kvs.map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                        .collect::<BTreeMap<_, _>>()
                };
                #[allow(clippy::cast_precision_loss)] // test counts stay tiny
                match metric.data() {
                    AggregatedMetrics::F64(MetricData::Histogram(h)) => {
                        points.extend(h.data_points().map(|p| Point {
                            attributes: attributes(&mut p.attributes()),
                            count: p.count(),
                            sum: p.sum(),
                        }));
                    }
                    AggregatedMetrics::U64(MetricData::Histogram(h)) => {
                        points.extend(h.data_points().map(|p| Point {
                            attributes: attributes(&mut p.attributes()),
                            count: p.count(),
                            sum: p.sum() as f64,
                        }));
                    }
                    AggregatedMetrics::U64(MetricData::Sum(s)) => {
                        points.extend(s.data_points().map(|p| Point {
                            attributes: attributes(&mut p.attributes()),
                            count: 0,
                            sum: p.value() as f64,
                        }));
                    }
                    AggregatedMetrics::I64(MetricData::Sum(s)) => {
                        points.extend(s.data_points().map(|p| Point {
                            attributes: attributes(&mut p.attributes()),
                            count: 0,
                            sum: p.value() as f64,
                        }));
                    }
                    other => panic!("{name}: unexpected data {other:?}"),
                }
            }
            points
        }

        /// The operation-duration samples, by `(operation, caller,
        /// error.type)`.
        fn operations(&self) -> BTreeMap<(String, String, Option<String>), u64> {
            self.points(semconv::OURIOS_STORAGE_OPERATION_DURATION)
                .into_iter()
                .map(|p| {
                    let key = (
                        p.attribute(semconv::OURIOS_STORAGE_OPERATION)
                            .expect("operation")
                            .to_owned(),
                        p.attribute(semconv::OURIOS_STORAGE_CALLER)
                            .expect("caller")
                            .to_owned(),
                        p.attribute("error.type").map(str::to_owned),
                    );
                    (key, p.count)
                })
                .collect()
        }

        fn active_calls(&self) -> f64 {
            self.points(semconv::OURIOS_STORAGE_BRIDGE_CALL_ACTIVE)
                .iter()
                .map(|p| p.sum)
                .sum()
        }

        fn threads_created(&self) -> f64 {
            match self
                .points(semconv::OURIOS_STORAGE_BRIDGE_THREAD_CREATED)
                .as_slice()
            {
                [point] => point.sum,
                other => panic!("one thread-created point, got {other:?}"),
            }
        }
    }

    fn key(op: &str, caller: &str, error: Option<&str>) -> (String, String, Option<String>) {
        (op.to_owned(), caller.to_owned(), error.map(str::to_owned))
    }

    /// Every operation kind on `store` records one sample under its
    /// operation and the handle's caller, and a LIST its object count.
    fn records_each_operation(harness: &Harness, store: &Store, caller: &str) {
        for name in ["a", "b", "c"] {
            store
                .put_blocking(&format!("p/{name}"), b"body".to_vec())
                .expect("put");
        }
        store.get_blocking("p/a").expect("get");
        store.get_suffix_blocking("p/a", 2).expect("suffix");
        assert_eq!(store.list_blocking(Some("p")).expect("list").len(), 3);
        store.list_delimited_blocking(None).expect("delimited");
        store.delete_blocking("p/c").expect("delete");

        let operations = harness.operations();
        let expected = BTreeMap::from([
            (key("put", caller, None), 3),
            (key("get", caller, None), 1),
            (key("get_range", caller, None), 1),
            (key("list", caller, None), 1),
            (key("list_delimited", caller, None), 1),
            (key("delete", caller, None), 1),
        ]);
        assert_eq!(operations, expected);

        let listed = harness.points(semconv::OURIOS_STORAGE_LIST_OBJECTS);
        assert!(
            listed
                .iter()
                .all(|p| p.attribute(semconv::OURIOS_STORAGE_CALLER) == Some(caller)),
            "{listed:?}"
        );
        // The recursive LIST found the three objects; the delimited one,
        // one level above them, none.
        assert_eq!(listed.iter().map(|p| p.count).sum::<u64>(), 2);
        assert!((listed.iter().map(|p| p.sum).sum::<f64>() - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_in_memory_store_records_each_operation_under_its_caller() {
        let harness = Harness::new();
        let store = harness.in_memory(Caller::Sink);
        records_each_operation(&harness, &store, "sink");
    }

    #[test]
    fn the_local_store_records_each_operation_under_its_caller() {
        let harness = Harness::new();
        let dir = tempfile::TempDir::new().expect("temp dir");
        let store = Store::local(dir.path())
            .expect("local")
            .with_metrics(harness.metrics.clone())
            .with_caller(Caller::Compaction);
        records_each_operation(&harness, &store, "compaction");
        assert!(
            harness.points("http.client.request.duration").is_empty(),
            "the local backend makes no HTTP requests"
        );
    }

    #[test]
    fn a_failing_call_carries_its_error_type() {
        let harness = Harness::new();
        let store = harness.in_memory(Caller::Querier);
        let err = store.get_blocking("absent").expect_err("missing object");
        assert!(err.is_not_found(), "{err:?}");

        assert_eq!(
            harness.operations(),
            BTreeMap::from([(key("get", "querier", Some("not_found")), 1)])
        );
        let bridged = harness.points(semconv::OURIOS_STORAGE_BRIDGE_DURATION);
        assert!(
            matches!(
                bridged.as_slice(),
                [p] if p.attribute("error.type") == Some("not_found")
                    && p.attribute(semconv::OURIOS_STORAGE_OPERATION) == Some("get")
                    && p.count == 1
            ),
            "{bridged:?}"
        );
        assert!(
            harness
                .points(semconv::OURIOS_STORAGE_LIST_OBJECTS)
                .is_empty()
        );
    }

    #[test]
    fn concurrent_bridged_calls_are_active_until_they_return() {
        const CALLS: usize = 8;
        let harness = Harness::new();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        std::thread::scope(|s| {
            for _ in 0..CALLS {
                let (gate, metrics) = (Arc::clone(&gate), harness.metrics.clone());
                s.spawn(move || {
                    block_on_off_runtime(&metrics, Operation::Get, async move {
                        let _permit = gate.acquire().await.map_err(|_| {
                            StoreError::Runtime(std::io::Error::other("gate closed"))
                        })?;
                        Ok(())
                    })
                    .expect("bridged call");
                });
            }
            let deadline = Instant::now() + Duration::from_secs(10);
            while harness.active_calls() < 8.0 {
                assert!(Instant::now() < deadline, "the calls never all parked");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!((harness.active_calls() - 8.0).abs() < f64::EPSILON);
            gate.add_permits(CALLS);
        });
        assert!(harness.active_calls().abs() < f64::EPSILON);
        let durations = harness.points(semconv::OURIOS_STORAGE_BRIDGE_DURATION);
        assert!(
            matches!(
                durations.as_slice(),
                [p] if p.count == 8 && p.attribute("error.type").is_none()
            ),
            "{durations:?}"
        );
    }

    /// The bridge runs every call on its runtime's long-lived threads, so
    /// the count of threads it created stays flat however many calls pass.
    /// The runtime is process-wide, and other tests in this process may
    /// grow its blocking pool meanwhile, so the bound is loose.
    #[test]
    fn sequential_bridged_calls_do_not_create_threads() {
        const CALLS: usize = 10_000;
        let harness = Harness::new();
        let store = harness.in_memory(Caller::Other);
        store
            .put_blocking("k", b"v".to_vec())
            .expect("warm the bridge");
        let before = harness.threads_created();
        assert!(before >= 1.0, "the bridge runtime's workers are counted");
        for _ in 0..CALLS {
            store.get_bytes_blocking("k").expect("get");
        }
        let created = harness.threads_created() - before;
        assert!(
            created <= f64::from(u32::try_from(4 * MAX_BRIDGE_WORKERS).expect("small")),
            "{CALLS} bridged calls created {created} threads"
        );
    }

    /// A fake S3 endpoint answering each request on its own connection with
    /// the next of `responses`. Returns its port.
    fn fake_s3(responses: Vec<&'static str>) -> u16 {
        let count = responses.len();
        let mut responses = responses.into_iter();
        serve(count, move |_| responses.next().map(str::to_owned))
    }

    /// A fake S3 endpoint serving `connections` connections, one request
    /// each, answered by `respond` from the request head. `None` leaves the
    /// request unanswered until the client hangs up. Returns its port.
    fn serve(
        connections: usize,
        mut respond: impl FnMut(&str) -> Option<String> + Send + 'static,
    ) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for _ in 0..connections {
                let (mut conn, _) = listener.accept().expect("accept");
                let request = read_head(&mut conn);
                answer(&mut conn, respond(&request));
            }
        });
        port
    }

    /// Read a request up to the end of its head.
    fn read_head(conn: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buf = [0_u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = conn.read(&mut buf).expect("read");
            assert!(n > 0, "request ended early");
            request.extend_from_slice(&buf[..n]);
        }
        String::from_utf8_lossy(&request).into_owned()
    }

    /// Send `response`, or with none, hold the connection until the client
    /// hangs up.
    fn answer(conn: &mut std::net::TcpStream, response: Option<String>) {
        let Some(response) = response else {
            let mut buf = [0_u8; 1024];
            while conn.read(&mut buf).is_ok_and(|n| n > 0) {}
            return;
        };
        conn.write_all(response.as_bytes()).expect("write");
    }

    fn s3_store(harness: &Harness, port: u16, caller: Caller) -> Store {
        let cfg = S3Config::new("bucket")
            .with_endpoint(format!("http://127.0.0.1:{port}"))
            .with_region("us-east-1")
            .with_access_key_id("key")
            .with_secret_access_key("secret");
        Store::s3_with(cfg, harness.metrics.clone())
            .expect("s3")
            .with_caller(caller)
    }

    #[test]
    fn a_retried_s3_get_records_one_operation_one_retry_and_two_attempts() {
        let port = fake_s3(vec![
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nETag: \"abc\"\r\n\
             Last-Modified: Wed, 07 Oct 2026 11:00:00 GMT\r\nConnection: close\r\n\r\nhello",
        ]);
        let harness = Harness::new();
        let store = s3_store(&harness, port, Caller::TemplateMap);

        assert_eq!(store.get_blocking("k").expect("get"), b"hello");

        assert_eq!(
            harness.operations(),
            BTreeMap::from([(key("get", "template_map", None), 1)])
        );
        let retries = harness.points(semconv::OURIOS_STORAGE_OPERATION_RETRIES);
        assert!(
            matches!(
                retries.as_slice(),
                [p] if (p.sum - 1.0).abs() < f64::EPSILON
                    && p.attribute("error.type") == Some("500")
                    && p.attribute(semconv::OURIOS_STORAGE_OPERATION) == Some("get")
                    && p.attribute(semconv::OURIOS_STORAGE_CALLER) == Some("template_map")
            ),
            "{retries:?}"
        );
        let attempts: BTreeMap<Option<String>, (u64, Option<String>)> = harness
            .points("http.client.request.duration")
            .into_iter()
            .map(|p| {
                assert_eq!(p.attribute("http.request.method"), Some("GET"));
                assert_eq!(p.attribute("server.address"), Some("127.0.0.1"));
                assert_eq!(p.attribute("server.port"), Some(port.to_string().as_str()));
                (
                    p.attribute("error.type").map(str::to_owned),
                    (
                        p.count,
                        p.attribute("http.response.status_code").map(str::to_owned),
                    ),
                )
            })
            .collect();
        assert_eq!(
            attempts,
            BTreeMap::from([
                (Some("500".to_owned()), (1, Some("500".to_owned()))),
                (None, (1, Some("200".to_owned()))),
            ])
        );
    }

    /// The size of the object the ranged-GET fake serves.
    const RANGED_SIZE: u64 = 40_000_000;

    /// A 206 for the `Range` in `request`, cut from an object of
    /// [`RANGED_SIZE`] bytes.
    fn partial_content(request: &str) -> Option<String> {
        let range = request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("range")
                .then(|| value.trim().strip_prefix("bytes=").map(str::to_owned))
                .flatten()
        })?;
        let (first, last) = range.split_once('-')?;
        let (first, last): (u64, u64) = (first.parse().ok()?, last.parse().ok()?);
        let length = last - first + 1;
        Some(format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\n\
             Content-Range: bytes {first}-{last}/{RANGED_SIZE}\r\nETag: \"abc\"\r\n\
             Last-Modified: Wed, 07 Oct 2026 11:00:00 GMT\r\nConnection: close\r\n\r\n{}",
            "x".repeat(usize::try_from(length).ok()?)
        ))
    }

    /// More ranges than `get_ranges` fetches at once.
    const RANGES: usize = 12;

    /// Ranges too far apart to coalesce are GETs of one URI, the later ones
    /// starting as earlier ones end, and none is a retry of another.
    #[test]
    fn the_parallel_gets_of_one_multi_range_read_are_not_retries() {
        let port = serve(RANGES, partial_content);
        let harness = Harness::new();
        let store = s3_store(&harness, port, Caller::Querier);
        let backend = store.object_store();
        let path = object_store::path::Path::from("k");
        let ranges: Vec<std::ops::Range<u64>> = (0..RANGES as u64)
            .map(|i| i * 3_000_000..i * 3_000_000 + 10)
            .collect();
        let got = store
            .bridge(Operation::GetRange, async move {
                backend
                    .get_ranges(&path, &ranges)
                    .await
                    .map_err(StoreError::Backend)
            })
            .expect("get_ranges");
        assert_eq!(
            got.iter().map(bytes::Bytes::len).collect::<Vec<_>>(),
            [10; RANGES]
        );

        assert_eq!(
            harness.operations(),
            BTreeMap::from([(key("get_range", "querier", None), 1)])
        );
        assert!(
            harness
                .points(semconv::OURIOS_STORAGE_OPERATION_RETRIES)
                .is_empty()
        );
        let attempts = harness.points("http.client.request.duration");
        assert!(
            matches!(
                attempts.as_slice(),
                [p] if p.count == RANGES as u64
                    && p.attribute("http.response.status_code") == Some("206")
                    && p.attribute("error.type").is_none()
            ),
            "{attempts:?}"
        );
    }

    /// A request its caller gives up on before the response still records
    /// its attempt, as cancelled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn an_abandoned_s3_request_records_a_cancelled_attempt() {
        use object_store::ObjectStoreExt as _;

        let port = serve(1, |_| None);
        let harness = Harness::new();
        let store = s3_store(&harness, port, Caller::Querier);
        let backend = store.object_store();
        let path = object_store::path::Path::from("k");
        let abandoned = tokio::time::timeout(Duration::from_millis(300), backend.get(&path)).await;
        assert!(abandoned.is_err(), "the fake never answers");

        assert_eq!(
            harness.operations(),
            BTreeMap::from([(key("get", "querier", Some("cancelled")), 1)])
        );
        let attempts = harness.points("http.client.request.duration");
        assert!(
            matches!(
                attempts.as_slice(),
                [p] if p.count == 1
                    && p.attribute("error.type") == Some("cancelled")
                    && p.attribute("http.response.status_code").is_none()
                    && p.attribute("http.request.method") == Some("GET")
            ),
            "{attempts:?}"
        );
    }
}
