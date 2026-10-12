//! The S3 / S3-compatible backend's client (see [`Store::s3`](super::Store::s3)).

use object_store::ClientConfigKey;
use object_store::aws::{AmazonS3, AmazonS3Builder, AmazonS3ConfigKey};
use object_store::client::SpawnedReqwestConnector;

use super::bridge::bridge_runtime;
use super::client_span::{S3Target, Tracing};
use super::metrics::Metrics;
use super::transport::MeteredConnector;
use super::{S3Config, StoreError};

/// Idle connections the S3 client keeps per host once a burst of concurrent
/// requests is over; the rest close instead of holding a descriptor each.
/// When credentials come from the chain, an `AWS_POOL_MAX_IDLE_PER_HOST` in
/// the environment takes precedence (explicit credentials read no `AWS_*`).
const MAX_IDLE_CONNECTIONS_PER_HOST: usize = 32;

/// The explicit credential fields of an [`S3Config`].
struct Credentials {
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
}

/// The client for `cfg`, and its key prefix.
pub(super) fn client(
    cfg: S3Config,
    metrics: &Metrics,
    tracing: Tracing,
) -> Result<(AmazonS3, Option<String>), StoreError> {
    let S3Config {
        bucket,
        endpoint,
        region,
        prefix,
        access_key_id,
        secret_access_key,
        session_token,
    } = cfg;
    let target = S3Target {
        bucket: bucket.trim().to_owned(),
        region: region.clone(),
        endpoint_host: endpoint.as_deref().map(endpoint_host),
        path_prefix: endpoint.as_deref().map(endpoint_path).unwrap_or_default(),
        tracing,
    };
    let builder = authenticated(
        &bucket,
        Credentials {
            access_key_id,
            secret_access_key,
            session_token,
        },
    )?;
    // reqwest runs each pooled connection on the runtime that issued the
    // request, and a connection only notices the store closing it while
    // that runtime polls it. Callers' runtimes stall (a current-thread
    // caller between requests, busy `DataFusion` workers), which left
    // connections in `CLOSE_WAIT` and handed them to later requests
    // (#791). The bridge runtime is never a caller's, so it always does.
    let s3 = addressed(builder, endpoint, region)
        .with_http_connector(MeteredConnector {
            inner: SpawnedReqwestConnector::new(bridge_runtime()?.handle().clone()),
            metrics: metrics.clone(),
            target,
        })
        .build()
        .map_err(StoreError::Backend)?;
    Ok((s3, prefix))
}

/// An endpoint override's authority and path: what follows the scheme,
/// split at the first `/`.
fn endpoint_parts(endpoint: &str) -> (&str, &str) {
    let after_scheme = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    after_scheme
        .find('/')
        .map_or((after_scheme, ""), |slash| after_scheme.split_at(slash))
}

/// The host of an endpoint override, as a request URI reports it: no port,
/// an IPv6 literal kept in brackets.
fn endpoint_host(endpoint: &str) -> String {
    let (authority, _) = endpoint_parts(endpoint);
    match authority.strip_prefix('[') {
        Some(v6) => format!("[{}]", v6.split(']').next().unwrap_or_default()),
        None => authority.split(':').next().unwrap_or_default().to_owned(),
    }
}

/// The path an endpoint override sits below (`/prefix`), without a trailing
/// slash; empty for one at the host root.
fn endpoint_path(endpoint: &str) -> String {
    endpoint_parts(endpoint).1.trim_end_matches('/').to_owned()
}

/// A builder for `bucket` carrying the credentials, explicit or from the
/// chain.
fn authenticated(bucket: &str, credentials: Credentials) -> Result<AmazonS3Builder, StoreError> {
    // Trim once and use the trimmed value for both the check and the
    // builder, so a whitespace-padded bucket can't pass validation and then
    // fail opaquely at request time.
    let bucket = bucket.trim();
    if bucket.is_empty() {
        return Err(StoreError::Config(
            "S3 bucket name must not be empty".to_string(),
        ));
    }
    // Normalize the explicit credential fields: trim and treat an empty or
    // whitespace-only value as unset, so a blank reads consistently across
    // every caller of `S3Config` (matching the server's env parsing) and
    // can't spuriously trip the partial-set check below (RFC 0019 §3.4).
    let normalize = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let access_key_id = normalize(credentials.access_key_id);
    let secret_access_key = normalize(credentials.secret_access_key);
    let session_token = normalize(credentials.session_token);
    reject_partial(
        access_key_id.is_some(),
        secret_access_key.is_some(),
        session_token.is_some(),
    )?;
    // Explicit credentials build from a **clean** builder so no ambient
    // chain credential (e.g. an `AWS_SESSION_TOKEN` `from_env` would pick up)
    // bleeds into the explicit static-key pair — the `with_*` setters do not
    // clear an inherited token. `from_env()` (the standard chain: static
    // `AWS_*`, shared profile, IRSA, instance metadata) is used only as the
    // fallback when no explicit pair is given (RFC 0019 §3.4).
    Ok(match (access_key_id, secret_access_key) {
        (Some(access_key_id), Some(secret_access_key)) => {
            let explicit = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_access_key_id(access_key_id)
                .with_secret_access_key(secret_access_key);
            match session_token {
                Some(session_token) => explicit.with_token(session_token),
                None => explicit,
            }
        }
        // `reject_partial` guarantees the remaining case is "no explicit
        // credentials" (a lone key, lone secret, or lone token is rejected).
        _ => AmazonS3Builder::from_env().with_bucket_name(bucket),
    })
}

/// The static access key and its secret are a pair; a session token is
/// meaningless without them. Reject a partial set rather than silently
/// falling back to the chain on an operator typo. The message names only
/// the offending key, never a value (RFC 0019 §3.4).
fn reject_partial(key: bool, secret: bool, token: bool) -> Result<(), StoreError> {
    let missing = match (key, secret, token) {
        (true, false, _) => {
            "OURIOS_S3_SECRET_ACCESS_KEY must be set (the static access key and secret access key are required together)"
        }
        (false, true, _) => {
            "OURIOS_S3_ACCESS_KEY_ID must be set (the static access key and secret access key are required together)"
        }
        (false, false, true) => {
            "OURIOS_S3_SESSION_TOKEN requires the static access key and secret access key (a session token is valid only with the static key pair)"
        }
        _ => return Ok(()),
    };
    Err(StoreError::Config(missing.to_string()))
}

/// `builder` pointed at `endpoint` and `region`, with the idle-pool cap.
fn addressed(
    mut builder: AmazonS3Builder,
    endpoint: Option<String>,
    region: Option<String>,
) -> AmazonS3Builder {
    if let Some(endpoint) = endpoint {
        // S3-compatible dev endpoints are often plain HTTP; object_store
        // refuses HTTP unless explicitly allowed.
        let allow_http = endpoint.starts_with("http://");
        builder = builder.with_endpoint(endpoint);
        if allow_http {
            builder = builder.with_allow_http(true);
        }
    }
    if let Some(region) = region {
        builder = builder.with_region(region);
    }
    let pool_cap = AmazonS3ConfigKey::Client(ClientConfigKey::PoolMaxIdlePerHost);
    if builder.get_config_value(&pool_cap).is_none() {
        builder = builder.with_config(pool_cap, MAX_IDLE_CONNECTIONS_PER_HOST.to_string());
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::{endpoint_host, endpoint_path};

    #[test]
    fn the_endpoint_path_is_what_sits_below_the_host() {
        assert_eq!(endpoint_path("http://h:9000"), "");
        assert_eq!(endpoint_path("http://h:9000/"), "");
        assert_eq!(endpoint_path("https://h/s3/"), "/s3");
        assert_eq!(endpoint_path("https://h/a/b"), "/a/b");
    }

    #[test]
    fn the_endpoint_host_drops_the_port_and_keeps_ipv6_brackets() {
        assert_eq!(endpoint_host("http://127.0.0.1:9000/s3"), "127.0.0.1");
        assert_eq!(endpoint_host("https://minio.example"), "minio.example");
        assert_eq!(endpoint_host("http://[::1]:9000"), "[::1]");
    }
}
