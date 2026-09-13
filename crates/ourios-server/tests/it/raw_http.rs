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
/// body satisfying a declared `Content-Length`.
///
/// **`Content-Length` is the only framing recognised**, deliberately. Nothing
/// these suites query answers with `Transfer-Encoding: chunked` — the served
/// binary sets a length on every response they assert — so a chunked branch
/// would be unreachable, and an unreachable parser that looks right is worse
/// than none: an earlier draft's `ends_with("0\r\n\r\n")` would have rejected
/// a perfectly valid chunked response carrying trailer fields. If a chunked
/// response ever arrives here it falls through as incomplete and panics with
/// the text, which is the honest outcome for a case this helper has never seen.
///
/// A response with no declared framing is framed by the close itself, which a
/// reset cannot distinguish from a truncation, so that is incomplete too rather
/// than being waved through.
fn is_complete(text: &str) -> bool {
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .is_some_and(|want| body.len() >= want)
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

    /// Chunked is not recognised, on purpose — see `is_complete`. A chunked
    /// response reaching here should fail loudly rather than be judged by a
    /// parser nothing exercises.
    #[test]
    fn a_chunked_response_is_not_recognised() {
        let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(!is_complete(&format!("{head}5\r\nhello\r\n0\r\n\r\n")));
    }

    /// Close-framed: a reset cannot be told from a truncation, so it is not
    /// waved through.
    #[test]
    fn a_response_with_no_declared_framing_is_not_complete() {
        assert!(!is_complete("HTTP/1.1 200 OK\r\nServer: x\r\n\r\nbody"));
    }
}
