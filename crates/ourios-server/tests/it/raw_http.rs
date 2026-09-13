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

use tokio::io::{AsyncRead, AsyncReadExt};

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
/// Generic over the reader so the loop and the reset policy are testable
/// deterministically: a socket test would depend on whether the peer's `RST`
/// lands before or after the client drains its buffer, which is the kind of
/// timing that makes a test about flakes flaky. Callers pass `&mut TcpStream`
/// unchanged.
pub async fn read_response<R: AsyncRead + Unpin>(stream: &mut R) -> String {
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
    use super::{is_complete, read_response};
    use std::collections::VecDeque;
    use std::io::{Error, ErrorKind, Result};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, ReadBuf};

    /// A reader that yields a scripted sequence of chunks and errors.
    ///
    /// The point of scripting it is determinism: driving the real loop over a
    /// socket would depend on whether the peer's `RST` arrives before or after
    /// the client drains its receive buffer, so a test about a flake would
    /// itself be flaky.
    struct Scripted(VecDeque<Result<&'static str>>);

    impl AsyncRead for Scripted {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            match self.0.pop_front() {
                // An exhausted script is EOF, which `read_response` treats as a
                // clean close.
                None => Poll::Ready(Ok(())),
                Some(Ok(bytes)) => {
                    buf.put_slice(bytes.as_bytes());
                    Poll::Ready(Ok(()))
                }
                Some(Err(e)) => Poll::Ready(Err(e)),
            }
        }
    }

    fn scripted(steps: Vec<Result<&'static str>>) -> Scripted {
        Scripted(steps.into_iter().collect())
    }

    fn reset() -> Result<&'static str> {
        Err(Error::new(ErrorKind::ConnectionReset, "peer reset"))
    }

    const OK_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n";

    /// The flake this PR fixes: the complete response arrives, then the peer
    /// resets instead of closing cleanly.
    #[tokio::test]
    async fn a_reset_after_a_complete_response_returns_it() {
        let mut reader = scripted(vec![Ok(OK_HEAD), Ok("hello"), reset()]);
        assert_eq!(read_response(&mut reader).await, format!("{OK_HEAD}hello"));
    }

    /// Arriving in one chunk must behave the same — the loop must not depend on
    /// the response being split across reads.
    #[tokio::test]
    async fn a_reset_after_a_single_chunk_response_returns_it() {
        let whole: &'static str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        let mut reader = scripted(vec![Ok(whole), reset()]);
        assert_eq!(read_response(&mut reader).await, whole);
    }

    /// The false-pass case: a reset mid-body must NOT be swallowed, or a caller
    /// asserting only on the status line would accept a truncated reply.
    #[tokio::test]
    #[should_panic(expected = "connection reset before a complete HTTP response")]
    async fn a_reset_inside_the_body_panics() {
        let mut reader = scripted(vec![Ok(OK_HEAD), Ok("hel"), reset()]);
        let _ = read_response(&mut reader).await;
    }

    /// And a reset part-way through the headers, which is the case that would
    /// otherwise look like a passing 200.
    #[tokio::test]
    #[should_panic(expected = "connection reset before a complete HTTP response")]
    async fn a_reset_inside_the_headers_panics() {
        let mut reader = scripted(vec![Ok("HTTP/1.1 200 OK\r\nContent-Len"), reset()]);
        let _ = read_response(&mut reader).await;
    }

    /// A reset with nothing received at all — the server-side fault the helper
    /// must keep loud rather than report as an empty response.
    #[tokio::test]
    #[should_panic(expected = "connection reset before a complete HTTP response")]
    async fn a_reset_with_no_response_panics() {
        let mut reader = scripted(vec![reset()]);
        let _ = read_response(&mut reader).await;
    }

    /// A clean EOF needs no completeness check: the server closed deliberately,
    /// so whatever arrived is the whole response even without a declared
    /// framing.
    #[tokio::test]
    async fn a_clean_eof_is_accepted_without_a_framing_check() {
        let unframed: &'static str = "HTTP/1.1 204 No Content\r\nServer: x\r\n\r\n";
        let mut reader = scripted(vec![Ok(unframed)]);
        assert_eq!(read_response(&mut reader).await, unframed);
    }

    /// Any other error stays fatal, reset or not.
    #[tokio::test]
    #[should_panic(expected = "read failed after")]
    async fn a_non_reset_error_is_fatal() {
        let mut reader = scripted(vec![
            Ok(OK_HEAD),
            Err(Error::new(ErrorKind::BrokenPipe, "broken")),
        ]);
        let _ = read_response(&mut reader).await;
    }

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
