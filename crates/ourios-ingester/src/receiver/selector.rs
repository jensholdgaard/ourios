//! The out-of-band tenant selector (RFC 0046 §3.1): `X-Ourios-Tenant` on
//! OTLP/HTTP, `x-ourios-tenant` metadata on OTLP/gRPC — required on every
//! export, exactly one occurrence, normalised once into the `TenantId` that
//! authorization, the WAL frame, storage and queries all use.

use ourios_core::tenant::{MAX_TENANT_BYTES, TenantId, TenantIdError};

/// The header / metadata key (lower-case; HTTP header names are
/// case-insensitive, gRPC metadata keys are lower-case by construction).
pub const TENANT_HEADER: &str = "x-ourios-tenant";

/// The RFC 0048 §3.1 upper bound on a normalised selector, in bytes — the
/// tenant grammar's bound, re-exported for the receiver's error surface.
pub const MAX_SELECTOR_BYTES: usize = MAX_TENANT_BYTES;

/// Why a request's tenant selector was refused — always a client error
/// (`400` / `INVALID_ARGUMENT`), decided before authorization and before
/// any WAL work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SelectorError {
    /// No selector on the request.
    Missing,
    /// More than one selector on the request (equal values included).
    Repeated { count: usize },
    /// The raw bytes are not valid UTF-8 (HTTP) or not visible ASCII (gRPC).
    NotText,
    /// Empty after trimming ASCII whitespace.
    Empty,
    /// Longer than [`MAX_SELECTOR_BYTES`] after trimming.
    TooLong { found: usize },
    /// A character outside the RFC 0048 §3.1 tenant grammar (ASCII
    /// graphic excluding `:`, `#` and `/`).
    OutsideGrammar { found: char },
}

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "the {TENANT_HEADER} header is required: it names the tenant this export lands in (RFC 0046 §3.1)"
            ),
            Self::Repeated { count } => write!(
                f,
                "the {TENANT_HEADER} header must appear exactly once, found {count} occurrences"
            ),
            Self::NotText => write!(f, "the {TENANT_HEADER} value must be valid text"),
            Self::Empty => write!(f, "the {TENANT_HEADER} value must be non-empty"),
            Self::TooLong { found } => write!(
                f,
                "the {TENANT_HEADER} value is {found} bytes; the maximum is {MAX_SELECTOR_BYTES}"
            ),
            Self::OutsideGrammar { found } => write!(
                f,
                "the {TENANT_HEADER} value must be ASCII graphic characters excluding \
                 ':', '#' and '/' (RFC 0048); found {found:?}"
            ),
        }
    }
}

impl std::error::Error for SelectorError {}

/// Normalise one raw selector value: trim ASCII whitespace, then apply
/// the RFC 0048 §3.1 tenant grammar.
///
/// # Errors
///
/// [`SelectorError`] naming the first failed rule.
pub fn normalise(raw: &str) -> Result<TenantId, SelectorError> {
    let value = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    TenantId::try_new(value).map_err(|e| match e {
        TenantIdError::Empty => SelectorError::Empty,
        TenantIdError::TooLong { found } => SelectorError::TooLong { found },
        TenantIdError::InvalidCharacter { found } => SelectorError::OutsideGrammar { found },
    })
}

/// Resolve the selector from every occurrence of the header/metadata key
/// on a request: exactly one occurrence, whose bytes decode as text via
/// `to_text`, normalised by [`normalise`].
///
/// # Errors
///
/// [`SelectorError`] — `Missing`, `Repeated`, `NotText`, or a
/// normalisation failure.
pub fn resolve<'a, I, V>(
    occurrences: I,
    to_text: impl Fn(&'a V) -> Option<&'a str>,
) -> Result<TenantId, SelectorError>
where
    I: IntoIterator<Item = &'a V>,
    V: 'a + ?Sized,
{
    let mut iter = occurrences.into_iter();
    let Some(first) = iter.next() else {
        return Err(SelectorError::Missing);
    };
    let extra = iter.count();
    if extra > 0 {
        return Err(SelectorError::Repeated { count: extra + 1 });
    }
    let raw = to_text(first).ok_or(SelectorError::NotText)?;
    normalise(raw)
}

/// The selector on an OTLP/HTTP request (RFC 0046 §3.1): all
/// `X-Ourios-Tenant` header values, UTF-8 required.
///
/// # Errors
///
/// See [`resolve`].
pub fn from_headers(headers: &axum::http::HeaderMap) -> Result<TenantId, SelectorError> {
    let values: Vec<&axum::http::HeaderValue> = headers.get_all(TENANT_HEADER).iter().collect();
    resolve(values.iter().copied(), |v| {
        std::str::from_utf8(v.as_bytes()).ok()
    })
}

/// The selector on an OTLP/gRPC request (RFC 0046 §3.1): all
/// `x-ourios-tenant` ASCII metadata entries. The RFC 0046 "non-ASCII over
/// HTTP but not gRPC" caveat is gone — the RFC 0048 grammar is ASCII on
/// every transport.
///
/// # Errors
///
/// See [`resolve`].
pub fn from_metadata(metadata: &tonic::metadata::MetadataMap) -> Result<TenantId, SelectorError> {
    let values: Vec<&tonic::metadata::MetadataValue<tonic::metadata::Ascii>> =
        metadata.get_all(TENANT_HEADER).iter().collect();
    resolve(values.iter().copied(), |v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use tonic::metadata::MetadataMap;

    // RFC0046.7 normalisation, under the RFC0048.1 grammar.
    #[test]
    fn normalise_trims_and_applies_the_grammar() {
        assert_eq!(normalise(" acme ").expect("trimmed").as_str(), "acme");
        assert_eq!(normalise("100%").expect("graphic ascii").as_str(), "100%");
        assert_eq!(normalise("   ").unwrap_err(), SelectorError::Empty);
        assert_eq!(normalise("").unwrap_err(), SelectorError::Empty);
        assert!(matches!(
            normalise(&"x".repeat(129)).unwrap_err(),
            SelectorError::TooLong { found: 129 }
        ));
        assert!(normalise(&"x".repeat(128)).is_ok());
        for (bad, found) in [
            ("a/b", '/'),
            ("a:b", ':'),
            ("a#b", '#'),
            ("a b", ' '),
            ("a\tb", '\t'),
            ("é-tenant", 'é'),
            ("a\u{7f}b", '\u{7f}'),
        ] {
            assert_eq!(
                normalise(bad).unwrap_err(),
                SelectorError::OutsideGrammar { found },
                "{bad:?}"
            );
        }
    }

    // RFC0046.1/.7 — HTTP: missing, single, repeated (equal values too).
    #[test]
    fn http_selector_missing_single_repeated() {
        let mut h = HeaderMap::new();
        assert_eq!(from_headers(&h).unwrap_err(), SelectorError::Missing);
        h.append("X-Ourios-Tenant", HeaderValue::from_static("acme"));
        assert_eq!(from_headers(&h).expect("one").as_str(), "acme");
        h.append("x-ourios-tenant", HeaderValue::from_static("acme"));
        assert_eq!(
            from_headers(&h).unwrap_err(),
            SelectorError::Repeated { count: 2 }
        );
        let mut bad = HeaderMap::new();
        bad.append(
            "x-ourios-tenant",
            HeaderValue::from_bytes(&[0xFF, 0xFE]).expect("opaque bytes"),
        );
        assert_eq!(from_headers(&bad).unwrap_err(), SelectorError::NotText);
    }

    // RFC0046.1/.7 — gRPC metadata: same rules; ASCII only.
    #[test]
    fn grpc_selector_missing_single_repeated() {
        let mut m = MetadataMap::new();
        assert_eq!(from_metadata(&m).unwrap_err(), SelectorError::Missing);
        m.append("x-ourios-tenant", "acme".parse().expect("ascii"));
        assert_eq!(from_metadata(&m).expect("one").as_str(), "acme");
        m.append("x-ourios-tenant", "globex".parse().expect("ascii"));
        assert_eq!(
            from_metadata(&m).unwrap_err(),
            SelectorError::Repeated { count: 2 }
        );
        // A non-ASCII value: HeaderValue admits the raw bytes (obs-text)
        // but they are not visible ASCII, so the selector is refused —
        // the RFC 0046 §3.1 gRPC caveat.
        let mut non_ascii = MetadataMap::new();
        non_ascii.append("x-ourios-tenant", "é".parse().expect("obs-text bytes"));
        assert_eq!(
            from_metadata(&non_ascii).unwrap_err(),
            SelectorError::NotText
        );
    }
}
