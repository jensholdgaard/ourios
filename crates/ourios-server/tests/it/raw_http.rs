//! Reading a raw HTTP response off a `TcpStream`, tolerantly.
//!
//! Several suites here hand-write an HTTP/1.1 request to the served binary and
//! read the reply back. They all used `read_to_end`, which demands a clean
//! `FIN` — and this server does not always give one. The handler can reject a
//! request before consuming its body, and closing with unread data still in the
//! receive queue makes the stack send `RST` rather than `FIN`, so the read
//! fails *after* the complete response has already arrived.
//!
//! That made `rfc0046_out_of_band_tenancy` flake at roughly 30% locally (issue
//! #799): it was asserting on the connection's termination style rather than on
//! what the server said. The same pattern sits in six sibling suites, which is
//! why this lives in one place.

use std::io::ErrorKind;

use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// Read until EOF, treating a `ConnectionReset` that arrives **after** response
/// bytes as the end of the response.
///
/// A reset with **nothing** received still panics, and is a server-side fault
/// rather than this helper's problem: a rejected request must get its response
/// flushed before the close. Keeping that case loud is the point — a blanket
/// "ignore resets" would hide exactly the bug worth finding.
pub async fn read_response(stream: &mut TcpStream) -> String {
    let mut response = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::ConnectionReset && !response.is_empty() => break,
            Err(e) => panic!(
                "read failed after {} response byte(s): {e:?} — a reset with no \
                 response at all is a server-side fault, not this helper's",
                response.len(),
            ),
        }
    }
    String::from_utf8_lossy(&response).into_owned()
}
