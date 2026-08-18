//! RFC 0047 §3.3 / §3.6 against a **real `OpenFGA` container** (testcontainers;
//! CI-gated like the other RFC 0047 container tests — `#[ignore]`d in the
//! default run): the compaction sweep feeds the graph from stored rows,
//! and a requested erasure removes the rows, then the tuples.
//!
//! Scenarios RFC0047.10 (emitter) and RFC0047.11 (erasure), plus the
//! end-to-end proof that emitted tuples bind a participant on the served
//! binary (no operator-written conversation tuples anywhere).
//! See `docs/rfcs/0047-rebac-resolver-and-graph-visibility.md` §5.

use std::sync::Arc;
use std::time::Duration;

use ourios_core::audit::{AuditPayload, SharedAuditSink};
use ourios_core::auth::openfga::{
    OpenFgaClient, OpenFgaSpec, TupleKey, VisibilityObjectSpec, VisibilitySpec,
    build_openfga_config,
};
use ourios_ingester::compactor::{pending_erasures, request_erasure, sweep_once};
use ourios_ingester::graph_emitter::GraphEmitter;
use ourios_parquet::{CompactionPolicy, Store};
use testcontainers_modules::testcontainers::core::ContainerPort;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{GenericImage, ImageExt};
use tokio::time::timeout;

use crate::rfc0029_oidc::claim_binding::spawn_with_auth_and_storage;
use crate::rfc0029_oidc::ingest_binding::{make_key, serve_issuer};
use crate::rfc0047_openfga::{OPENFGA_IMAGE, OPENFGA_TAG, mint, provision, tuple};
use crate::rfc0047_visibility::{conversations, promoted, query, row_at, write_records};

/// 2026-04-02T10:58:00 UTC — a long-sealed hour, so the partition is a
/// compaction candidate under the default policy.
const TS0: u64 = 1_775_127_480_000_000_000;
/// A window that reaches back to `TS0` from any 2026 wall clock.
const ALL: &str = "true | range(-365d, now) | limit 100";

fn row(i: u64, conversation: &str, user: &str) -> ourios_core::record::MinedRecord {
    row_at(TS0 + i * 1_000, conversation, user)
}

/// Scenarios RFC0047.10 / RFC0047.11 on a real graph.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one container, one store, both scenarios in sequence
#[ignore = "RFC0047.10–.11 — needs Docker (real OpenFGA container); run by the openfga-resolver CI job via --ignored"]
async fn rfc0047_10_11_emitter_and_erasure_end_to_end() {
    // --- OpenFGA -----------------------------------------------------------
    let container = GenericImage::new(OPENFGA_IMAGE, OPENFGA_TAG)
        .with_exposed_port(ContainerPort::Tcp(8080))
        .with_cmd(["run"])
        .start()
        .await
        .expect("openfga started");
    let port = container
        .get_host_port_ipv4(8080)
        .await
        .expect("mapped port");
    let api_url = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    timeout(Duration::from_secs(60), async {
        loop {
            if let Ok(response) = http.get(format!("{api_url}/healthz")).send().await
                && response.status().is_success()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("openfga healthy before timeout");
    let (store_id, model_id) = provision(&api_url).await;
    let config = build_openfga_config(&OpenFgaSpec {
        api_url: Some(api_url.clone()),
        store_id: Some(store_id.clone()),
        authorization_model_id: Some(model_id.clone()),
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
    let fga = OpenFgaClient::new(&config).expect("client");
    let emitter = Arc::new(
        GraphEmitter::from_config(&config)
            .expect("emitter")
            .expect("conversation bound"),
    );

    // --- Parquet: two files in one sealed hour → a compaction candidate ----
    let tmp = tempfile::TempDir::new().expect("temp");
    let bucket = tmp.path().to_path_buf();
    write_records(
        &bucket,
        &[
            row(1, "c-1", "alice"),
            row(2, "c-1", "alice"),
            row(3, "c-2", "bob"),
        ],
    );
    write_records(&bucket, &[row(4, "c-3", "carol"), row(5, "c-2", "bob")]);
    let store = Store::local(&bucket).expect("store");
    let audit = SharedAuditSink::new();

    // --- RFC0047.10: the sweep feeds the graph -----------------------------
    let (result, _, sink) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        Box::new(audit.clone()),
        Some(Arc::clone(&emitter)),
    )
    .await;
    let report = result.expect("sweep");
    assert_eq!(report.partitions_compacted, 1, "{report:?}");
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.graph_tuples_emitted > 0);
    for (user, relation, object) in [
        ("tenant:acme", "parent", "conversation:acme/c-1"),
        ("user:alice", "participant", "conversation:acme/c-1"),
        ("user:alice", "scoped_reader", "tenant:acme"),
        ("user:bob", "participant", "conversation:acme/c-2"),
        ("tenant:acme", "parent", "conversation:acme/c-3"),
        ("tenant:acme", "parent", "tool:acme/query_logs"),
    ] {
        let tuples = fga.read_by_object(object).await.expect("read");
        assert!(
            tuples.contains(&TupleKey::new(user, relation, object)),
            "missing {user} {relation} {object}: {tuples:?}"
        );
    }
    // Idempotent: a second sweep has nothing to rewrite and writes nothing new.
    let count = |object: &str| {
        let fga = fga.clone();
        let object = object.to_string();
        async move { fga.read_by_object(&object).await.expect("read").len() }
    };
    let c1_before = count("conversation:acme/c-1").await;
    let (result, _, sink) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        sink,
        Some(Arc::clone(&emitter)),
    )
    .await;
    let report = result.expect("sweep");
    assert_eq!(report.partitions_compacted, 0);
    assert_eq!(report.graph_tuples_emitted, 0);
    assert_eq!(count("conversation:acme/c-1").await, c1_before);

    // --- The emitted tuples bind a participant on the served binary ------
    // No operator wrote a single conversation tuple: alice's `participant`
    // and binding tuples came from her rows.
    let (encoding, jwk) = make_key("key-1");
    let issuer = serve_issuer(jwk).await;
    let storage_yaml = "  promoted_attributes:\n    log: [gen_ai.conversation.id, user.hash, model, {key: cost_usd, type: f64}]\n";
    let auth_yaml = format!(
        "auth:\n\
         \x20\x20oidc:\n\
         \x20\x20\x20\x20issuer: {issuer}\n\
         \x20\x20\x20\x20audience: ourios\n\
         \x20\x20openfga:\n\
         \x20\x20\x20\x20api_url: {api_url}\n\
         \x20\x20\x20\x20store_id: {store_id}\n\
         \x20\x20\x20\x20authorization_model_id: {model_id}\n\
         \x20\x20\x20\x20session_ttl_secs: 1\n\
         \x20\x20\x20\x20visibility:\n\
         \x20\x20\x20\x20\x20\x20objects:\n\
         \x20\x20\x20\x20\x20\x20\x20\x20- type: conversation\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20column: attr.gen_ai.conversation.id\n"
    );
    let (mut child, _grpc, _http, querier) =
        spawn_with_auth_and_storage(&tmp, storage_yaml, &auth_yaml, &[]).await;
    let alice = mint(&encoding, &issuer, "alice", &[], false);
    let (status, body) = query(&http, querier, &alice, "acme", ALL).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        conversations(&body),
        ["c-1", "c-1"],
        "alice sees exactly her conversation, through tuples the data produced"
    );
    let bob = mint(&encoding, &issuer, "bob", &[], false);
    let (status, body) = query(&http, querier, &bob, "acme", ALL).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(conversations(&body), ["c-2", "c-2"]);
    child.kill().await.expect("kill the server");

    // --- RFC0047.11: erasure removes tuples after rows ---------------------
    let _ = audit.drain();
    request_erasure(&store, "acme", "c-1").expect("request");
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
    let outcome = &report.erasures[0];
    assert_eq!(outcome.rows_dropped, 2);
    assert_eq!(outcome.tuples_deleted, Some(2), "parent + participant");
    assert!(outcome.finished);
    assert!(pending_erasures(&store).expect("pending").is_empty());
    assert!(
        fga.read_by_object("conversation:acme/c-1")
            .await
            .expect("read")
            .is_empty(),
        "no tuple for the object remains"
    );
    assert!(
        !fga.check(
            &tuple("user:alice", "can_read_content", "conversation:acme/c-1"),
            &[]
        )
        .await
        .expect("check"),
        "unreachable"
    );
    assert!(
        !fga.read_by_object("conversation:acme/c-2")
            .await
            .expect("read")
            .is_empty(),
        "other conversations untouched"
    );
    // Audit ordering: the erasure event is the last event of the sweep,
    // after the rewrite's compaction events.
    let events = audit.drain();
    let last = events.last().expect("events");
    assert!(
        matches!(&last.payload, AuditPayload::ConversationErased { conversation_id, rows_dropped: 2, tuples_deleted: 2, .. } if conversation_id == "c-1"),
        "{last:?}"
    );
    // The rows are gone from the store: the served binary no longer
    // returns c-1 to a tenant-wide reader either.
    fga.write(&[tuple("user:zed", "reader", "tenant:acme")], &[])
        .await
        .expect("grant");
    let (mut child, _grpc, _http, querier) =
        spawn_with_auth_and_storage(&tmp, storage_yaml, &auth_yaml, &[]).await;
    let zed = mint(&encoding, &issuer, "zed", &[], false);
    let (status, body) = query(&http, querier, &zed, "acme", ALL).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        conversations(&body),
        ["c-2", "c-2", "c-3"],
        "c-1's rows are gone"
    );
    child.kill().await.expect("kill the server");
    drop(container);
}
