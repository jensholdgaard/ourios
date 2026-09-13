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

/// Read until EOF, treating a `ConnectionReset` that arrives after a
/// **complete** response as the end of it.
///
/// "Complete" is checked, not assumed. A peer can reset part-way through the
/// headers or the body, and several callers only assert
/// `starts_with("HTTP/1.1 200")` — so swallowing any non-empty prefix would let
/// a truncated response pass, trading a flake for a false pass. On a reset the
/// response must have a full header block and a body matching its framing; a
/// clean EOF needs no such check, because the server closed deliberately.
///
/// A reset before a complete response still panics, and is a server-side fault
/// rather than this helper's problem: a rejected request must get its response
/// flushed before the close. Keeping that loud is the point — a blanket
/// "ignore resets" would hide exactly the bug worth finding.
pub async fn read_response(stream: &mut TcpStream) -> String {
    let mut response = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    let mut reset = false;
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                reset = true;
                break;
            }
            Err(e) => panic!(
                "read failed after {} response byte(s): {e:?}",
                response.len(),
            ),
        }
    }
    let text = String::from_utf8_lossy(&response).into_owned();
    if reset {
        assert!(
            is_complete(&text),
            "connection reset before a complete HTTP response ({} byte(s) \
             received). A reset AFTER a complete response is expected here, but \
             a truncated one means the server closed before flushing — a \
             server-side fault, not this helper's. Received: {text:?}",
            response.len(),
        );
    }
    text
}

/// Whether `text` is a whole HTTP/1.1 response: a full header block, plus a
/// body matching whatever framing the headers declare.
///
/// `Content-Length` and `Transfer-Encoding: chunked` are the two framings these
/// suites can see. A response with **neither** is framed by the close itself,
/// which a reset cannot distinguish from a truncation — so that counts as
/// incomplete rather than being waved through.
fn is_complete(text: &str) -> bool {
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let header_value = |name: &str| {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_ascii_lowercase())
    };
    match header_value("content-length") {
        Some(declared) => declared
            .parse::<usize>()
            .is_ok_and(|want| body.len() >= want),
        None => header_value("transfer-encoding")
            .is_some_and(|encoding| encoding.contains("chunked") && body.ends_with("0\r\n\r\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::is_complete;

    #[test]
    fn a_content_length_framed_response_is_complete_only_when_the_body_arrived() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";
        assert!(is_complete(&format!("{head}hello")));
        assert!(!is_complete(&format!("{head}hel")));
    }

    /// The case that motivated the check: a reset after only the status line
    /// must not read as a complete 200, because callers assert on the prefix.
    #[test]
    fn a_truncated_header_block_is_not_complete() {
        assert!(!is_complete("HTTP/1.1 200 OK\r\nContent-Len"));
        assert!(!is_complete("HTTP/1.1 200 OK\r\n"));
        assert!(!is_complete(""));
    }

    #[test]
    fn a_chunked_response_needs_its_terminator() {
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(is_complete(&format!("{head}5\r\nhello\r\n0\r\n\r\n")));
        assert!(!is_complete(&format!("{head}5\r\nhello\r\n")));
    }

    /// Close-framed: a reset cannot be told from a truncation, so it is not
    /// waved through.
    #[test]
    fn a_response_with_no_declared_framing_is_not_complete() {
        assert!(!is_complete("HTTP/1.1 200 OK\r\nServer: x\r\n\r\nbody"));
    }
}
