//! RFC0003.13 — Compression over HTTP: identity and gzip MUST be supported.
//! See `docs/rfcs/0003-otlp-receiver.md` §5.
//!
//! Red gate (`specified → red`): `#[ignore]`'d until the receiver
//! lands.

/// Scenario RFC0003.13 — Compression over HTTP: identity and gzip MUST be supported.
#[ignore = "RFC 0003 red gate — implementation pending (RFC0003.13)"]
#[test]
fn rfc0003_13_identity_and_gzip_decode_equally_unsupported_is_415() {
    unimplemented!(
        "RFC0003.13 — the same payload sent with Content-Encoding: identity (or \
         absent) and with Content-Encoding: gzip yields equal OtlpLogRecord \
         sequences (both encodings are an OTLP MUST). An unsupported encoding \
         (zstd, br) is rejected with HTTP 415; zstd is deferred per §9."
    );
}
