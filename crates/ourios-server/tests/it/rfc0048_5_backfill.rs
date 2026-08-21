//! RFC 0048 §3.4 against a **real `OpenFGA` container** (CI-gated like the
//! other graph tests): `graph backfill` feeds the graph from history that
//! predates the graph configuration (RFC0048.5 — the `--from` boundary,
//! idempotency, and no Parquet rewrite included), and backfill / erasure
//! hold each other off through the store (RFC0048.8).

use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use ourios_core::audit::SharedAuditSink;
use ourios_core::auth::openfga::{
    OpenFgaClient, OpenFgaSpec, VisibilityObjectSpec, VisibilitySpec, build_openfga_config,
};
use ourios_ingester::compactor::{
    acquire_backfill_lock, backfill_locks, pending_erasures, release_backfill_lock, sweep_once,
};
use ourios_ingester::graph_emitter::GraphEmitter;
use ourios_parquet::{CompactionPolicy, Store};
use testcontainers_modules::testcontainers::core::ContainerPort;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{GenericImage, ImageExt};
use tokio::time::timeout;

use crate::rfc0029_oidc::claim_binding::spawn_with_auth_and_storage;
use crate::rfc0029_oidc::ingest_binding::{make_key, serve_issuer};
use crate::rfc0047_openfga::{OPENFGA_IMAGE, OPENFGA_TAG, mint, provision};
use crate::rfc0047_visibility::{conversations, promoted, query, row_at, write_records};

/// 2026-04-02T10:58:00 UTC and one hour later — two sealed partitions.
const HOUR1: u64 = 1_775_127_480_000_000_000;
const HOUR2_START: &str = "2026-04-02T11:00:00Z";
const HOUR2: u64 = 1_775_131_080_000_000_000;
const ALL: &str = "true | range(-365d, now) | limit 100";

/// The `auth:` block binding this test's issuer and graph — one raw
/// literal, so the YAML reads as YAML.
fn auth_yaml(issuer: &str, api_url: &str, store_id: &str, model_id: &str) -> String {
    format!(
        r"auth:
  oidc:
    issuer: {issuer}
    audience: ourios
  openfga:
    api_url: {api_url}
    store_id: {store_id}
    authorization_model_id: {model_id}
    session_ttl_secs: 1
    visibility:
      objects:
        - type: conversation
          column: attr.gen_ai.conversation.id
"
    )
}

/// Run one `graph` CLI verb against `config_path`; return the output.
async fn cli(config_path: &std::path::Path, args: &[&str]) -> std::process::Output {
    timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_ourios-server"))
            .arg("--config")
            .arg(config_path)
            .args(args)
            .env("RFC0048_TOKEN", "tok-ops")
            // The verb boots the same telemetry stack as the daemon
            // (RFC 0048 §3.3); tests want its stderr mirror, not an export
            // attempt — the universal env var is the off switch.
            .env("OTEL_SDK_DISABLED", "true")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("verb exits before timeout")
    .expect("run ourios-server")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one container, one store, both scenarios in sequence
#[ignore = "RFC0048.5/.8 — needs Docker (real OpenFGA container); run by the openfga-resolver CI job via --ignored"]
async fn rfc0048_5_8_backfill_and_fence_end_to_end() {
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

    // --- History stored before any graph existed -------------------------
    let tmp = tempfile::TempDir::new().expect("temp");
    write_records(
        tmp.path(),
        &[
            row_at(HOUR1, "c-1", "alice"),
            row_at(HOUR1 + 1_000, "c-1", "alice"),
            row_at(HOUR2, "c-2", "bob"),
        ],
    );
    let store = Store::local(tmp.path()).expect("store");
    let parquet_before = {
        let mut keys = store.list_blocking(Some("data/")).expect("list");
        keys.sort();
        keys
    };

    // The daemon/CLI config: OIDC + the graph. No self_principal_column —
    // RFC0048.5's "sees no rows" must come from the graph alone.
    let (encoding, jwk) = make_key("key-1");
    let issuer = serve_issuer(jwk).await;
    let config_path = tmp.path().join("ourios.yaml");
    let mut file = std::fs::File::create(&config_path).expect("create config");
    // A raw literal: the YAML's indentation is the file's indentation, so
    // a structural mistake is visible here rather than inside escapes.
    write!(
        file,
        r"storage:
  local:
    bucket_root: {bucket}
  promoted_attributes:
    log: [gen_ai.conversation.id, user.hash, model, {{key: cost_usd, type: f64}}]
{auth}",
        bucket = tmp.path().display(),
        auth = auth_yaml(&issuer, &api_url, &store_id, &model_id),
    )
    .expect("write config");

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

    // --- RFC0048.5: --from feeds only the later hour ----------------------
    let output = cli(
        &config_path,
        &[
            "graph",
            "backfill",
            "--tenant",
            "acme",
            "--from",
            HOUR2_START,
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 partitions"), "{stdout}");
    assert!(
        fga.read_by_object("conversation:acme/c-1")
            .await
            .expect("read")
            .is_empty(),
        "the earlier hour was not fed"
    );
    assert!(
        !fga.read_by_object("conversation:acme/c-2")
            .await
            .expect("read")
            .is_empty(),
        "the later hour was fed"
    );

    // --- RFC0048.5: the full backfill feeds everything, idempotently ------
    let output = cli(&config_path, &["graph", "backfill", "--tenant", "acme"]).await;
    assert!(output.status.success(), "{output:?}");
    // RFC 0048 §3.4 — the per-partition progress events are the run's
    // observability contract, and they reach the operator on stderr (the
    // `fmt` mirror is installed even with the SDK disabled).
    let progress = String::from_utf8_lossy(&output.stderr);
    assert!(
        progress.contains("backfill progress: tenant \"acme\" partition 2026-04-02T10"),
        "{progress}"
    );
    assert!(
        progress.contains("rows offered"),
        "the event names what it fed: {progress}"
    );
    let c1_count = fga
        .read_by_object("conversation:acme/c-1")
        .await
        .expect("read")
        .len();
    assert!(c1_count > 0, "history fed");
    let output = cli(&config_path, &["graph", "backfill", "--tenant", "acme"]).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fga.read_by_object("conversation:acme/c-1")
            .await
            .expect("read")
            .len(),
        c1_count,
        "a second run writes nothing new"
    );
    let parquet_after = {
        let mut keys = store.list_blocking(Some("data/")).expect("list");
        keys.sort();
        keys
    };
    assert_eq!(
        parquet_before, parquet_after,
        "no Parquet file was rewritten"
    );
    assert!(
        backfill_locks(&store).expect("locks").is_empty(),
        "the lock is gone after completion"
    );

    // --- The scoped principals see exactly their history ------------------
    let storage_yaml = "  promoted_attributes:\n    log: [gen_ai.conversation.id, user.hash, model, {key: cost_usd, type: f64}]\n";
    let auth = auth_yaml(&issuer, &api_url, &store_id, &model_id);
    let (mut child, _grpc, _http, querier) =
        spawn_with_auth_and_storage(&tmp, storage_yaml, &auth, &[]).await;
    let alice = mint(&encoding, &issuer, "alice", &[], false);
    let (status, body) = query(&http, querier, &alice, "acme", ALL).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        conversations(&body),
        ["c-1", "c-1"],
        "alice sees her history"
    );
    let bob = mint(&encoding, &issuer, "bob", &[], false);
    let (status, body) = query(&http, querier, &bob, "acme", ALL).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(conversations(&body), ["c-2"]);
    child.kill().await.expect("kill the server");

    // --- RFC0048.8: pending erasure refuses backfill, leaving no lock -----
    let output = cli(
        &config_path,
        &[
            "graph",
            "erase",
            "--tenant",
            "acme",
            "--conversation",
            "c-1",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let output = cli(&config_path, &["graph", "backfill", "--tenant", "acme"]).await;
    assert!(!output.status.success(), "refused while erasures pend");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("run again after the next sweep"),
        "{stderr}"
    );
    assert!(
        backfill_locks(&store).expect("locks").is_empty(),
        "a refused backfill leaves no lock behind"
    );
    let output = cli(&config_path, &["graph", "erasures"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("pending erasure"), "{stdout}");
    assert!(!stdout.contains("backfill lock"), "{stdout}");

    // --- RFC0048.8: a backfill lock defers the erasure, then it completes -
    assert!(acquire_backfill_lock(&store, "acme").expect("acquire"));
    let emitter = Arc::new(
        GraphEmitter::from_config(&config)
            .expect("emitter")
            .expect("conversation bound"),
    );
    let audit = SharedAuditSink::new();
    let (result, _, sink) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        Box::new(audit.clone()),
        Some(Arc::clone(&emitter)),
    )
    .await;
    let report = result.expect("sweep");
    assert_eq!(report.erasures_deferred.len(), 1, "{report:?}");
    assert!(report.erasures.is_empty(), "not advanced");
    assert_eq!(pending_erasures(&store).expect("pending").len(), 1);
    let output = cli(&config_path, &["graph", "erasures"]).await;
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(r#"backfill lock: tenant "acme""#),
        "{output:?}"
    );
    release_backfill_lock(&store, "acme").expect("release");
    let (result, _, _) = sweep_once(
        store.clone(),
        CompactionPolicy::default(),
        promoted(),
        sink,
        Some(emitter),
    )
    .await;
    let report = result.expect("sweep");
    assert!(report.erasures_deferred.is_empty());
    assert!(report.erasures[0].finished, "{report:?}");
    assert!(pending_erasures(&store).expect("pending").is_empty());
    assert!(
        fga.read_by_object("conversation:acme/c-1")
            .await
            .expect("read")
            .is_empty(),
        "the deferred erasure completed after the lock was removed"
    );
    drop(container);
}
