//! The compaction sweep against a fake `OpenFGA` store: the RFC 0047
//! graph feed and erasure.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use ourios_core::audit::{AuditEvent, AuditPayload, AuditSink, SharedAuditSink};
use ourios_core::auth::openfga::{OpenFgaSpec, build_openfga_config};
use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::MinedRecord;
use ourios_parquet::{
    CompactionPolicy, PartitionKey, PromotedAttributes, PromotedKey, Reader, Store, Writer,
};
use ourios_serving::openfga::TupleKey;
use serde_json::json;

use super::{
    ErasureOutcome, ErasurePhase, IngestError, SweepReport, SweepTarget, pending_erasures,
    request_erasure, sweep_once,
};
use crate::graph_emitter::GraphEmitter;

/// A fake `OpenFGA` store: `/write` applies writes/deletes (asserting the
/// ≤ 100 chunk), `/read` answers by object.
#[derive(Clone, Default)]
struct Fake {
    tuples: Arc<Mutex<Vec<TupleKey>>>,
    writes: Arc<Mutex<Vec<usize>>>,
}

fn json(value: &serde_json::Value) -> ([(&'static str, &'static str); 1], String) {
    ([("content-type", "application/json")], value.to_string())
}

/// The tuple keys of a `Write` request's `section` (`writes` / `deletes`),
/// asserting the ≤ 100 chunk and the idempotency `flag` the section carries.
fn section_keys(request: &serde_json::Value, section: &str, flag: &str) -> Vec<TupleKey> {
    let Some(keys) = request[section]["tuple_keys"].as_array() else {
        return Vec::new();
    };
    assert!(keys.len() <= 100, "RFC 0047 §3.3: ≤ 100 tuples per Write");
    assert_eq!(request[section][flag], "ignore");
    keys.iter()
        .map(|key| serde_json::from_value(key.clone()).expect("tuple"))
        .collect()
}

async fn write(
    State(fake): State<Fake>,
    body: axum::body::Bytes,
) -> ([(&'static str, &'static str); 1], String) {
    let request: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let mut tuples = fake.tuples.lock().expect("lock");
    if request["writes"]["tuple_keys"].is_array() {
        let writes = section_keys(&request, "writes", "on_duplicate");
        fake.writes.lock().expect("lock").push(writes.len());
        for key in writes {
            if !tuples.contains(&key) {
                tuples.push(key);
            }
        }
    }
    for key in section_keys(&request, "deletes", "on_missing") {
        tuples.retain(|t| *t != key);
    }
    json(&json!({}))
}

async fn read(
    State(fake): State<Fake>,
    body: axum::body::Bytes,
) -> ([(&'static str, &'static str); 1], String) {
    let request: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let object = request["tuple_key"]["object"].as_str().expect("object");
    let tuples = fake.tuples.lock().expect("lock");
    let matching: Vec<serde_json::Value> = tuples
        .iter()
        .filter(|t| t.object == object)
        .map(|t| json!({ "key": t }))
        .collect();
    json(&json!({ "tuples": matching, "continuation_token": "" }))
}

async fn serve(fake: Fake) -> String {
    let app = Router::new()
        .route("/stores/{store}/write", post(write))
        .route("/stores/{store}/read", post(read))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    url
}

fn emitter(url: &str) -> Arc<GraphEmitter> {
    use ourios_core::auth::openfga::{VisibilityObjectSpec, VisibilitySpec};
    let config = build_openfga_config(&OpenFgaSpec {
        api_url: Some(url.to_string()),
        store_id: Some("s".to_string()),
        request_timeout_secs: Some("2".to_string()),
        visibility: VisibilitySpec {
            objects: vec![VisibilityObjectSpec {
                object_type: Some("conversation".to_string()),
                column: Some("attr.gen_ai.conversation.id".to_string()),
            }],
            ..VisibilitySpec::default()
        },
        ..OpenFgaSpec::default()
    })
    .expect("config");
    Arc::new(
        GraphEmitter::from_config(&config)
            .expect("client")
            .expect("bound"),
    )
}

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(Value::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

fn promoted() -> PromotedAttributes {
    PromotedAttributes::new_typed(
        [],
        [
            PromotedKey::string("gen_ai.conversation.id".to_string()),
            PromotedKey::string("user.hash".to_string()),
        ],
    )
}

/// `n` rows of one conversation by `user` (and `agent`), as one file in the
/// sealed hour partition.
struct Rows<'a> {
    conversation: &'a str,
    user: &'a str,
    agent: Option<&'a str>,
    n: u64,
}

impl Rows<'_> {
    fn of<'a>(conversation: &'a str, user: &'a str, n: u64) -> Rows<'a> {
        Rows {
            conversation,
            user,
            agent: None,
            n,
        }
    }

    fn record(&self, i: u64) -> MinedRecord {
        let mut r = super::tests::rec("acme", 1, super::tests::TS0 + i * 1_000);
        r.attributes = vec![
            kv("gen_ai.conversation.id", self.conversation),
            kv("user.hash", self.user),
        ];
        if let Some(agent) = self.agent {
            r.attributes.push(kv("gen_ai.agent.id", agent));
        }
        r
    }

    fn write(&self, store: &Store) {
        let rows: Vec<MinedRecord> = (0..self.n).map(|i| self.record(i)).collect();
        let partition = PartitionKey::derive(&rows[0]).expect("derive");
        let mut w = Writer::open_in_with_promoted(
            store,
            partition,
            ourios_parquet::DEFAULT_ZSTD_LEVEL,
            promoted(),
        )
        .expect("open writer");
        w.append_records(&rows).expect("append");
        w.close().expect("close");
    }
}

/// A store, a fake graph, and an emitter wired to it.
struct Graph {
    fake: Fake,
    emitter: Arc<GraphEmitter>,
    store: Store,
    bucket: tempfile::TempDir,
}

impl Graph {
    async fn new() -> Self {
        let fake = Fake::default();
        let url = serve(fake.clone()).await;
        let bucket = tempfile::TempDir::new().expect("temp");
        Self {
            fake,
            emitter: emitter(&url),
            store: super::tests::store_at(bucket.path()),
            bucket,
        }
    }

    fn target(&self) -> SweepTarget {
        SweepTarget::new(self.store.clone(), CompactionPolicy::default(), promoted())
            .with_emitter(Arc::clone(&self.emitter))
    }

    async fn sweep(
        &self,
        sink: Box<dyn AuditSink>,
    ) -> (Result<SweepReport, IngestError>, Box<dyn AuditSink>) {
        let (result, _, sink) = sweep_once(self.target(), sink).await;
        (result, sink)
    }

    fn tuples(&self) -> Vec<TupleKey> {
        self.fake.tuples.lock().expect("lock").clone()
    }

    fn writes(&self) -> Vec<usize> {
        self.fake.writes.lock().expect("lock").clone()
    }

    fn has_object(&self, object: &str) -> bool {
        self.tuples().iter().any(|t| t.object == object)
    }

    fn live_rows(&self) -> Vec<MinedRecord> {
        let mut rows = Vec::new();
        for key in self.store.list_blocking(Some("data/")).expect("list") {
            if key.ends_with(".parquet") {
                let bytes = self.store.get_blocking(&key).expect("get");
                let reader = Reader::open_bytes(bytes.into()).expect("open");
                rows.extend(reader.read_all().expect("read"));
            }
        }
        rows
    }
}

/// Scenario RFC0047.10 — the sweep feeds the graph: after a sweep the
/// `parent`, `participant`, `actor` (and binding, and tool) tuples exist
/// with tenant-prefixed ids; a second sweep writes nothing new (the
/// partition is consolidated, nothing is rewritten); every `Write` is
/// ≤ 100 tuples. See `docs/rfcs/0047-rebac-resolver-and-graph-visibility.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0047_10_sweep_emits_tuples_idempotently() {
    let graph = Graph::new().await;
    // Two files → a sealed candidate; 130 distinct conversations so the
    // tuple set spans more than one chunk.
    Rows {
        agent: Some("bot"),
        ..Rows::of("c-1", "alice", 3)
    }
    .write(&graph.store);
    for i in 0..130 {
        Rows::of(&format!("c-{}", i + 10), "bob", 1).write(&graph.store);
    }
    let (result, sink) = graph.sweep(Box::new(SharedAuditSink::new())).await;
    let report = result.expect("sweep");
    assert_eq!(report.partitions_compacted, 1, "{report:?}");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let tuples = graph.tuples();
    let t = |u: &str, r: &str, o: &str| TupleKey::new(u, r, o);
    for tuple in [
        t("tenant:acme", "parent", "conversation:acme/c-1"),
        t("user:alice", "participant", "conversation:acme/c-1"),
        t("user:alice", "scoped_reader", "tenant:acme"),
        t("agent:bot", "actor", "conversation:acme/c-1"),
        t("agent:bot", "scoped_reader", "tenant:acme"),
        t("tenant:acme", "parent", "conversation:acme/c-42"),
        t("user:bob", "participant", "conversation:acme/c-42"),
        t("user:bob", "scoped_reader", "tenant:acme"),
        t("tenant:acme", "parent", "tool:acme/query_logs"),
    ] {
        assert!(tuples.contains(&tuple), "missing {tuple:?}");
    }
    assert_eq!(
        report.graph_tuples_emitted,
        tuples.len(),
        "every tuple sent once"
    );
    let writes = graph.writes();
    assert!(
        writes.len() >= 2 && writes.iter().all(|n| *n <= 100),
        "{writes:?}"
    );

    // Second sweep: nothing to consolidate, nothing rewritten, nothing sent.
    let (result, _) = graph.sweep(sink).await;
    let report = result.expect("sweep");
    assert_eq!(
        (report.partitions_compacted, report.graph_tuples_emitted),
        (0, 0)
    );
    assert_eq!(
        (graph.writes().len(), graph.tuples().len()),
        (writes.len(), tuples.len()),
        "nothing new"
    );
}

/// Makes a directory unlistable, and listable again when dropped, so a
/// failing assertion or panic still leaves the temp dir removable.
#[cfg(unix)]
struct Unlistable(std::path::PathBuf);

#[cfg(unix)]
impl Unlistable {
    fn new(dir: std::path::PathBuf) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).expect("unlistable");
        Self(dir)
    }
}

#[cfg(unix)]
impl Drop for Unlistable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// A sweep that commits a partition and then fails fatally (the erasure
/// markers cannot be listed) still writes that partition's tuples after
/// the pass: the partition is consolidated, so no later sweep derives
/// them again.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_sweep_still_writes_its_committed_partitions_tuples() {
    // Arrange
    let graph = Graph::new().await;
    Rows::of("c-1", "alice", 1).write(&graph.store);
    Rows::of("c-1", "alice", 1).write(&graph.store);
    let markers = graph.bucket.path().join("erasure");
    std::fs::create_dir(&markers).expect("erasure dir");
    let unlistable = Unlistable::new(markers);

    // Act
    let (result, _) = graph.sweep(Box::new(SharedAuditSink::new())).await;
    drop(unlistable);

    // Assert
    assert!(result.is_err(), "the sweep failed fatally: {result:?}");
    assert!(
        graph.has_object("conversation:acme/c-1"),
        "{:?}",
        graph.tuples()
    );
}

/// RFC0047.11's setup: a repeated request is a no-op (create-if-absent)
/// that never resets a marker's phase, whether the marker is the
/// sweep-written one or hand-written in the `tuples` phase.
fn request_c1_erasure(store: &Store) {
    request_erasure(store, "acme", "c-1").expect("request");
    request_erasure(store, "acme", "c-1").expect("repeat");
    assert_eq!(pending_erasures(store).expect("pending").len(), 1);
    let c9 = super::erasure_marker_key("acme", "c-9");
    store
        .put_blocking(&c9, b" { \"phase\" : \"tuples\" }\n".to_vec())
        .expect("hand-written marker");
    request_erasure(store, "acme", "c-9").expect("repeat");
    let phases: Vec<_> = pending_erasures(store)
        .expect("pending")
        .into_iter()
        .map(|r| (r.conversation_id, r.phase))
        .collect();
    assert!(
        phases.contains(&("c-9".to_string(), ErasurePhase::Tuples)),
        "{phases:?}"
    );
    store.delete_blocking(&c9).expect("cleanup");
}

/// RFC0047.11's outcome: c-1's three rows dropped from one partition, its
/// three tuples (parent + participant + actor) deleted, the erasure done.
fn assert_c1_erased(outcome: &ErasureOutcome) {
    assert_eq!(
        (
            outcome.request.conversation_id.as_str(),
            outcome.rows_dropped,
            outcome.partitions_rewritten,
            outcome.phase,
        ),
        ("c-1", 3, 1, ErasurePhase::Tuples)
    );
    assert_eq!(
        (outcome.tuples_deleted, outcome.finished),
        (Some(3), true),
        "parent + participant + actor"
    );
}

/// Only c-2's rows remain, and in the graph no tuple on c-1 remains while
/// c-2's and the tenant-scoped binding tuples stay.
fn assert_only_c2_left(graph: &Graph) {
    let rows = graph.live_rows();
    assert_eq!(rows.len(), 2, "c-1's three rows are gone");
    let c2 = Some(Value::StringValue("c-2".to_string()));
    assert!(rows.iter().all(|r| {
        r.attributes.iter().any(|kv| {
            kv.key == "gen_ai.conversation.id"
                && kv.value.as_ref().and_then(|v| v.value.clone()) == c2
        })
    }));
    let binding = TupleKey::new("user:alice", "scoped_reader", "tenant:acme");
    assert_eq!(
        (
            graph.has_object("conversation:acme/c-1"),
            graph.has_object("conversation:acme/c-2"),
            graph.tuples().contains(&binding),
        ),
        (false, true, true)
    );
}

/// The erasure's audit trail: the rewrite is audited as a compaction, and
/// the `conversation_erased` event, carrying the counts, is the sweep's
/// last.
fn assert_erasure_audited(events: &[AuditEvent]) {
    assert!(
        events
            .iter()
            .any(|e| matches!(e.payload, AuditPayload::Compaction { .. })),
        "the erasure rewrite is audited as a compaction: {events:?}"
    );
    let erased = events
        .iter()
        .position(|e| matches!(e.payload, AuditPayload::ConversationErased { .. }))
        .expect("conversation_erased event");
    assert_eq!(erased, events.len() - 1, "last event of the sweep");
    let AuditPayload::ConversationErased {
        conversation_id,
        partitions_rewritten,
        rows_dropped,
        tuples_deleted,
    } = &events[erased].payload
    else {
        unreachable!("matched above");
    };
    assert_eq!(
        (
            conversation_id.as_str(),
            *partitions_rewritten,
            *rows_dropped,
            *tuples_deleted,
            events[erased].tenant_id.as_str(),
        ),
        ("c-1", 1, 3, 3, "acme")
    );
}

/// Scenario RFC0047.11 — erasure removes tuples after rows: a requested
/// erasure rewrites the tenant's partitions with the conversation's rows
/// dropped, then deletes its tuples, then writes the `conversation_erased`
/// audit event after every compaction event, then removes the marker; the
/// object is unlisted (no tuple on it remains) and other conversations'
/// tuples are untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0047_11_erasure_removes_tuples_after_rows() {
    let graph = Graph::new().await;
    Rows {
        agent: Some("bot"),
        ..Rows::of("c-1", "alice", 3)
    }
    .write(&graph.store);
    Rows::of("c-2", "bob", 2).write(&graph.store);
    let audit = SharedAuditSink::new();
    // Sweep 1: consolidate + feed the graph.
    let (result, sink) = graph.sweep(Box::new(audit.clone())).await;
    result.expect("sweep");
    assert!(graph.has_object("conversation:acme/c-1"));
    let _ = audit.drain();

    // Request the erasure of c-1; sweep 2 performs it.
    request_c1_erasure(&graph.store);
    let (result, _) = graph.sweep(sink).await;
    let report = result.expect("sweep");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.erasures.len(), 1, "{report:?}");
    assert_c1_erased(&report.erasures[0]);
    assert_only_c2_left(&graph);
    // Marker gone; nothing pending.
    assert!(pending_erasures(&graph.store).expect("pending").is_empty());
    assert_erasure_audited(&audit.drain());
    assert_eq!(
        report.partitions_compacted, 1,
        "and counted in the sweep's IO accounting"
    );
    assert!(report.bytes_read > 0);
}

/// RFC0047.11 (raw ids): a conversation whose id can never be a graph
/// object (here: whitespace) still has its rows erased — matched on the
/// stored value — with zero tuples to delete and an honest audit event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0047_11_erasure_matches_raw_ids() {
    let graph = Graph::new().await;
    Rows::of("odd id", "alice", 2).write(&graph.store);
    Rows::of("c-2", "bob", 1).write(&graph.store);
    let (result, sink) = graph.sweep(Box::new(SharedAuditSink::new())).await;
    result.expect("sweep");
    assert!(
        !graph.tuples().iter().any(|t| t.object.contains("odd")),
        "no tuple was ever minted for a non-object-id conversation"
    );
    request_erasure(&graph.store, "acme", "odd id").expect("request");
    let (result, _) = graph.sweep(sink).await;
    let report = result.expect("sweep");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let outcome = &report.erasures[0];
    assert_eq!(outcome.rows_dropped, 2, "rows matched on the raw value");
    assert_eq!(
        (
            outcome.tuples_deleted,
            outcome.finished,
            graph.live_rows().len()
        ),
        (Some(0), true, 1)
    );
}

/// RFC0048.4 — the completion event: its **registry-backed name** is
/// the contract (a reword of the message must not silently drop it),
/// and the message carries the four values RFC 0048 §3.3 names.
/// Current-thread runtime on purpose: the event is emitted after an
/// `.await`, and a thread-local subscriber only sees it when the task
/// cannot migrate to another worker.
#[tokio::test]
async fn rfc0048_4_completion_event_carries_the_registry_name() {
    let graph = Graph::new().await;
    Rows::of("c-1", "alice", 2).write(&graph.store);
    let (result, sink) = graph.sweep(Box::new(SharedAuditSink::new())).await;
    result.expect("sweep");
    request_erasure(&graph.store, "acme", "c-1").expect("request");
    let events: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
    let subscriber = {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry().with(CaptureEvents(Arc::clone(&events)))
    };
    let guard = tracing::subscriber::set_default(subscriber);
    let (result, _) = graph.sweep(sink).await;
    drop(guard);
    let report = result.expect("sweep");
    assert!(report.erasures[0].finished, "{report:?}");
    let events = events.lock().expect("events").clone();
    let completion = events
        .iter()
        .find(|(name, _)| name == ourios_semconv::EVENT_OURIOS_COMPACTION_ERASURE_COMPLETED)
        .unwrap_or_else(|| panic!("no completion event among {events:?}"));
    assert_eq!(completion.0, "ourios.compaction.erasure.completed");
    // The fake's `Read` returns no tuples, so the delete count is 0 —
    // the count *reaching the message* is what this pins (RFC0047.11
    // covers a real graph's non-zero delete).
    assert!(
        completion.1.contains(
            "conversation erasure completed: tenant \"acme\" conversation \"c-1\", \
             2 rows dropped, 0 tuples deleted"
        ),
        "{completion:?}"
    );
}

/// A `Layer` capturing `(event name, rendered message)` pairs — the
/// `fmt` mirror renders the message but not the metadata name, and the
/// name is what the registry pins.
struct CaptureEvents(Arc<std::sync::Mutex<Vec<(String, String)>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureEvents {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        self.0
            .lock()
            .expect("events")
            .push((event.metadata().name().to_string(), message.0));
    }
}
