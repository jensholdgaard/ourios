//! The compaction sweep against a fake `OpenFGA` store: the RFC 0047
//! graph feed, erasure, and the per-commit tuple flush.

use std::path::Path;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use ourios_core::audit::{AuditPayload, SharedAuditSink};
use ourios_core::auth::openfga::{OpenFgaSpec, build_openfga_config};
use ourios_core::otlp::any_value::Value;
use ourios_core::otlp::{AnyValue, KeyValue};
use ourios_core::record::MinedRecord;
use ourios_parquet::{
    CompactionPolicy, PartitionKey, PromotedAttributes, PromotedKey, Reader, Store, Writer,
};
use ourios_serving::openfga::TupleKey;
use serde_json::json;

use super::{ErasurePhase, pending_erasures, request_erasure, sweep_once};
use crate::graph_emitter::GraphEmitter;

/// A fake `OpenFGA` store: `/write` applies writes/deletes (asserting the
/// ≤ 100 chunk), `/read` answers by object. With `fail_next_write` set,
/// the next `/write` answers `503` instead.
#[derive(Clone, Default)]
struct Fake {
    tuples: Arc<Mutex<Vec<TupleKey>>>,
    writes: Arc<Mutex<Vec<usize>>>,
    fail_next_write: Arc<std::sync::atomic::AtomicBool>,
}

fn json(value: &serde_json::Value) -> ([(&'static str, &'static str); 1], String) {
    ([("content-type", "application/json")], value.to_string())
}

async fn write(
    State(fake): State<Fake>,
    body: axum::body::Bytes,
) -> ([(&'static str, &'static str); 1], String) {
    let request: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let mut tuples = fake.tuples.lock().expect("lock");
    if let Some(keys) = request["writes"]["tuple_keys"].as_array() {
        assert!(keys.len() <= 100, "RFC 0047 §3.3: ≤ 100 tuples per Write");
        assert_eq!(request["writes"]["on_duplicate"], "ignore");
        fake.writes.lock().expect("lock").push(keys.len());
        for key in keys {
            let key: TupleKey = serde_json::from_value(key.clone()).expect("tuple");
            if !tuples.contains(&key) {
                tuples.push(key);
            }
        }
    }
    if let Some(keys) = request["deletes"]["tuple_keys"].as_array() {
        assert!(keys.len() <= 100);
        assert_eq!(request["deletes"]["on_missing"], "ignore");
        for key in keys {
            let key: TupleKey = serde_json::from_value(key.clone()).expect("tuple");
            tuples.retain(|t| *t != key);
        }
    }
    json(&json!({}))
}

/// Answers the next `/write` with `503` once `fail_next_write` is set.
async fn fail_gate(
    State(fake): State<Fake>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if fake
        .fail_next_write
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "down").into_response();
    }
    next.run(request).await
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
        .route(
            "/stores/{store}/write",
            post(write).layer(axum::middleware::from_fn_with_state(
                fake.clone(),
                fail_gate,
            )),
        )
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

/// One file per call, in the sealed hour partition, `rows` records with
/// `conversation`/`user` attributes.
fn write_rows(store: &Store, conversation: &str, user: &str, agent: Option<&str>, n: u64) {
    write_rows_at(store, super::tests::TS0, conversation, user, agent, n);
}

/// [`write_rows`], its rows from `ts0` on.
fn write_rows_at(
    store: &Store,
    ts0: u64,
    conversation: &str,
    user: &str,
    agent: Option<&str>,
    n: u64,
) {
    let rows: Vec<MinedRecord> = (0..n)
        .map(|i| {
            let mut r = super::tests::rec("acme", 1, ts0 + i * 1_000);
            r.attributes = vec![
                kv("gen_ai.conversation.id", conversation),
                kv("user.hash", user),
            ];
            if let Some(agent) = agent {
                r.attributes.push(kv("gen_ai.agent.id", agent));
            }
            r
        })
        .collect();
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

fn live_rows(store: &Store, bucket: &Path) -> Vec<MinedRecord> {
    let mut rows = Vec::new();
    for key in store.list_blocking(Some("data/")).expect("list") {
        if !key.ends_with(".parquet") {
            continue;
        }
        let bytes = store.get_blocking(&key).expect("get");
        let reader = Reader::open_bytes(bytes.into()).expect("open");
        rows.extend(reader.read_all().expect("read"));
    }
    let _ = bucket;
    rows
}

/// Scenario RFC0047.10 — the sweep feeds the graph: after a sweep the
/// `parent`, `participant`, `actor` (and binding, and tool) tuples exist
/// with tenant-prefixed ids; a second sweep writes nothing new (the
/// partition is consolidated, nothing is rewritten); every `Write` is
/// ≤ 100 tuples. See `docs/rfcs/0047-rebac-resolver-and-graph-visibility.md` §5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0047_10_sweep_emits_tuples_idempotently() {
    let fake = Fake::default();
    let url = serve(fake.clone()).await;
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = super::tests::store_at(bucket.path());
    // Two files → a sealed candidate; 130 distinct conversations so the
    // tuple set spans more than one chunk.
    write_rows(&store, "c-1", "alice", Some("bot"), 3);
    for i in 0..130 {
        write_rows(&store, &format!("c-{}", i + 10), "bob", None, 1);
    }
    let emitter = emitter(&url);
    let (result, _, sink) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        Box::new(SharedAuditSink::new()),
        Some(Arc::clone(&emitter)),
    )
    .await;
    let report = result.expect("sweep");
    assert_eq!(report.partitions_compacted, 1, "{report:?}");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let tuples = fake.tuples.lock().expect("lock").clone();
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
    let writes = fake.writes.lock().expect("lock").clone();
    assert!(
        writes.len() >= 2 && writes.iter().all(|n| *n <= 100),
        "{writes:?}"
    );

    // Second sweep: nothing to consolidate, nothing rewritten, nothing sent.
    let before = fake.writes.lock().expect("lock").len();
    let (result, _, _) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        sink,
        Some(emitter),
    )
    .await;
    let report = result.expect("sweep");
    assert_eq!(report.partitions_compacted, 0);
    assert_eq!(report.graph_tuples_emitted, 0);
    assert_eq!(
        fake.writes.lock().expect("lock").len(),
        before,
        "nothing new"
    );
    assert_eq!(fake.tuples.lock().expect("lock").len(), tuples.len());
}

/// A committed partition's tuples are written right after its commit,
/// not at sweep end: a sweep dying in its second partition has already
/// fed the graph the first partition's conversation, and not yet the
/// second's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_committed_partitions_tuples_survive_a_sweep_dying_after_it() {
    // Arrange
    let fake = Fake::default();
    let url = serve(fake.clone()).await;
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = super::tests::store_at(bucket.path());
    for (hour, conversation) in [(0, "c-1"), (1, "c-2")] {
        let ts0 = super::tests::TS0 + hour * super::tests::HOUR;
        write_rows_at(&store, ts0, conversation, "alice", None, 1);
        write_rows_at(&store, ts0 + 1_000_000, conversation, "alice", None, 1);
    }
    let sink = super::tests::CrashingSink::new(&SharedAuditSink::new(), 2);

    // Act
    let sweep = tokio::spawn(sweep_once(
        store,
        CompactionPolicy::default(),
        promoted(),
        Box::new(sink),
        Some(emitter(&url)),
    ))
    .await;

    // Assert
    assert!(matches!(&sweep, Err(e) if e.is_panic()), "the sweep died");
    let objects: Vec<String> = fake
        .tuples
        .lock()
        .expect("lock")
        .iter()
        .map(|t| t.object.clone())
        .collect();
    assert!(
        objects.iter().any(|o| o == "conversation:acme/c-1"),
        "{objects:?}"
    );
    assert!(
        !objects.iter().any(|o| o == "conversation:acme/c-2"),
        "{objects:?}"
    );
}

/// A sweep that commits a partition, fails to write its tuples, then
/// fails fatally (the erasure markers cannot be listed) still writes
/// those tuples: the partition is consolidated, so no later sweep
/// derives them again.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_sweep_still_writes_its_committed_partitions_tuples() {
    use std::os::unix::fs::PermissionsExt;

    /// Makes a directory unlistable, and listable again when dropped, so a
    /// failing assertion or panic still leaves the temp dir removable.
    struct Unlistable(std::path::PathBuf);
    impl Unlistable {
        fn new(dir: std::path::PathBuf) -> Self {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000))
                .expect("unlistable");
            Self(dir)
        }
    }
    impl Drop for Unlistable {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    // Arrange
    let fake = Fake::default();
    let url = serve(fake.clone()).await;
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = super::tests::store_at(bucket.path());
    write_rows(&store, "c-1", "alice", None, 1);
    write_rows(&store, "c-1", "alice", None, 1);
    let markers = bucket.path().join("erasure");
    std::fs::create_dir(&markers).expect("erasure dir");
    let unlistable = Unlistable::new(markers);
    fake.fail_next_write
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // Act
    let (result, _, _) = sweep_once(
        store,
        CompactionPolicy::default(),
        promoted(),
        Box::new(SharedAuditSink::new()),
        Some(emitter(&url)),
    )
    .await;
    drop(unlistable);

    // Assert
    assert!(result.is_err(), "the sweep failed fatally: {result:?}");
    let tuples = fake.tuples.lock().expect("lock").clone();
    assert!(
        tuples.iter().any(|t| t.object == "conversation:acme/c-1"),
        "{tuples:?}"
    );
}

/// Scenario RFC0047.11 — erasure removes tuples after rows: a requested
/// erasure rewrites the tenant's partitions with the conversation's rows
/// dropped, then deletes its tuples, then writes the `conversation_erased`
/// audit event after every compaction event, then removes the marker; the
/// object is unlisted (no tuple on it remains) and other conversations'
/// tuples are untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one store, one graph: rows → tuples → audit → marker in sequence
async fn rfc0047_11_erasure_removes_tuples_after_rows() {
    let fake = Fake::default();
    let url = serve(fake.clone()).await;
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = super::tests::store_at(bucket.path());
    write_rows(&store, "c-1", "alice", Some("bot"), 3);
    write_rows(&store, "c-2", "bob", None, 2);
    let emitter = emitter(&url);
    let audit = SharedAuditSink::new();
    // Sweep 1: consolidate + feed the graph.
    let (result, _, sink) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        Box::new(audit.clone()),
        Some(Arc::clone(&emitter)),
    )
    .await;
    result.expect("sweep");
    assert!(
        fake.tuples
            .lock()
            .expect("lock")
            .iter()
            .any(|t| t.object == "conversation:acme/c-1")
    );
    let _ = audit.drain();

    // Request the erasure of c-1; sweep 2 performs it. A repeated request
    // is a no-op (create-if-absent) — it never resets a marker's phase.
    request_erasure(&store, "acme", "c-1").expect("request");
    request_erasure(&store, "acme", "c-1").expect("repeat");
    assert_eq!(pending_erasures(&store).expect("pending").len(), 1);
    store
        .put_blocking(
            &super::erasure_marker_key("acme", "c-9"),
            b" { \"phase\" : \"tuples\" }\n".to_vec(),
        )
        .expect("hand-written marker");
    request_erasure(&store, "acme", "c-9").expect("repeat");
    let phases: Vec<_> = pending_erasures(&store)
        .expect("pending")
        .into_iter()
        .map(|r| (r.conversation_id, r.phase))
        .collect();
    assert!(
        phases.contains(&("c-9".to_string(), ErasurePhase::Tuples)),
        "{phases:?}"
    );
    store
        .delete_blocking(&super::erasure_marker_key("acme", "c-9"))
        .expect("cleanup");
    let (result, _, _) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        sink,
        Some(Arc::clone(&emitter)),
    )
    .await;
    let report = result.expect("sweep");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.erasures.len(), 1, "{report:?}");
    let outcome = &report.erasures[0];
    assert_eq!(outcome.request.conversation_id, "c-1");
    assert_eq!(outcome.rows_dropped, 3);
    assert_eq!(outcome.partitions_rewritten, 1);
    assert_eq!(outcome.phase, ErasurePhase::Tuples);
    assert_eq!(
        outcome.tuples_deleted,
        Some(3),
        "parent + participant + actor"
    );
    assert!(outcome.finished);

    // Rows: only c-2's remain.
    let rows = live_rows(&store, bucket.path());
    assert_eq!(rows.len(), 2, "c-1's three rows are gone");
    assert!(rows.iter().all(|r| {
        r.attributes.iter().any(|kv| {
            kv.key == "gen_ai.conversation.id"
                && kv.value.as_ref().and_then(|v| v.value.as_ref())
                    == Some(&Value::StringValue("c-2".to_string()))
        })
    }));
    // Tuples: no tuple on the object remains; c-2's untouched; the
    // binding tuples (tenant-scoped, not object-scoped) stay.
    let tuples = fake.tuples.lock().expect("lock").clone();
    assert!(!tuples.iter().any(|t| t.object == "conversation:acme/c-1"));
    assert!(tuples.iter().any(|t| t.object == "conversation:acme/c-2"));
    assert!(tuples.contains(&TupleKey::new("user:alice", "scoped_reader", "tenant:acme")));
    // Marker gone; nothing pending.
    assert!(pending_erasures(&store).expect("pending").is_empty());
    // Audit order: the erasure event comes after every compaction
    // event of the sweep (the rewrite is itself audited and counted as
    // a compaction), carrying the counts.
    let events = audit.drain();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.payload, AuditPayload::Compaction { .. })),
        "the erasure rewrite is audited as a compaction: {events:?}"
    );
    assert_eq!(
        report.partitions_compacted, 1,
        "and counted in the sweep's IO accounting"
    );
    assert!(report.bytes_read > 0);
    let erased = events
        .iter()
        .position(|e| matches!(e.payload, AuditPayload::ConversationErased { .. }))
        .expect("conversation_erased event");
    assert_eq!(erased, events.len() - 1, "last event of the sweep");
    match &events[erased].payload {
        AuditPayload::ConversationErased {
            conversation_id,
            partitions_rewritten,
            rows_dropped,
            tuples_deleted,
        } => {
            assert_eq!(conversation_id, "c-1");
            assert_eq!(*partitions_rewritten, 1);
            assert_eq!(*rows_dropped, 3);
            assert_eq!(*tuples_deleted, 3);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(events[erased].tenant_id.as_str(), "acme");
}

/// RFC0047.11 (raw ids): a conversation whose id can never be a graph
/// object (here: whitespace) still has its rows erased — matched on the
/// stored value — with zero tuples to delete and an honest audit event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0047_11_erasure_matches_raw_ids() {
    let fake = Fake::default();
    let url = serve(fake.clone()).await;
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = super::tests::store_at(bucket.path());
    write_rows(&store, "odd id", "alice", None, 2);
    write_rows(&store, "c-2", "bob", None, 1);
    let emitter = emitter(&url);
    let audit = SharedAuditSink::new();
    let (result, _, sink) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        Box::new(audit.clone()),
        Some(Arc::clone(&emitter)),
    )
    .await;
    result.expect("sweep");
    assert!(
        !fake
            .tuples
            .lock()
            .expect("lock")
            .iter()
            .any(|t| t.object.contains("odd")),
        "no tuple was ever minted for a non-object-id conversation"
    );
    request_erasure(&store, "acme", "odd id").expect("request");
    let (result, _, _) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        sink,
        Some(emitter),
    )
    .await;
    let report = result.expect("sweep");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let outcome = &report.erasures[0];
    assert_eq!(outcome.rows_dropped, 2, "rows matched on the raw value");
    assert_eq!(outcome.tuples_deleted, Some(0));
    assert!(outcome.finished);
    assert_eq!(live_rows(&store, bucket.path()).len(), 1);
}

/// RFC0048.4 — the completion event: its **registry-backed name** is
/// the contract (a reword of the message must not silently drop it),
/// and the message carries the four values RFC 0048 §3.3 names.
/// Current-thread runtime on purpose: the event is emitted after an
/// `.await`, and a thread-local subscriber only sees it when the task
/// cannot migrate to another worker.
#[tokio::test]
async fn rfc0048_4_completion_event_carries_the_registry_name() {
    let fake = Fake::default();
    let url = serve(fake.clone()).await;
    let bucket = tempfile::TempDir::new().expect("temp");
    let store = super::tests::store_at(bucket.path());
    write_rows(&store, "c-1", "alice", None, 2);
    let emitter = emitter(&url);
    let audit = SharedAuditSink::new();
    let (result, _, sink) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        Box::new(audit),
        Some(Arc::clone(&emitter)),
    )
    .await;
    result.expect("sweep");
    request_erasure(&store, "acme", "c-1").expect("request");
    let events: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::default();
    let subscriber = {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry().with(CaptureEvents(Arc::clone(&events)))
    };
    let guard = tracing::subscriber::set_default(subscriber);
    let (result, _, _) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        sink,
        Some(emitter),
    )
    .await;
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
