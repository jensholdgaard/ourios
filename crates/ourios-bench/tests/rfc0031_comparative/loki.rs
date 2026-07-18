//! Loki container plumbing: image pins + the shared dispatch flag
//! list, OTLP push, `query_range`/matrix HTTP, per-pair measurement
//! polls, and the deadline-miss diagnostics.

use crate::*;

/// `grafana/loki`, digest-pinned like the Dex image (the tag names the
/// release a competent operator would run; the digest makes CI
/// reproducible).
pub(crate) const LOKI_IMAGE: &str = "grafana/loki";

pub(crate) const LOKI_TAG: &str =
    "3.5.3@sha256:3165cecce301ce5b9b6e3530284b080934a05cd5cafac3d3d82edcb887b45ecd";

/// The dispatch run's Loki flags — every documented deviation from the
/// stock single-binary config, each one a run-history lesson. ONE
/// constant shared by the dispatch (`rfc0031_indicative_comparative_run`)
/// and the per-PR backdated interop test
/// (`rfc0031_backdated_wide_range_interop`), so the config the cheap
/// CI test pins is BY CONSTRUCTION the config the expensive run uses —
/// drift between them is unrepresentable (issue #538 item 2).
pub(crate) const LOKI_DISPATCH_FLAGS: &[&str] = &[
    "-validation.reject-old-samples=false",
    // Run #11/#13 post-mortems (the #488 diagnostics): queries over
    // the replayed corpus's WEEKS-OLD time range skip the ingesters
    // entirely (`query_ingesters_within`, default 3h — the failing
    // response showed `ingester.totalReached: 0`), so rows still
    // sitting in unflushed low-volume chunks are INVISIBLE — the
    // L3 trace pair flickered with flush timing while high-volume
    // streams (kafka) always flushed fast enough to be seen. `0`
    // disables the cutoff so ingesters are always consulted — the
    // query-side twin of `reject-old-samples=false` for frozen
    // corpora, and in Loki's favour (without it Loki's answer to
    // an old-range query is silently incomplete).
    "-querier.query-ingesters-within=0",
    "-distributor.ingestion-rate-limit-mb=512",
    "-distributor.ingestion-burst-size-mb=1024",
    "-ingester.per-stream-rate-limit=512MB",
    "-ingester.per-stream-rate-limit-burst=1GB",
    // Runs #2–#4 all failed on the SAME ~5.27 MB internal message
    // regardless of our batch size (3 MiB → 1.5 MB outer), proving
    // a single `kafka`-service LogsData line's content alone
    // inflates past the stock 4 MiB internal gRPC cap. Raising the
    // cap (standard operator tuning, in Loki's favour — it lets
    // Loki accept the data at all) preserves the identical-ingest
    // precondition the equivalence check requires; skipping the
    // line would silently unequalize the two corpora. Flag names
    // per dskit's server registry (grpc_server_max_*_msg_size).
    "-server.grpc-max-recv-msg-size-bytes=16777216",
    "-server.grpc-max-send-msg-size-bytes=16777216",
    // `loki_measure_frequency_pair` polls the SAME query+step
    // repeatedly waiting for completeness; disable query_range
    // results caching so every poll re-queries fresh data rather
    // than risking a stale cached answer (flag lives under the
    // `querier.` prefix on `queryrangebase.Config.CacheResults`,
    // verified against the pinned v3.5.3 source).
    "-querier.cache-results=false",
    // L4's near-miss completeness plateau (runs #4/#6/#7 all
    // converged around 93-96% of expected rows, independent of
    // poll deadline — ruling out a slow-ingest race) is Loki's
    // default `-validation.max-entries-limit` (5000): the
    // `count_over_time(... | regexp ...)` aggregation still has
    // to scan every raw kafka log line in-window before the
    // line filter narrows it down, and kafka's overall volume in
    // some query-frontend splits exceeds 5000 lines, silently
    // truncating the scan before it reaches every matching line.
    // Confirmed by pulling the frozen otel-demo-v8 corpus and
    // checking every line matching the L4 needle against the
    // capture regex directly: all match — the shortfall isn't a
    // regex/content mismatch, it's lines never being scanned.
    // Raised well past the corpus's noisiest single template's
    // volume (~971K rows) — in Loki's favour, cost is memory,
    // not correctness.
    "-validation.max-entries-limit=2000000",
];

/// Start a Loki container on the stock image config plus `extra_args`
/// (explicit, documented CLI-flag deviations), wait for `/ready`, and
/// hand back the container (kept alive by the caller), the base URL,
/// and a timeout-bearing HTTP client.
///
/// The stock image config (schema v13 / TSDB) serves the native OTLP
/// endpoint and maps `service.name` → the `service_name` stream label;
/// auth is disabled. Exactly what a competent single-binary operator
/// gets out of the box.
pub(crate) async fn start_loki(
    extra_args: &[&str],
) -> (
    testcontainers_modules::testcontainers::ContainerAsync<
        testcontainers_modules::testcontainers::GenericImage,
    >,
    String,
    reqwest::Client,
) {
    use testcontainers_modules::testcontainers::core::ContainerPort;
    use testcontainers_modules::testcontainers::runners::AsyncRunner;
    use testcontainers_modules::testcontainers::{GenericImage, ImageExt};

    let mut cmd = vec!["-config.file=/etc/loki/local-config.yaml"];
    cmd.extend_from_slice(extra_args);
    let container = GenericImage::new(LOKI_IMAGE, LOKI_TAG)
        .with_exposed_port(ContainerPort::Tcp(3100))
        .with_cmd(cmd)
        .start()
        .await
        .expect("loki container starts");
    let port = container
        .get_host_port_ipv4(3100)
        .await
        .expect("loki host port");
    let base = format!("http://127.0.0.1:{port}");
    // A per-request timeout so a wedged container/network stack fails a
    // request (and the surrounding deadline loop moves on) rather than
    // hanging the CI job to its global timeout.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client");

    // Readiness: /ready flips 200 once the ingester is up. Surface the
    // container's own output on timeout so a config rejection doesn't
    // read as a bare timeout (Loki writes startup errors to both streams).
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(r) = http.get(format!("{base}/ready")).send().await
            && r.status().is_success()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let stdout = container
                .stdout_to_vec()
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let stderr = container
                .stderr_to_vec()
                .await
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            panic!(
                "loki /ready never turned 200.\n--- loki stdout ---\n{stdout}\n\
                 --- loki stderr ---\n{stderr}"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    (container, base, http)
}

/// POST one OTLP protobuf payload to Loki, retrying transient rejections
/// (429 rate-limit / 5xx) with a short backoff — a sustained-ingest push
/// must not fail the run on a burst limit.
pub(crate) async fn push_otlp(http: &reqwest::Client, base: &str, payload: Vec<u8>) {
    // Fail FAST on an oversized payload: it would 503 permanently (Loki's
    // stock 4 MiB internal gRPC cap), and burning the retry deadline on it
    // masks the real cause. This converts the batcher's byte ESTIMATE into
    // a checked guarantee at the actual encoded size.
    assert!(
        payload.len() < 4 * 1024 * 1024,
        "OTLP payload is {} bytes — at/over Loki's stock 4 MiB gRPC cap; \
         the byte-capped batcher under-estimated",
        payload.len(),
    );
    // `Bytes` so the per-attempt body handoff is a refcount bump, not a
    // multi-MB copy on every push of a long replay.
    let payload = bytes::Bytes::from(payload);
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let sent = http
            .post(format!("{base}/otlp/v1/logs"))
            .header("content-type", "application/x-protobuf")
            .body(payload.clone())
            .send()
            .await;
        // Transport errors (connection reset, timeout) are as transient as
        // a 429 during a sustained replay — retry them within the same
        // deadline rather than aborting the run on a blip.
        let (status, body) = match sent {
            Ok(resp) if resp.status().is_success() => {
                // A 2xx can still carry an OTLP partialSuccess with
                // silently-rejected records — which would unequalize the
                // two corpora and surface later as a baffling equivalence
                // or row-count failure. Fail HERE, loudly.
                let body = resp.bytes().await.expect("otlp push response body");
                if !body.is_empty() {
                    use prost::Message as _;
                    let decoded =
                        opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceResponse::decode(
                            body.as_ref(),
                        )
                        .expect("otlp push response decodes");
                    if let Some(partial) = decoded.partial_success {
                        assert!(
                            partial.rejected_log_records == 0,
                            "loki silently rejected {} records: {}",
                            partial.rejected_log_records,
                            partial.error_message,
                        );
                    }
                }
                return;
            }
            Ok(resp) => {
                let status = resp.status();
                let retryable = status.as_u16() == 429 || status.is_server_error();
                let body = resp.text().await.unwrap_or_default();
                assert!(retryable, "loki otlp push rejected: {status} — {body}");
                (status.to_string(), body)
            }
            Err(e) => ("transport error".to_string(), e.to_string()),
        };
        assert!(
            std::time::Instant::now() < deadline,
            "loki otlp push kept failing past the deadline: {status} — {body}",
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// One Loki `query_range` call, parsed to [`LineKey`]s.
pub(crate) async fn loki_query_range(
    http: &reqwest::Client,
    base: &str,
    logql: &str,
    start: u64,
    end: u64,
) -> Vec<LineKey> {
    let resp = http
        .get(format!("{base}/loki/api/v1/query_range"))
        .query(&[
            ("query", logql),
            ("start", &start.to_string()),
            ("end", &end.to_string()),
            // Loki's stock max_entries_limit_per_query is 5000; the
            // indicative pair picker caps expected rows at 4000, so a
            // complete result always fits one page (no pagination, and
            // the equivalence check can't silently truncate).
            ("limit", "5000"),
            ("direction", "forward"),
        ])
        .send()
        .await
        .expect("query_range");
    // Check the HTTP status before parsing: a non-2xx body may not be the
    // streams JSON at all, and "parse failed" would mask the real error.
    let status = resp.status();
    let body = resp.text().await.expect("query_range body");
    assert!(
        status.is_success(),
        "loki query_range returned {status}: {body}",
    );
    parse_loki_streams(&body).expect("parse loki streams")
}

/// One `query_range` call returning the lines plus BOTH of Loki's byte
/// figures from the same response body: engine-level decompressed
/// `totalBytesProcessed`, and the storage-side compressed/head-chunk
/// figures — so the report can carry the conservative ratio.
pub(crate) async fn loki_query_with_stats(
    http: &reqwest::Client,
    base: &str,
    logql: &str,
    start: u64,
    end: u64,
) -> (Vec<LineKey>, u64, LokiFetchedBytes) {
    let resp = http
        .get(format!("{base}/loki/api/v1/query_range"))
        .query(&[
            ("query", logql),
            ("start", &start.to_string()),
            ("end", &end.to_string()),
            ("limit", "5000"),
            ("direction", "forward"),
        ])
        .send()
        .await
        .expect("query_range");
    let status = resp.status();
    let body = resp.text().await.expect("query_range body");
    assert!(status.is_success(), "loki query_range {status}: {body}");
    (
        parse_loki_streams(&body).expect("parse loki streams"),
        parse_loki_bytes_processed(&body).expect("parse loki bytes"),
        parse_loki_fetched_bytes(&body).expect("parse loki fetched bytes"),
    )
}

/// One Loki **metric** `query_range` call — the L4 matrix counterpart of
/// [`loki_query_with_stats`]: `step` is pinned to the bucket width so
/// every evaluation instant is `t = bucket_start + width`
/// ([`parse_loki_matrix`]'s bucket-alignment convention), and the same
/// bytes-processed / fetched-bytes figures are read from the identical
/// stats block the line-returning pairs use.
///
/// **Precondition:** `start_ns` must already be whole-second AND
/// `bucket_width_ns`-aligned, or the response's evaluation instants land
/// off the second grid and [`parse_loki_matrix`] rejects them as
/// fractional-second samples. This function does not align `start_ns`
/// itself — [`l4_pair_spec`] (this function's only caller, via
/// [`loki_measure_frequency_pair`]) guarantees it by construction:
/// `start = (min_effective_time_unix_nano / bucket_width_ns) *
/// bucket_width_ns`, and `bucket_width_ns` is itself always a whole
/// multiple of `1_000_000_000` ([`bucket_width_seconds`] never produces
/// a sub-second width), so any multiple of it is automatically
/// whole-second-aligned too.
///
/// Returns `Err` on any transport/HTTP/parse failure rather than
/// panicking: this is called from [`loki_measure_frequency_pair`]'s poll
/// loop, which runs LAST in the same async block that holds every other
/// pair's already-collected measurement — a panic on a transient blip
/// would unwind and lose all of it. The poll loop retries an `Err` until
/// its deadline, same as an incomplete answer.
pub(crate) async fn loki_query_matrix(
    http: &reqwest::Client,
    base: &str,
    logql: &str,
    start_ns: u64,
    end_ns: u64,
    bucket_width_ns: u64,
    label_name: &str,
) -> Result<L4Measured, String> {
    // `bucket_width_seconds` — the only producer of a bucket width this
    // harness ever queries with — guarantees a whole-second width, so
    // this truncation is exact, not a precision loss.
    let step_s = (bucket_width_ns / 1_000_000_000).to_string();
    let resp = http
        .get(format!("{base}/loki/api/v1/query_range"))
        .query(&[
            ("query", logql),
            ("start", &start_ns.to_string()),
            ("end", &end_ns.to_string()),
            ("step", &step_s),
        ])
        .send()
        .await
        .map_err(|e| format!("query_range (matrix) transport error: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("query_range (matrix) body read error: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "loki query_range (matrix) returned {status}: {body}"
        ));
    }
    Ok((
        parse_loki_matrix(&body, label_name, bucket_width_ns)
            .map_err(|e| format!("parse loki matrix: {e}"))?,
        parse_loki_bytes_processed(&body).map_err(|e| format!("parse loki bytes: {e}"))?,
        parse_loki_fetched_bytes(&body).map_err(|e| format!("parse loki fetched bytes: {e}"))?,
    ))
}

/// Push a whole OTLP/JSON Lines corpus into Loki, batched by **encoded
/// bytes** ([`FLUSH_BYTES`] per request — sized for Loki's stock 4 MiB
/// internal gRPC cap *after* its OTLP→logproto inflation, see the
/// constant's doc) with a 500-`LogsData` secondary cap, via the retrying
/// pusher. Byte-capped because count-capped batching is blind to
/// heterogeneous batch sizes — run #2 died on a 500-batch push that
/// encoded to 5.28 MB (503 `ResourceExhausted`); adapting the pusher to
/// Loki's stock limit is the anti-strawman direction (a real OTLP
/// exporter batches under size limits too).
pub(crate) async fn push_corpus_to_loki(
    http: &reqwest::Client,
    base: &str,
    corpus_dir: &std::path::Path,
) {
    use prost::Message as _;
    use std::io::BufRead as _;

    /// The 4 MiB that matters is Loki's INTERNAL gRPC message, not our
    /// HTTP body: run #3 — under this constant's PREVIOUS value of
    /// 3 MiB — proved Loki's OTLP→logproto translation INFLATES the
    /// content (a ≤3 MiB push produced a 5,276,869-byte internal message,
    /// ≥1.76×, because OTLP shares resource/scope attributes per batch
    /// while the internal push repeats labels and structured metadata per
    /// entry). The current 1.5 MB gives ≥2.6× inflation headroom under
    /// the cap; `push_otlp` still asserts our own encoded size as a floor
    /// guarantee.
    const FLUSH_BYTES: usize = 1_500_000;

    let mut paths: Vec<_> = std::fs::read_dir(corpus_dir)
        .expect("read corpus dir")
        .filter_map(|e| {
            let p = e.expect("dir entry").path();
            (p.extension().and_then(|x| x.to_str()) == Some("jsonl")).then_some(p)
        })
        .collect();
    paths.sort();

    let mut pending: Vec<opentelemetry_proto::tonic::logs::v1::ResourceLogs> = Vec::new();
    let (mut pending_bytes, mut pending_lines) = (0usize, 0u64);
    let (mut batched, mut pushed) = (0u64, 0u64);
    for path in paths {
        let file = std::fs::File::open(&path).expect("open corpus file");
        for line in std::io::BufReader::new(file).lines() {
            let line = line.expect("read corpus line");
            if line.trim().is_empty() {
                continue;
            }
            let data: opentelemetry_proto::tonic::logs::v1::LogsData =
                serde_json::from_str(&line).expect("parse LogsData line");
            let line_bytes: usize = data
                .resource_logs
                .iter()
                .map(|rl| rl.encoded_len() + 8)
                .sum();
            // Flush BEFORE appending if this line would overflow the byte
            // cap — so no push ever exceeds it (a single oversized line
            // still goes alone; none in practice approaches 3 MiB).
            if !pending.is_empty()
                && (pending_bytes + line_bytes > FLUSH_BYTES || pending_lines >= 500)
            {
                let payload =
                    opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
                        resource_logs: std::mem::take(&mut pending),
                    }
                    .encode_to_vec();
                push_otlp(http, base, payload).await;
                (pending_bytes, pending_lines) = (0, 0);
                pushed += 1;
                if pushed % 500 == 0 {
                    eprintln!(
                        "loki ingest: {batched} LogsData lines read, {pushed} requests sent…"
                    );
                }
            }
            pending.extend(data.resource_logs);
            pending_bytes += line_bytes;
            pending_lines += 1;
            batched += 1;
        }
    }
    if !pending.is_empty() {
        let payload = opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest {
            resource_logs: pending,
        }
        .encode_to_vec();
        push_otlp(http, base, payload).await;
        pushed += 1;
    }
    eprintln!("loki ingest complete: {batched} LogsData lines in {pushed} requests");
}

/// The Loki half of the §3.6 latency channel: [`LATENCY_REPS`] timed
/// `query_range` round trips of EXACTLY the measurement query (same
/// `LogQL`, window, limit, direction as [`loki_query_with_stats`]),
/// median wall time, the body drained inside the timing — delivering
/// the result is part of the cost being measured. Runs post-poll
/// ([`loki_measure_pair`] succeeded), so ingest has caught up and the
/// reps are cache-warm, mirroring the Ourios side. A failed repetition
/// logs loudly and yields `None` rather than failing the run.
///
/// Reps run before the CROSS-SYSTEM equivalence assert (which needs
/// every pair measured first), but nothing is ever REPORTED from a run
/// where equivalence broke — the run panics before the report prints —
/// so published latencies always come from equivalence-held runs.
pub(crate) async fn loki_latency_p50(
    http: &reqwest::Client,
    base: &str,
    spec: &PairSpec,
) -> Option<Duration> {
    // The heavy strings (URL, window bounds) are built outside the
    // timed region; the per-rep request builder still runs inside it —
    // a client constructing its request is part of an honest round
    // trip. The body is chunk-drained (not buffered) inside the timing:
    // delivery is part of the measured cost, a whole-body buffer is not.
    let url = format!("{base}/loki/api/v1/query_range");
    let (start_s, end_s) = (spec.start.to_string(), spec.end.to_string());
    let mut samples = Vec::with_capacity(LATENCY_REPS);
    for _ in 0..LATENCY_REPS {
        let start = std::time::Instant::now();
        let sent = http
            .get(&url)
            .query(&[
                ("query", spec.logql.as_str()),
                ("start", start_s.as_str()),
                ("end", end_s.as_str()),
                ("limit", "5000"),
                ("direction", "forward"),
            ])
            .send()
            .await;
        let outcome = match sent {
            Ok(mut resp) => {
                let status = resp.status();
                let drained = loop {
                    match resp.chunk().await {
                        Ok(Some(_)) => {}
                        Ok(None) => break Ok(()),
                        Err(e) => break Err(format!("body read: {e}")),
                    }
                };
                match drained {
                    Ok(()) if status.is_success() => Ok(()),
                    Ok(()) => Err(format!("HTTP {status}")),
                    Err(detail) => Err(detail),
                }
            }
            Err(e) => Err(format!("transport: {e}")),
        };
        if let Err(detail) = outcome {
            eprintln!(
                "LATENCY UNMEASURED (loki) [{}]: a timed repetition failed: {detail}",
                spec.label,
            );
            return None;
        }
        samples.push(start.elapsed());
    }
    median_duration(&samples)
}

/// One pair's Loki measurement: poll `query_range` until ingest has
/// caught up to the expected row count. On a deadline miss it emits a
/// diagnostic dump (itself bounded to 90 s) and then returns `Err`
/// instead of panicking, so one pair's failure cannot destroy the other
/// pairs' already-measured report (run #11 lost three pairs'
/// measurements to one L3 timeout panic).
pub(crate) async fn loki_measure_pair(
    http: &reqwest::Client,
    base: &str,
    spec: &PairSpec,
) -> Result<(Vec<LineKey>, u64, LokiFetchedBytes), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let (lines, bytes, fetched) =
            loki_query_with_stats(http, base, &spec.logql, spec.start, spec.end).await;
        if lines.len() as u64 >= spec.expected_rows {
            break Ok((lines, bytes, fetched));
        }
        if std::time::Instant::now() >= deadline {
            // Bounded so diagnostics can't extend the miss indefinitely.
            if tokio::time::timeout(
                Duration::from_secs(90),
                dump_loki_diagnostics(http, base, spec),
            )
            .await
            .is_err()
            {
                eprintln!(
                    "(diagnostics dump for [{}] itself timed out after 90 s)",
                    spec.label,
                );
            }
            break Err(format!(
                "loki returned {} of {} expected rows for [{}] before timeout",
                lines.len(),
                spec.expected_rows,
                spec.label,
            ));
        }
        // 10 s, not 2 s: an expensive pair's poll (the L3 full-corpus
        // scan runs ~19 s of engine time) must not queue up behind
        // itself — run #13's failing query showed 321 s of queueTime
        // from 2 s polling, which ate the deadline in Loki's queue.
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// The L4 pair's Loki measurement: poll the matrix `query_range` until
/// ingest has caught up to the expected total row count — the
/// aggregation counterpart of [`loki_measure_pair`]. Returns `Err`
/// instead of panicking on a deadline miss, so an L4 failure cannot
/// destroy the already-measured/printed evidence for the other pairs
/// (same run #11 salvage lesson; L4 is measured and reported last, see
/// `rfc0031_indicative_comparative_run`).
///
/// Deadline matches [`loki_measure_pair`]'s 300 s — widening it to
/// 600/900 s (runs #7/#9) changed nothing, and bucket-aligning the
/// query window (run #11) narrowed but didn't close the gap either.
/// Six straight dispatches (#6-#11) all converged to a stable ~93-97%
/// regardless of which query-side knob moved, so a deadline miss now
/// also runs a plain, unaggregated line-filter count (bypassing
/// `count_over_time`/`regexp` entirely) alongside [`dump_loki_diagnostics`]
/// — decisive evidence for whether the shortfall is Loki never storing
/// the missing lines at ingest, or specific to the aggregation path,
/// instead of another guess.
///
/// Run #13 (a lower-frequency candidate, `L4_MIN_AVG_INTERVAL_SECONDS`)
/// still fell short (1144/1197), and a corpus-side check ruled out
/// exact `(timestamp, body)` collision entirely — every one of the
/// 1197 matching records has a unique timestamp AND unique body, so
/// Loki's documented dedup rule cannot be the mechanism here (or,
/// evidently, for the prior candidate either). Run #14 added a
/// `level=warn`/`level=error` scan of Loki's own container stderr and
/// came back with zero matches — whatever is happening, Loki doesn't
/// consider it log-worthy, ruling out rate limiting, out-of-order
/// rejection, and stream-limit drops (all of which log at WARN). A
/// deadline miss now also scrapes `/metrics` for
/// `loki_discarded_samples_total`/`loki_discarded_bytes_total` — Loki's
/// own dedicated counters for silent/expected discards, incremented
/// even when nothing is logged, labeled by `reason`.
pub(crate) async fn loki_measure_frequency_pair(
    http: &reqwest::Client,
    base: &str,
    container: &testcontainers_modules::testcontainers::ContainerAsync<
        testcontainers_modules::testcontainers::GenericImage,
    >,
    spec: &PairSpec,
    bucket_width_ns: u64,
) -> Result<L4Measured, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let (groups, bytes, fetched) = match loki_query_matrix(
            http,
            base,
            &spec.logql,
            spec.start,
            spec.end,
            bucket_width_ns,
            "value",
        )
        .await
        {
            Ok(measured) => measured,
            // A transient blip (transport error, a 5xx, a torn body) is
            // retried until the deadline, same as an incomplete answer —
            // panicking here would unwind the async block holding every
            // other pair's already-collected measurement.
            Err(detail) => {
                if std::time::Instant::now() >= deadline {
                    break Err(format!(
                        "loki matrix query for [{}] still failing at its poll deadline: \
                         {detail}",
                        spec.label,
                    ));
                }
                eprintln!(
                    "loki matrix query for [{}] failed (retrying until deadline): {detail}",
                    spec.label,
                );
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        let total: u64 = groups.values().sum();
        if total >= spec.expected_rows {
            break Ok((groups, bytes, fetched));
        }
        if std::time::Instant::now() >= deadline {
            // `expected_rows` is a real corpus row count, non-zero (the
            // picker's L4_MIN_ROWS floor) and capped at L4_MAX_ROWS —
            // nowhere near f64's 2^53 exact-integer range.
            #[allow(clippy::cast_precision_loss)]
            let completeness = total as f64 / spec.expected_rows as f64;
            if completeness >= L4_COMPLETENESS_MARGIN {
                eprintln!(
                    "loki returned {total} of {} expected rows for [{}] — short of exact but \
                     within the {:.0}% completeness margin (RFC 0031 §7, 2026-07-17); \
                     accepting",
                    spec.expected_rows,
                    spec.label,
                    L4_COMPLETENESS_MARGIN * 100.0,
                );
                break Ok((groups, bytes, fetched));
            }
            // Below the margin — this run will fail, so the expensive
            // diagnostics (each its own HTTP round trip, up to 90s+60s
            // of timeout budget) are worth paying for here, unlike the
            // common case above where they'd just be noise on an
            // already-accepted result.
            dump_l4_shortfall_diagnostics(http, base, container, spec).await;
            break Err(format!(
                "loki returned {total} of {} expected rows for [{}] before timeout — below \
                 the {:.0}% completeness margin",
                spec.expected_rows,
                spec.label,
                L4_COMPLETENESS_MARGIN * 100.0,
            ));
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// The full L4 shortfall post-mortem, run only when a poll deadline
/// passed AND completeness fell below [`L4_COMPLETENESS_MARGIN`] (i.e.
/// the run is about to fail): the general timed-out-pair diagnostics,
/// Loki's own container `level=warn`/`level=error` lines, its
/// discarded-samples counters, and the plain line-filter count.
///
/// That last probe is the decisive metric-vs-plain-query split — NOT
/// ingest-vs-query: a plain line-filter count (no `count_over_time`, no
/// `| regexp`) still goes through `query_range`, so a shortfall here
/// doesn't prove Loki never STORED the missing lines (that would require
/// checking the ingester/store directly, which this harness doesn't do).
/// What it DOES prove: whether the shortfall is specific to the
/// metric-aggregation machinery, or affects a plain streams query too
/// (in which case it's the same wide-time-range query incompleteness
/// RFC 0031 §7 ultimately characterizes, not an artifact of
/// `count_over_time`/`regexp`). Every prior fix (cache, entries-limit,
/// bucket alignment) moved the number without closing the gap — six
/// straight dispatches at a stable ~93-97%, so guessing another
/// query-side knob isn't warranted without this evidence.
pub(crate) async fn dump_l4_shortfall_diagnostics(
    http: &reqwest::Client,
    base: &str,
    container: &testcontainers_modules::testcontainers::ContainerAsync<
        testcontainers_modules::testcontainers::GenericImage,
    >,
    spec: &PairSpec,
) {
    if tokio::time::timeout(
        Duration::from_secs(90),
        dump_loki_diagnostics(http, base, spec),
    )
    .await
    .is_err()
    {
        eprintln!(
            "(diagnostics dump for [{}] itself timed out after 90 s)",
            spec.label,
        );
    }
    dump_loki_container_warnings(container, spec).await;
    dump_loki_discard_metrics(http, base, spec).await;
    if let Some(needle) = l4_needle_from_logql(&spec.logql) {
        match tokio::time::timeout(
            Duration::from_secs(60),
            loki_query_range_uncapped(
                http,
                base,
                &format!(r#"{{service_name=~".+"}} |= "{needle}""#),
                spec.start,
                spec.end,
                spec.expected_rows,
            ),
        )
        .await
        {
            Ok(Ok(count)) => eprintln!(
                "plain line-filter count for [{}]: {count} of {} expected \
                 (bypasses count_over_time/regexp entirely — a shortfall here means \
                 the loss isn't specific to the metric-aggregation path; it's still \
                 a query_range call, so this doesn't prove ingest-side loss)",
                spec.label, spec.expected_rows,
            ),
            Ok(Err(detail)) => eprintln!(
                "(plain line-filter count for [{}] itself failed: {detail})",
                spec.label,
            ),
            Err(_) => eprintln!(
                "(plain line-filter count for [{}] itself timed out after 60 s)",
                spec.label,
            ),
        }
    }
}

/// Pull the bare `|= "..."` needle back out of an L4 [`PairSpec`]'s
/// `logql` (built by [`l4_pair_spec`]) for the plain-count diagnostic —
/// parsing rather than threading a new parameter through, since this is
/// diagnostic-only code and a parse miss should just skip the probe, not
/// fail the run.
pub(crate) fn l4_needle_from_logql(logql: &str) -> Option<&str> {
    let after = logql.split_once("|= \"")?.1;
    let (needle, _) = after.split_once('"')?;
    Some(needle)
}

/// Diagnostic-only: a plain streams count, unlike [`loki_query_range`],
/// whose `limit=5000` is deliberately tuned to the equivalence pairs'
/// (≤4000-row) expectations and would silently under-report an L4 pair
/// whose `expected_rows` exceeds it. `limit` is set to `expected_rows`
/// rounded up with headroom, not a fixed constant, so the probe can
/// never itself be the reason the count looks short.
///
/// Returns `Err` rather than panicking on any failure (unlike
/// [`loki_query_range`]'s and [`loki_query_with_stats`]'s `expect`-based
/// style): this runs on the deadline-miss path of an already-flaky L4
/// candidate, inside the same `runtime.block_on` that gathers L1–L3/L6's
/// evidence — a `panic!`/`expect!`/`assert!` here would unwind the whole
/// async block, losing that evidence too, defeating the "print before
/// assert" salvage design this diagnostic path exists alongside
/// ([`dump_loki_diagnostics`] and its siblings are already panic-free for
/// the same reason).
pub(crate) async fn loki_query_range_uncapped(
    http: &reqwest::Client,
    base: &str,
    logql: &str,
    start: u64,
    end: u64,
    expected_rows: u64,
) -> Result<u64, String> {
    let limit = expected_rows.saturating_mul(2).max(5000);
    let resp = http
        .get(format!("{base}/loki/api/v1/query_range"))
        .query(&[
            ("query", logql),
            ("start", &start.to_string()),
            ("end", &end.to_string()),
            ("limit", &limit.to_string()),
            ("direction", "forward"),
        ])
        .send()
        .await
        .map_err(|e| format!("query_range (uncapped diagnostic) transport error: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("query_range (uncapped diagnostic) body read error: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "loki query_range (uncapped diagnostic) returned {status}: {body}"
        ));
    }
    parse_loki_streams(&body)
        .map(|lines| lines.len() as u64)
        .map_err(|e| format!("parse loki streams (uncapped diagnostic): {e}"))
}

/// Split successes from failures: equivalence + report run for every
/// measured pair BEFORE the run fails on the broken ones, so one pair's
/// timeout cannot destroy the others' 40-minute measurements. The
/// trailing `Option<Duration>` is the pair's Loki `latency_p50` —
/// `None` when the (corroborating) latency channel failed on an
/// otherwise-good pair, reported as "unmeasured".
pub(crate) type Measured = (Vec<LineKey>, u64, LokiFetchedBytes, Option<Duration>);

/// The L4 pair's Loki measurement — the aggregation counterpart of
/// [`Measured`]: a `(bucket, group) -> count` map rather than a
/// [`LineKey`] multiset, with no latency channel this slice wires (see
/// [`run_l4_pair`]'s doc).
pub(crate) type L4Measured = (HashMap<AggKey, u64>, u64, LokiFetchedBytes);

/// Timed-out pair post-mortem, printed to stderr so the run log carries
/// the evidence: the raw (truncated) response body of the failing query,
/// and a filterless sample of the same window so the entries' shape —
/// including whether structured metadata is present at all on replayed
/// data — is visible. Bodies are truncated to ~4 KiB (4096 chars).
pub(crate) async fn dump_loki_diagnostics(http: &reqwest::Client, base: &str, spec: &PairSpec) {
    let raw = |query: &str, limit: u32| {
        let query = query.to_string();
        async move {
            match http
                .get(format!("{base}/loki/api/v1/query_range"))
                .query(&[
                    ("query", query.as_str()),
                    ("start", &spec.start.to_string()),
                    ("end", &spec.end.to_string()),
                    ("limit", &limit.to_string()),
                    // Mirror the measurement request so the dump reflects
                    // exactly what the poll saw.
                    ("direction", "forward"),
                ])
                .send()
                .await
            {
                Ok(mut resp) => {
                    let status = resp.status();
                    // Stream at most ~8 KiB rather than buffering a whole
                    // (possibly huge) body just to print a snippet.
                    let mut buf: Vec<u8> = Vec::with_capacity(8192);
                    loop {
                        match resp.chunk().await {
                            Ok(Some(chunk)) => {
                                buf.extend_from_slice(&chunk);
                                if buf.len() >= 8192 {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            // A truncated-by-error body must say so, or the
                            // post-mortem reads as a complete response.
                            Err(e) => {
                                buf.extend_from_slice(format!("<body read error: {e}>").as_bytes());
                                break;
                            }
                        }
                    }
                    // Truncate on a char boundary; the cap is ~4 KiB of
                    // ASCII JSON, not an exact byte count.
                    let body: String = String::from_utf8_lossy(&buf).chars().take(4096).collect();
                    format!("HTTP {status}: {body}")
                }
                Err(e) => format!("transport error: {e}"),
            }
        }
    };
    eprintln!(
        "=== diagnostics for timed-out pair [{}] ===\nfailing query {:?} =>\n{}\n\n\
         filterless sample of the same window =>\n{}\n=== end diagnostics ===",
        spec.label,
        spec.logql,
        raw(&spec.logql, 5000).await,
        raw("{service_name=~\".+\"}", 3).await,
    );
}

/// Loki's own logs, not just its HTTP answers: the ingester logs at
/// `level=warn`/`level=error` for rate limiting, out-of-order
/// rejection, and stream-limit drops — none of which surface in a
/// query response or the OTLP push's `partial_success`. Matched on the
/// `level=` FIELD, not a bare substring: run #14 showed a naive
/// "contains warn" filter drowning in false positives from query text
/// like `severity_text="WARN"` inside `level=info` lines.
pub(crate) async fn dump_loki_container_warnings(
    container: &testcontainers_modules::testcontainers::ContainerAsync<
        testcontainers_modules::testcontainers::GenericImage,
    >,
    spec: &PairSpec,
) {
    match container.stderr_to_vec().await {
        Ok(stderr) => {
            let text = String::from_utf8_lossy(&stderr);
            let relevant: Vec<&str> = text
                .lines()
                .filter(|l| l.contains("level=warn") || l.contains("level=error"))
                .collect();
            eprintln!(
                "loki container stderr for [{}] — {} level=warn/error line(s) out of {} \
                 total:",
                spec.label,
                relevant.len(),
                text.lines().count(),
            );
            for line in relevant.iter().rev().take(200).rev() {
                eprintln!("  {line}");
            }
        }
        Err(e) => eprintln!(
            "(couldn't read loki container stderr for [{}]: {e})",
            spec.label
        ),
    }
}

/// Loki's OWN accounting for silent/expected drops: the distributor
/// increments `loki_discarded_samples_total` (labeled by `reason` —
/// `rate_limited`, `out_of_order`, `too_many_streams`, `line_too_long`,
/// ...) even when nothing is logged, since a single discarded sample
/// isn't always WARN-worthy on its own ([`dump_loki_container_warnings`]
/// came back clean on run #14). This is Loki's dedicated answer to
/// "how much did you drop and why."
pub(crate) async fn dump_loki_discard_metrics(http: &reqwest::Client, base: &str, spec: &PairSpec) {
    match tokio::time::timeout(
        Duration::from_secs(30),
        http.get(format!("{base}/metrics")).send(),
    )
    .await
    {
        Ok(Ok(resp)) => match resp.text().await {
            Ok(body) => {
                let discard_lines: Vec<&str> = body
                    .lines()
                    .filter(|l| {
                        !l.starts_with('#')
                            && (l.starts_with("loki_discarded_samples_total")
                                || l.starts_with("loki_discarded_bytes_total"))
                    })
                    .collect();
                eprintln!(
                    "loki /metrics discarded-samples counters for [{}] — {} \
                     non-zero-eligible series:",
                    spec.label,
                    discard_lines.len(),
                );
                for line in &discard_lines {
                    eprintln!("  {line}");
                }
            }
            Err(e) => eprintln!(
                "(couldn't read loki /metrics body for [{}]: {e})",
                spec.label
            ),
        },
        Ok(Err(e)) => eprintln!("(loki /metrics request failed for [{}]: {e})", spec.label),
        Err(_) => eprintln!(
            "(loki /metrics request for [{}] itself timed out after 30 s)",
            spec.label,
        ),
    }
}
