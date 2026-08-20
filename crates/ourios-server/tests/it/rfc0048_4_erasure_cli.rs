//! RFC 0048 §3.3 (RFC0048.4) — erasure has a front door: the `graph
//! erase` / `graph erasures` subcommands against the daemon's storage
//! config. The sweep half (rows → tuples → marker gone → audit event) is
//! RFC0047.11, unchanged, in the container suite; the completion log
//! event is asserted at the compactor. Here: the marker lifecycle through
//! the CLI, idempotency, the listing and the grammar refusals — all
//! before any daemon runs.

use std::io::Write as _;
use std::process::Output;
use std::time::Duration;

use tokio::time::timeout;

async fn run(tmp: &tempfile::TempDir, args: &[&str]) -> Output {
    let config_path = tmp.path().join("ourios.yaml");
    if !config_path.exists() {
        let mut file = std::fs::File::create(&config_path).expect("create config");
        write!(
            file,
            "storage:\n  local:\n    bucket_root: {}\n",
            tmp.path().display(),
        )
        .expect("write config");
    }
    timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_ourios-server"))
            .arg("--config")
            .arg(&config_path)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("verb exits before timeout")
    .expect("run ourios-server")
}

#[tokio::test]
async fn rfc0048_4_erase_and_erasures_verbs() {
    let tmp = tempfile::TempDir::new().expect("temp");

    // Nothing pending on a fresh store.
    let output = run(&tmp, &["graph", "erasures"]).await;
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no pending erasures"), "{stdout}");

    // Erase writes the marker; the listing shows it in the rows phase.
    let output = run(
        &tmp,
        &[
            "graph",
            "erase",
            "--tenant",
            "acme",
            "--conversation",
            "c-7",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("erasure requested"),
        "{output:?}"
    );
    let output = run(&tmp, &["graph", "erasures"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#"tenant "acme" conversation "c-7" phase rows"#),
        "{stdout}"
    );

    // Idempotent: a second erase succeeds and the listing still shows one.
    let output = run(
        &tmp,
        &[
            "graph",
            "erase",
            "--tenant",
            "acme",
            "--conversation",
            "c-7",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let output = run(&tmp, &["graph", "erasures", "--tenant", "acme"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches("pending erasure").count(), 1, "{stdout}");

    // A conversation id with `/` is fine (object-id grammar).
    let output = run(
        &tmp,
        &[
            "graph",
            "erase",
            "--tenant",
            "acme",
            "--conversation",
            "sess/42",
        ],
    )
    .await;
    assert!(output.status.success(), "{output:?}");

    // The tenant filter excludes other tenants.
    let output = run(&tmp, &["graph", "erasures", "--tenant", "globex"]).await;
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("no pending erasures"),
        "{output:?}"
    );

    // Grammar refusals, before touching the store: off-grammar tenant,
    // off-grammar conversation, off-grammar filter.
    for (args, needle) in [
        (
            vec!["graph", "erase", "--tenant", "a/b", "--conversation", "c-1"],
            "--tenant",
        ),
        (
            vec![
                "graph",
                "erase",
                "--tenant",
                "acme",
                "--conversation",
                "a b",
            ],
            "--conversation",
        ),
        (vec!["graph", "erasures", "--tenant", "a:b"], "--tenant"),
    ] {
        let output = run(&tmp, &args).await;
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(needle), "{needle:?} not in {stderr}");
    }
}
