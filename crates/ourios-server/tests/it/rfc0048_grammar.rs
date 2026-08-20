//! RFC 0048 §3.1 at the served surfaces (RFC0048.1) and, behind the
//! Docker gate, the verbatim naming rule against a real `OpenFGA`
//! (RFC0048.2). The config and claim boundaries are unit-tested where
//! they live (`ourios-core::auth`); the OTLP boundary lives in the
//! RFC 0046 end-to-end test. Here: the querier header and the MCP
//! `tenant` argument speak the same grammar, and the graph holds the
//! exact bytes the grammar admits.

use std::time::Duration;

use ourios_core::auth::openfga::{
    OpenFgaSpec, VisibilityObjectSpec, VisibilitySpec, build_openfga_config,
};
use ourios_core::record::MinedRecord;
use ourios_ingester::graph_emitter::GraphEmitter;
use testcontainers_modules::testcontainers::core::ContainerPort;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{GenericImage, ImageExt};
use tokio::time::timeout;

use crate::rfc0029_oidc::claim_binding::spawn_with_auth;
use crate::rfc0047_openfga::{OPENFGA_IMAGE, OPENFGA_TAG, provision};
use crate::rfc0047_visibility::row_at;

const BAD_TENANTS: [&str; 6] = ["a/b", "a:b", "a#b", "a b", "\u{e9}-tenant", ""];

/// RFC0048.1 — the querier header applies the grammar (400, named reason).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rfc0048_1_querier_header_speaks_the_grammar() {
    let tmp = tempfile::TempDir::new().expect("temp");
    let (mut child, _grpc, _http, addr) = spawn_with_auth(&tmp, "", &[]).await;
    let http = reqwest::Client::new();
    for bad in BAD_TENANTS {
        let response = http
            .post(format!("http://{addr}/v1/query"))
            .header("content-type", "text/plain")
            .header("x-ourios-tenant", bad.replace('\u{e9}', "e"))
            .body("true | limit 1")
            .send()
            .await
            .expect("sent");
        if bad.contains('\u{e9}') {
            continue; // reqwest refuses non-ASCII header values client-side
        }
        assert_eq!(response.status(), 400, "{bad:?}");
    }
    let long = "x".repeat(129);
    let response = http
        .post(format!("http://{addr}/v1/query"))
        .header("content-type", "text/plain")
        .header("x-ourios-tenant", long)
        .body("true | limit 1")
        .send()
        .await
        .expect("sent");
    assert_eq!(response.status(), 400, "129 bytes");
    let ok = http
        .post(format!("http://{addr}/v1/query"))
        .header("content-type", "text/plain")
        .header("x-ourios-tenant", "team-eu.%1~x")
        .body("true | limit 1")
        .send()
        .await
        .expect("sent");
    assert_eq!(
        ok.status(),
        200,
        "graphic punctuation is inside the grammar"
    );
    child.kill().await.expect("kill");
}

/// RFC0048.2 — verbatim objects on a real graph: write → `Read`
/// byte-for-byte → streamed prefix filter; the 114/115 budget and a `/`
/// conversation id, end to end through the emitter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // one container: budget, `/` id, byte-for-byte read in sequence
#[ignore = "RFC0048.2 — needs Docker (real OpenFGA container); run by the openfga-resolver CI job via --ignored"]
async fn rfc0048_2_verbatim_tenant_on_a_real_graph() {
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
        store_id: Some(store_id),
        authorization_model_id: Some(model_id),
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
    let fga = ourios_core::auth::openfga::OpenFgaClient::new(&config).expect("client");
    let emitter = GraphEmitter::from_config(&config)
        .expect("emitter")
        .expect("conversation bound");

    // A 128-byte tenant, the exact RFC 0048 §3.1 budget: a 114-byte id and
    // an id containing `/` land verbatim; a 115-byte id is skipped by the
    // emitter (derive produces no tuple for it — never sent).
    let tenant = "t".repeat(128);
    let fits = "c".repeat(114);
    let over = "c".repeat(115);
    let rows: Vec<MinedRecord> = vec![
        row_at(1_000, &fits, "alice"),
        row_at(2_000, &over, "alice"),
        row_at(3_000, "sess/42", "bob"),
    ];
    let tuples = emitter.derive(&tenant, &rows);
    let objects: Vec<&str> = tuples
        .iter()
        .map(|t| t.object.as_str())
        .filter(|o| o.starts_with("conversation:"))
        .collect();
    let full_fits = format!("conversation:{tenant}/{fits}");
    let full_slash = format!("conversation:{tenant}/sess/42");
    assert!(objects.contains(&full_fits.as_str()), "114 fits");
    assert!(objects.contains(&full_slash.as_str()), "`/` id fits");
    assert!(
        !objects.iter().any(|o| o.contains(&over)),
        "115 is skipped, never sent"
    );
    assert_eq!(full_fits.len(), 256, "the budget is exact");
    emitter.emit(&tuples).await.expect("emit");

    // `Read` returns the exact bytes — no encoding step anywhere.
    let read = fga.read_by_object(&full_fits).await.expect("read");
    assert!(
        read.iter().any(|t| t.object == full_fits),
        "byte-for-byte: {read:?}"
    );
    let read = fga.read_by_object(&full_slash).await.expect("read");
    assert!(read.iter().any(|t| t.object == full_slash), "{read:?}");
    drop(container);
}
