//! Pooled object-store connections must not outlive the remote's close
//! (#791). An S3-compatible store closes idle keep-alive connections; the
//! client only learns of that when the task owning the connection is polled.
//! That task runs on whichever runtime issued the request, so a caller whose
//! runtime stops being driven left the socket in `CLOSE_WAIT`, and later
//! handed the dead connection to the next request.
//!
//! The fake is a keep-alive HTTP/1.1 server over `std::net` that closes a
//! connection once it has been idle, then waits to see whether the client
//! closes its side. It counts outcomes from the server end, so no `/proc`.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;
use ourios_parquet::{S3Config, Store};

/// How long the client has to close its side after the server closes.
const LINGER: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Counts {
    accepted: AtomicUsize,
    /// The client closed its side: the descriptor went back to the OS.
    closed_by_client: AtomicUsize,
    /// The server closed and the client kept its side open (`CLOSE_WAIT`).
    held_by_client: AtomicUsize,
    /// The client sent a request on a connection the server had closed.
    reused_after_close: AtomicUsize,
}

impl Counts {
    fn get(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    fn open(&self) -> usize {
        Self::get(&self.accepted) - Self::get(&self.closed_by_client)
    }
}

#[derive(Clone, Copy)]
struct Behaviour {
    /// The server closes a connection idle for this long.
    idle: Duration,
    /// Hold every response until this many connections are open, so a
    /// concurrent burst needs one connection per request.
    respond_once_accepted: usize,
}

/// Serve requests on `stream` until the client closes it or it idles out.
fn serve(stream: &TcpStream, behaviour: Behaviour, counts: &Counts) {
    let mut reader = BufReader::new(stream);
    loop {
        stream
            .set_read_timeout(Some(behaviour.idle))
            .expect("timeout");
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                counts.closed_by_client.fetch_add(1, Ordering::SeqCst);
                return;
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                close_idle(stream, &mut reader, counts);
                return;
            }
            Err(_) => return,
        }
        if skip_request_rest(&mut reader).is_err() {
            return;
        }
        wait_for_accepted(counts, behaviour.respond_once_accepted);
        if respond(stream).is_err() {
            return;
        }
    }
}

fn wait_for_accepted(counts: &Counts, target: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Counts::get(&counts.accepted) < target {
        assert!(Instant::now() < deadline, "only some connections opened");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Close our side of an idle connection and record what the client does.
fn close_idle(stream: &TcpStream, reader: &mut BufReader<&TcpStream>, counts: &Counts) {
    let _ = stream.shutdown(Shutdown::Write);
    stream.set_read_timeout(Some(LINGER)).expect("timeout");
    let outcome = match reader.read(&mut [0; 64]) {
        Ok(0) => &counts.closed_by_client,
        Ok(_) => &counts.reused_after_close,
        Err(_) => &counts.held_by_client,
    };
    outcome.fetch_add(1, Ordering::SeqCst);
}

/// Consume the headers and body that follow a request line.
fn skip_request_rest(reader: &mut BufReader<&TcpStream>) -> std::io::Result<()> {
    let mut len = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().expect("content-length");
        }
    }
    reader.read_exact(&mut vec![0; len])
}

fn respond(mut stream: &TcpStream) -> std::io::Result<()> {
    let body = b"hello";
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"e1\"\r\n\
         Last-Modified: Thu, 01 Jan 2026 00:00:00 GMT\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)
}

fn fake_store(behaviour: Behaviour) -> (Store, Arc<Counts>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let counts = Arc::new(Counts::default());
    let served = Arc::clone(&counts);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            served.accepted.fetch_add(1, Ordering::SeqCst);
            let served = Arc::clone(&served);
            std::thread::spawn(move || serve(&stream, behaviour, &served));
        }
    });
    let store = Store::s3(
        S3Config::new("bucket")
            .with_endpoint(format!("http://{addr}"))
            .with_region("us-east-1")
            .with_access_key_id("test")
            .with_secret_access_key("test"),
    )
    .expect("build s3 store");
    (store, counts)
}

const IDLE: Duration = Duration::from_millis(100);

/// Let every idle close run its course, including the linger.
fn settle() {
    std::thread::sleep(IDLE * 3 + LINGER);
}

fn assert_every_close_observed(counts: &Counts) {
    assert_eq!(
        (
            Counts::get(&counts.held_by_client),
            Counts::get(&counts.reused_after_close),
            counts.open(),
        ),
        (0, 0, 0),
        "of {} connections, the client held some open after the store closed \
         them, or sent a request on one",
        Counts::get(&counts.accepted),
    );
}

/// A caller whose runtime is not driven between requests — a current-thread
/// runtime, or a multi-thread one whose workers are busy — still has every
/// connection closed once the store closes it, on the async surface and on
/// the raw backend the querier hands to `DataFusion`.
#[test]
fn an_idle_caller_runtime_strands_no_connection() {
    let (store, counts) = fake_store(Behaviour {
        idle: IDLE,
        respond_once_accepted: 0,
    });
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("caller runtime");
    let raw = store.object_store();
    for _ in 0..4 {
        caller.block_on(store.get("k")).expect("get");
        std::thread::sleep(IDLE * 3);
        caller
            .block_on(async { raw.get(&ObjectPath::from("k")).await?.bytes().await })
            .expect("raw get");
        std::thread::sleep(IDLE * 3);
    }
    settle();

    assert_every_close_observed(&counts);
}

/// The sync bridge closes every connection the store closes.
#[test]
fn bridged_calls_strand_no_connection() {
    let (store, counts) = fake_store(Behaviour {
        idle: IDLE,
        respond_once_accepted: 0,
    });
    for _ in 0..8 {
        store.get_blocking("k").expect("get");
        std::thread::sleep(IDLE * 3);
    }
    settle();

    assert_every_close_observed(&counts);
}

/// The default idle-connection cap per host `Store::s3` configures.
const IDLE_CONNECTIONS_KEPT: usize = 32;

/// A burst of concurrent requests opens one connection each; once it is
/// over, the pool keeps exactly its cap idle and closes the rest.
#[test]
fn the_pool_keeps_a_bounded_number_of_idle_connections() {
    const BURST: usize = 96;
    let (store, counts) = fake_store(Behaviour {
        idle: Duration::from_secs(30),
        respond_once_accepted: BURST,
    });
    let caller = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("caller runtime");
    caller
        .block_on(futures::future::try_join_all(
            (0..BURST).map(|_| store.get("k")),
        ))
        .expect("burst");
    std::thread::sleep(Duration::from_millis(500));

    assert_eq!(Counts::get(&counts.accepted), BURST, "one connection each");
    assert_eq!(
        counts.open(),
        IDLE_CONNECTIONS_KEPT,
        "idle connections the pool kept of {BURST}"
    );
}
