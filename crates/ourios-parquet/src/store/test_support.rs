//! A fake S3 endpoint for the store's transport, metrics and span tests.

use std::io::{Read, Write};

use super::S3Config;

/// The configuration of a store against the fake endpoint on `port`.
pub(super) fn s3_config(port: u16) -> S3Config {
    S3Config::new("bucket")
        .with_endpoint(format!("http://127.0.0.1:{port}"))
        .with_region("us-east-1")
        .with_access_key_id("key")
        .with_secret_access_key("secret")
}

/// A fake S3 endpoint answering each request on its own connection with
/// the next of `responses`. Returns its port.
pub(super) fn fake_s3(responses: Vec<&'static str>) -> u16 {
    let count = responses.len();
    let mut responses = responses.into_iter();
    serve(count, move |_| responses.next().map(str::to_owned))
}

/// A fake S3 endpoint serving `connections` connections, one request
/// each, answered by `respond` from the request head. `None` leaves the
/// request unanswered until the client hangs up. Returns its port.
pub(super) fn serve(
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
