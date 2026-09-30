//! The listener events carry registry names, and each one is emitted and
//! live-checked (#873): the accept failure, a connection that ends with
//! an error, and the TLS certificate reload completing and failing.
//!
//! Its own test binary: `ourios_telemetry::live_check` installs the
//! process-global `tracing` subscriber it reads the events from.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use ourios_semconv as semconv;
use ourios_serving::serve::{PlainListener, accept_backoff, serve_http};
use ourios_serving::tls::{ALPN_HTTP, TlsSettings};
use ourios_serving::tls_serve::{LISTENER_HTTP, reloading_acceptor};
use ourios_telemetry::live_check::{self, Checked, Event, EventCapture, EventSpec};
use tokio::io::AsyncWriteExt as _;

const WITH_LISTENER: &[&str] = &[semconv::OURIOS_SERVER_LISTENER_NAME];
const FAILURE_ON_LISTENER: &[&str] = &["error.type", semconv::OURIOS_SERVER_LISTENER_NAME];

/// The listener events as the registry declares them.
const LISTENER_EVENTS: [EventSpec; 4] = [
    EventSpec {
        name: semconv::EVENT_OURIOS_SERVER_LISTENER_ACCEPT_ERROR,
        required: FAILURE_ON_LISTENER,
        optional: &[],
    },
    EventSpec {
        name: semconv::EVENT_OURIOS_SERVER_LISTENER_CONNECTION_ERROR,
        required: FAILURE_ON_LISTENER,
        optional: &[],
    },
    EventSpec {
        name: semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_COMPLETED,
        required: WITH_LISTENER,
        optional: &[],
    },
    EventSpec {
        name: semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_ERROR,
        required: FAILURE_ON_LISTENER,
        optional: &[],
    },
];

/// A listener's accept stream that fails with each error in turn.
struct FailingAccepts(VecDeque<io::Error>);

impl Stream for FailingAccepts {
    type Item = io::Result<()>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.get_mut().0.pop_front().map(Err))
    }
}

async fn wait_for(
    capture: &EventCapture,
    what: &str,
    found: impl Fn(&[Event]) -> bool,
) -> Vec<Event> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let events = capture.events();
            if found(&events) {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} within the deadline"))
}

fn has(events: &[Event], name: &str, error_type: Option<&str>) -> bool {
    events
        .iter()
        .any(|e| e.name == name && e.error_type() == error_type)
}

async fn drive_accept_failures() {
    let mut accepts = accept_backoff(
        FailingAccepts(VecDeque::from([
            io::Error::from(io::ErrorKind::ConnectionReset),
            io::Error::other("an accept failure with no class of its own"),
        ])),
        LISTENER_HTTP,
    );
    while std::future::poll_fn(|cx| Pin::new(&mut accepts).poll_next(cx))
        .await
        .is_some()
    {}
}

async fn drive_connection_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(serve_http(
        PlainListener::new(listener, LISTENER_HTTP),
        axum::Router::new(),
        std::future::pending::<()>(),
    ));
    let mut client = tokio::net::TcpStream::connect(addr).await.expect("connect");
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .expect("a partial request head");
    client.shutdown().await.expect("close mid-head");
    drop(client);
    wait_for(
        live_check::event_capture().expect("capture"),
        "the connection error",
        |events| {
            events.iter().any(|e| {
                e.name == semconv::EVENT_OURIOS_SERVER_LISTENER_CONNECTION_ERROR
                    && e.attributes.get(semconv::OURIOS_SERVER_LISTENER_NAME)
                        == Some(&LISTENER_HTTP.to_owned())
            })
        },
    )
    .await;
    server.abort();
}

async fn drive_tls_reloads(capture: &EventCapture) {
    let tmp = tempfile::TempDir::new().expect("temp");
    let cert = tmp.path().join("server.crt");
    let key = tmp.path().join("server.key");
    let mint =
        || rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("mint a cert");
    let first = mint();
    std::fs::write(&cert, first.cert.pem()).expect("write cert");
    std::fs::write(&key, first.signing_key.serialize_pem()).expect("write key");
    let settings = TlsSettings::from_parts(
        "receiver.http_tls",
        Some(&cert.display().to_string()),
        Some(&key.display().to_string()),
        None,
        None,
        Some("1"),
    )
    .expect("valid settings")
    .expect("configured");
    let _acceptor = reloading_acceptor(&settings, ALPN_HTTP, LISTENER_HTTP).expect("acceptor");

    let second = mint();
    std::fs::write(&cert, second.cert.pem()).expect("write cert");
    std::fs::write(&key, second.signing_key.serialize_pem()).expect("write key");
    wait_for(capture, "the reload", |events| {
        has(
            events,
            semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_COMPLETED,
            None,
        )
    })
    .await;

    std::fs::write(&cert, b"not a certificate").expect("write garbage");
    wait_for(capture, "the invalid reload", |events| {
        has(
            events,
            semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_ERROR,
            Some("invalid"),
        )
    })
    .await;

    std::fs::remove_file(&key).expect("remove the key");
    wait_for(capture, "the unreadable reload", |events| {
        has(
            events,
            semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_ERROR,
            Some("unreadable"),
        )
    })
    .await;

    ourios_serving::tls_serve::panic_next_reload();
    let events = wait_for(capture, "the panicked reload", |events| {
        has(
            events,
            semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_ERROR,
            Some("panic"),
        )
    })
    .await;
    assert!(
        events.iter().any(|e| {
            e.name == semconv::EVENT_OURIOS_SERVER_TLS_RELOAD_ERROR
                && e.error_type() == Some("panic")
                && e.attributes.get(semconv::OURIOS_SERVER_LISTENER_NAME)
                    == Some(&LISTENER_HTTP.to_owned())
                && e.body
                    .as_deref()
                    .is_some_and(|body| body.contains(&cert.display().to_string()))
        }),
        "the unwound reload names its listener and its certificate path"
    );
}

/// #873 — every listener event is emitted under its registry name with
/// only registry attributes, and live-checked where weaver is configured.
#[tokio::test]
async fn every_listener_event_is_named_and_live_checked() {
    let capture = live_check::event_capture().expect("the only subscriber this binary installs");
    capture.reset();

    drive_accept_failures().await;
    drive_connection_error().await;
    drive_tls_reloads(capture).await;

    let events = capture.events();
    for (class, pause) in [("connection_reset", false), ("_OTHER", true)] {
        assert!(
            events.iter().any(|e| {
                e.name == semconv::EVENT_OURIOS_SERVER_LISTENER_ACCEPT_ERROR
                    && e.error_type() == Some(class)
                    && e.attributes.get(semconv::OURIOS_SERVER_LISTENER_NAME)
                        == Some(&LISTENER_HTTP.to_owned())
            }),
            "an accept failure classed {class} (backs off: {pause}) names its listener"
        );
    }
    let checked = live_check::live_check(&events, &LISTENER_EVENTS)
        .expect("every listener event is emitted and registry-conformant");
    if checked == Checked::SpecOnly {
        eprintln!("#873: weaver is not configured here; checked each event against its spec only");
    }
}
