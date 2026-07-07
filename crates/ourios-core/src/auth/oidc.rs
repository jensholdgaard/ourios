//! The RFC 0029 OIDC verifier: local JWT verification against a cached
//! JWKS, resolving verified claims onto the RFC 0026 `(name, tenants)`
//! binding.
//!
//! Verification is **local** (§3.2): a signature check against cached
//! keys plus `iss`/`aud`/`exp`/`nbf` validation — the issuer is contacted
//! only at construction ([`OidcVerifier::discover`]), and on unseen-`kid`
//! misses (rotation, throttled by `REFRESH_MIN_INTERVAL`). Only the
//! RFC's asymmetric allow-list verifies (`ALLOWED_ALGORITHMS`);
//! `alg: none` and HMAC — including the public-key-as-HMAC-secret
//! downgrade — are rejected by construction because they are never in
//! the validation set.
//!
//! Every rejection collapses to the one undifferentiated
//! unauthenticated-shaped `None` at the
//! resolution layer (§3.2's no-oracle rule); nothing here renders a
//! token, a claims payload, or key material into any error or `Debug`
//! surface.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, KeyAlgorithm};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use tokio::sync::RwLock;

use super::{OidcConfig, TenantSet};

/// The §3.2 asymmetric allow-list. `alg: none` and every HMAC variant are
/// outside this set and therefore unverifiable, whatever a token's header
/// claims.
const ALLOWED_ALGORITHMS: [Algorithm; 8] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
    // ES512 is not in `jsonwebtoken`'s Algorithm set.
    Algorithm::ES384,
    // PS256/384/512 complete the RSA family.
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
];

/// Unseen-`kid` refresh throttle: a burst of unknown-`kid` tokens (a
/// probing client, or the window right after a rotation) collapses to one
/// JWKS fetch per interval rather than one per request.
const REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// One decode attempt against the current key cache. `UnknownKid` is the
/// only outcome that may trigger a (throttled) JWKS refetch — the §3.2
/// rotation path; everything else is a terminal rejection.
enum DecodeAttempt {
    Verified(serde_json::Map<String, serde_json::Value>),
    UnknownKid,
    Rejected,
}

/// The verified identity a token resolves to: the RFC 0026 binding shape,
/// derived from the configured claims' **values** (RFC 0029 §3.2).
#[derive(Debug, Clone)]
pub struct VerifiedIdentity {
    /// The `name_claim` value — the audit/metric label.
    pub name: String,
    /// The `tenant_claim` value, validated into the RFC 0026 set.
    pub tenants: TenantSet,
}

/// Why construction failed. Startup-only; request-path failures are the
/// undifferentiated `None`.
#[derive(Debug)]
pub struct DiscoveryError(String);

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for DiscoveryError {}

/// The subset of `/.well-known/openid-configuration` the verifier needs.
#[derive(serde::Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

/// One cached verification key.
struct CachedKey {
    key: DecodingKey,
    /// The algorithms this key may verify, pinned from the JWK itself
    /// (never from a presented token's header — the header only *selects*
    /// the key). A JWK carrying an explicit `alg` pins exactly that
    /// algorithm; one without pins its whole key *family* (issuers
    /// routinely omit `alg`, and an RSA key legitimately signs any RS*/PS*
    /// variant). Cross-family re-typing — the HMAC/EC downgrade shapes —
    /// stays impossible.
    allowed: Vec<Algorithm>,
}

/// The RFC 0029 §3.2 verifier: config + cached JWKS + the refresh path.
pub struct OidcVerifier {
    config: OidcConfig,
    http: reqwest::Client,
    jwks_uri: String,
    keys: RwLock<HashMap<String, CachedKey>>,
    last_refresh: RwLock<Instant>,
    refresh_min_interval: Duration,
}

impl std::fmt::Debug for OidcVerifier {
    /// Issuer and audience only — never key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcVerifier")
            .field("issuer", &self.config.issuer())
            .field("audience", &self.config.audience())
            .finish_non_exhaustive()
    }
}

impl OidcVerifier {
    /// Fetch the issuer's discovery document and initial JWKS (RFC 0029
    /// §3.2 — the startup contact). With no cached keys nothing could
    /// verify, so a failure here is a startup error, not a degraded mode.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError`] on an unreachable issuer, a discovery document
    /// whose `issuer` disagrees with the configured one (a misdirected or
    /// spoofed root would otherwise silently verify nothing), or an
    /// unusable JWKS.
    pub async fn discover(config: OidcConfig) -> Result<Self, DiscoveryError> {
        let http = reqwest::Client::new();
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            config.issuer().trim_end_matches('/')
        );
        let document: DiscoveryDocument = fetch_json(&http, &discovery_url).await?;
        if document.issuer.trim_end_matches('/') != config.issuer().trim_end_matches('/') {
            return Err(DiscoveryError(format!(
                "issuer mismatch: the discovery document names a different issuer \
                 than the configured {} (RFC 0029 §3.2)",
                config.issuer(),
            )));
        }
        let jwks: JwkSet = fetch_json(&http, &document.jwks_uri).await?;
        let keys = cache_keys(&jwks);
        if keys.is_empty() {
            return Err(DiscoveryError(
                "the issuer's JWKS holds no usable RS*/ES*/PS* keys with key ids \
                 (RFC 0029 §3.2 verifies the asymmetric allow-list only)"
                    .to_string(),
            ));
        }
        Ok(Self {
            config,
            http,
            jwks_uri: document.jwks_uri,
            keys: RwLock::new(keys),
            // Backdated one interval so the FIRST unseen-kid miss can
            // refresh immediately — the throttle bounds the gap *between*
            // refreshes, not the time to the first one (a rotation just
            // before startup must not eat a 401 window).
            last_refresh: RwLock::new(
                Instant::now()
                    .checked_sub(REFRESH_MIN_INTERVAL)
                    .unwrap_or_else(Instant::now),
            ),
            refresh_min_interval: REFRESH_MIN_INTERVAL,
        })
    }

    /// Verify a presented bearer as a JWT and resolve it to the RFC 0026
    /// binding shape (§3.2).
    ///
    /// `None` for every rejected shape — not-a-JWT, disallowed algorithm,
    /// unknown `kid` (after at most one throttled JWKS refresh), bad
    /// signature, `iss`/`aud`/`exp`/`nbf` failures, or claims that do not
    /// carry the configured tenant/name values. The caller maps `None`
    /// onto the one undifferentiated 401; no reason leaves this function.
    pub async fn verify(&self, token: &str) -> Option<VerifiedIdentity> {
        let header = decode_header(token).ok()?;
        if !ALLOWED_ALGORITHMS.contains(&header.alg) {
            return None;
        }
        let kid = header.kid?;

        let claims = match self.try_decode(token, &kid, header.alg).await {
            DecodeAttempt::Verified(claims) => claims,
            DecodeAttempt::UnknownKid => {
                // Unseen kid: refresh once, throttled, and retry — the §3.2
                // rotation path. Only a key miss refetches: any other
                // failure is terminal, because a refetch cannot make an
                // invalid token valid and invalid tokens must not be able
                // to drive issuer traffic.
                self.refresh_keys().await?;
                match self.try_decode(token, &kid, header.alg).await {
                    DecodeAttempt::Verified(claims) => claims,
                    _ => return None,
                }
            }
            DecodeAttempt::Rejected => return None,
        };
        self.resolve_identity(&claims)
    }

    /// Decode + validate against the currently cached key for `kid`,
    /// distinguishing the one outcome that may refetch (an unknown `kid`)
    /// from terminal rejections (alg/key family mismatch, any validation
    /// failure).
    async fn try_decode(&self, token: &str, kid: &str, token_alg: Algorithm) -> DecodeAttempt {
        let keys = self.keys.read().await;
        let Some(cached) = keys.get(kid) else {
            return DecodeAttempt::UnknownKid;
        };
        // The token's header must claim an algorithm the key itself
        // permits — a header is never allowed to re-type a key into
        // another family (the downgrade shape).
        if !cached.allowed.contains(&token_alg) {
            return DecodeAttempt::Rejected;
        }
        let mut validation = Validation::new(token_alg);
        validation.set_issuer(&[self.config.issuer()]);
        validation.set_audience(&[self.config.audience()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        // Off by default in jsonwebtoken; §3.2 validates `nbf` when the
        // token carries it (it stays optional — only `exp`/`iss`/`aud`
        // are required above).
        validation.validate_nbf = true;
        validation.leeway = self.config.clock_skew_secs();
        match decode::<serde_json::Map<String, serde_json::Value>>(token, &cached.key, &validation)
        {
            Ok(data) => DecodeAttempt::Verified(data.claims),
            Err(_) => DecodeAttempt::Rejected,
        }
    }

    /// Map the configured claims' values onto the binding shape. `None`
    /// when the tenant claim is missing/mistyped/invalid or the name claim
    /// is not a non-empty string — an identity the audit surface cannot
    /// label is not an identity.
    fn resolve_identity(
        &self,
        claims: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<VerifiedIdentity> {
        let name = claims.get(self.config.name_claim())?.as_str()?;
        if name.is_empty() {
            return None;
        }
        let tenants: Vec<String> = claims
            .get(self.config.tenant_claim())?
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Option<_>>()?;
        let tenants = validate_tenant_list(&tenants)?;
        Some(VerifiedIdentity {
            name: name.to_string(),
            tenants,
        })
    }

    /// Refetch the JWKS (rotation / unseen kid), throttled to one fetch
    /// per [`REFRESH_MIN_INTERVAL`]. `None` when throttled, when the fetch
    /// fails, or when the refetched set filters down to zero usable keys —
    /// the cached set stays authoritative in every one of those cases. A
    /// real rotation always publishes at least one usable key, so an empty
    /// result is treated as an issuer glitch (availability) rather than a
    /// total withdrawal; withdrawn keys still stop verifying on the next
    /// non-empty refresh.
    async fn refresh_keys(&self) -> Option<()> {
        {
            let last = self.last_refresh.read().await;
            if last.elapsed() < self.refresh_min_interval {
                return None;
            }
        }
        let mut last = self.last_refresh.write().await;
        // Re-check under the write lock: a concurrent refresher may have
        // just fetched, and one fetch per interval is the whole point.
        if last.elapsed() < self.refresh_min_interval {
            return None;
        }
        *last = Instant::now();
        let jwks: JwkSet = fetch_json(&self.http, &self.jwks_uri).await.ok()?;
        let fresh = cache_keys(&jwks);
        if fresh.is_empty() {
            return None;
        }
        *self.keys.write().await = fresh;
        Some(())
    }
}

/// GET + JSON-decode one document, with a bounded timeout.
async fn fetch_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
) -> Result<T, DiscoveryError> {
    let response = http
        .get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| DiscoveryError(format!("fetch {url}: {e}")))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| DiscoveryError(format!("read {url}: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| DiscoveryError(format!("decode {url}: {e}")))
}

/// Index a JWKS by `kid`, keeping only keys in the allow-list families.
/// Keys without a `kid` are skipped (the verifier selects by kid); an
/// individually malformed key is skipped rather than failing the set —
/// issuers routinely publish keys for algorithms a client does not use.
fn cache_keys(jwks: &JwkSet) -> HashMap<String, CachedKey> {
    let mut keys = HashMap::new();
    for jwk in &jwks.keys {
        let Some(kid) = jwk.common.key_id.clone() else {
            continue;
        };
        let allowed: Vec<Algorithm> = match &jwk.algorithm {
            AlgorithmParameters::RSA(_) => match jwk.common.key_algorithm {
                // Explicit `alg` pins exactly that algorithm.
                Some(KeyAlgorithm::RS256) => vec![Algorithm::RS256],
                Some(KeyAlgorithm::RS384) => vec![Algorithm::RS384],
                Some(KeyAlgorithm::RS512) => vec![Algorithm::RS512],
                Some(KeyAlgorithm::PS256) => vec![Algorithm::PS256],
                Some(KeyAlgorithm::PS384) => vec![Algorithm::PS384],
                Some(KeyAlgorithm::PS512) => vec![Algorithm::PS512],
                // No `alg`: the whole RSA family — issuers routinely omit
                // it, and rejecting their RS384/PS* tokens would be a
                // false negative, not a hardening.
                None => vec![
                    Algorithm::RS256,
                    Algorithm::RS384,
                    Algorithm::RS512,
                    Algorithm::PS256,
                    Algorithm::PS384,
                    Algorithm::PS512,
                ],
                // An RSA key claiming a non-RSA algorithm is malformed:
                // skip-don't-fail, like any other unusable key.
                Some(_) => continue,
            },
            // For EC the curve *is* the algorithm; a present `alg` must
            // agree with it or the key is malformed (skip-don't-fail).
            AlgorithmParameters::EllipticCurve(ec) => {
                let (derived, agreeing) = match ec.curve {
                    jsonwebtoken::jwk::EllipticCurve::P256 => {
                        (Algorithm::ES256, KeyAlgorithm::ES256)
                    }
                    jsonwebtoken::jwk::EllipticCurve::P384 => {
                        (Algorithm::ES384, KeyAlgorithm::ES384)
                    }
                    _ => continue,
                };
                match jwk.common.key_algorithm {
                    None => vec![derived],
                    Some(ka) if ka == agreeing => vec![derived],
                    Some(_) => continue,
                }
            }
            _ => continue,
        };
        let Ok(key) = DecodingKey::from_jwk(jwk) else {
            continue;
        };
        keys.insert(kid, CachedKey { key, allowed });
    }
    keys
}

/// Validate a tenant-claim value with the RFC 0026 §3.1 list rules (the
/// wildcard alone, else non-empty ids without surrounding whitespace).
fn validate_tenant_list(tenants: &[String]) -> Option<TenantSet> {
    if tenants.is_empty() {
        return None;
    }
    if tenants.iter().any(|t| t == "*") {
        if tenants.len() > 1 {
            return None;
        }
        return Some(TenantSet::All);
    }
    if tenants.iter().any(|t| t.is_empty() || t.trim() != t) {
        return None;
    }
    Some(TenantSet::Listed(tenants.iter().cloned().collect()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, RwLock as StdRwLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::Json;
    use axum::extract::State;
    use axum::routing::get;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::{EncodingKey, Header};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::EncodePrivateKey as _;
    use serde_json::{Value, json};

    use super::super::{OidcSpec, TenantSet, build_oidc_config};
    use super::OidcVerifier;

    /// One fixture signing key: the private half for minting, the public
    /// half as a JWK. Generated at test runtime — no committed key
    /// material.
    fn make_key(kid: &str) -> (EncodingKey, Value) {
        let signing = SigningKey::random(&mut rand::rngs::OsRng);
        let pem = signing
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .expect("pkcs8 pem");
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes()).expect("encoding key");
        let point = signing.verifying_key().to_encoded_point(false);
        let jwk = json!({
            "kty": "EC", "crv": "P-256", "use": "sig", "alg": "ES256", "kid": kid,
            "x": URL_SAFE_NO_PAD.encode(point.x().expect("x")),
            "y": URL_SAFE_NO_PAD.encode(point.y().expect("y")),
        });
        (encoding, jwk)
    }

    /// The RSA-family fixture key, generated once per test process (no
    /// committed private-key fixtures for scanners to flag — same policy
    /// as the P-256 fixture). 1024-bit keeps unoptimized keygen fast; the
    /// tests exercise signature *shape*, not strength. Its JWK
    /// deliberately omits `alg`: the shape issuers commonly publish,
    /// which must pin the whole RSA family rather than defaulting to
    /// RS256.
    fn make_rsa_key_without_alg(kid: &str) -> (EncodingKey, Value) {
        use rsa::traits::PublicKeyParts;
        static KEY: std::sync::OnceLock<(String, Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();
        let (pem, n, e) = KEY.get_or_init(|| {
            let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 1024).expect("rsa keygen");
            let pem = rsa::pkcs8::EncodePrivateKey::to_pkcs8_pem(&key, rsa::pkcs8::LineEnding::LF)
                .expect("pkcs8 pem")
                .to_string();
            (pem, key.n().to_bytes_be(), key.e().to_bytes_be())
        });
        let encoding = EncodingKey::from_rsa_pem(pem.as_bytes()).expect("rsa pem");
        let jwk = json!({
            "kty": "RSA", "use": "sig", "kid": kid,
            "n": URL_SAFE_NO_PAD.encode(n),
            "e": URL_SAFE_NO_PAD.encode(e),
        });
        (encoding, jwk)
    }

    /// A loopback issuer serving discovery + a swappable JWKS — the §6
    /// fixture-issuer tier (no container). Returns the issuer URL and a
    /// counter of `/jwks` fetches (construction is fetch #1), so tests can
    /// assert exactly when the verifier goes back to the issuer.
    async fn serve_issuer(jwks: Arc<StdRwLock<Value>>) -> (String, Arc<AtomicUsize>) {
        serve_issuer_claiming(None, jwks).await
    }

    /// The fixture, optionally publishing a discovery document that names
    /// a *different* issuer than the one it serves under (the §3.2
    /// mismatch arm).
    async fn serve_issuer_claiming(
        claimed: Option<&str>,
        jwks: Arc<StdRwLock<Value>>,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture issuer");
        let issuer = format!("http://{}", listener.local_addr().expect("addr"));
        let discovery = json!({
            "issuer": claimed.unwrap_or(&issuer),
            "jwks_uri": format!("{issuer}/jwks"),
        });
        let jwks_hits = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(|State((discovery, _, _)): DiscState| async move { Json(discovery) }),
            )
            .route(
                "/jwks",
                get(|State((_, jwks, hits)): DiscState| async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let current = jwks.read().expect("jwks lock").clone();
                    Json(current)
                }),
            )
            .with_state((discovery, jwks, Arc::clone(&jwks_hits)));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve issuer");
        });
        (issuer, jwks_hits)
    }

    type DiscState = State<(Value, Arc<StdRwLock<Value>>, Arc<AtomicUsize>)>;

    fn now_secs() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("epoch")
                .as_secs(),
        )
        .expect("fits")
    }

    /// Standard well-formed claims for the fixture audience.
    fn claims(issuer: &str) -> Value {
        json!({
            "iss": issuer, "aud": "ourios", "exp": now_secs() + 600,
            "sub": "edge-collector", "ourios_tenants": ["acme", "globex"],
        })
    }

    fn mint(encoding: &EncodingKey, kid: &str, claims: &Value) -> String {
        let mut header = Header::new(super::Algorithm::ES256);
        header.kid = Some(kid.to_string());
        jsonwebtoken::encode(&header, claims, encoding).expect("mint")
    }

    fn config_for(issuer: &str) -> super::OidcConfig {
        build_oidc_config(&OidcSpec {
            issuer: Some(issuer.to_string()),
            audience: Some("ourios".to_string()),
            tenant_claim: Some("ourios_tenants".to_string()),
            name_claim: None,
            clock_skew_secs: Some("0".to_string()),
        })
        .expect("valid config")
    }

    /// Scenario RFC0029.2 — the verification matrix against the fixture
    /// issuer: one valid acceptance, and every rejected shape collapsing
    /// to `None` (the caller's one undifferentiated 401).
    /// See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[tokio::test]
    async fn rfc0029_2_verification_matrix() {
        let (encoding, jwk) = make_key("key-1");
        let jwks = Arc::new(StdRwLock::new(json!({ "keys": [jwk] })));
        let (issuer, jwks_hits) = serve_issuer(jwks).await;
        let verifier = OidcVerifier::discover(config_for(&issuer))
            .await
            .expect("discover");

        // (a) valid: accepted, claims' values resolve the binding.
        let identity = verifier
            .verify(&mint(&encoding, "key-1", &claims(&issuer)))
            .await
            .expect("valid token verifies");
        assert_eq!(identity.name, "edge-collector");
        assert!(identity.tenants.allows("acme"));
        assert!(!identity.tenants.allows("initech"));

        // (b)–(e) claim failures: expired, nbf in the future (beyond the
        // zero configured skew), wrong audience, wrong issuer.
        let mut expired = claims(&issuer);
        expired["exp"] = json!(now_secs() - 30);
        let mut premature = claims(&issuer);
        premature["nbf"] = json!(now_secs() + 300);
        let mut wrong_aud = claims(&issuer);
        wrong_aud["aud"] = json!("someone-else");
        let mut wrong_iss = claims(&issuer);
        wrong_iss["iss"] = json!("https://evil.example");
        for (arm, bad) in [
            ("expired", &expired),
            ("nbf beyond skew", &premature),
            ("wrong aud", &wrong_aud),
            ("wrong iss", &wrong_iss),
        ] {
            assert!(
                verifier
                    .verify(&mint(&encoding, "key-1", bad))
                    .await
                    .is_none(),
                "{arm} must be rejected"
            );
        }

        // (f) corrupted signature.
        let valid = mint(&encoding, "key-1", &claims(&issuer));
        let corrupted = format!("{}AAAA", &valid[..valid.len() - 4]);
        assert!(verifier.verify(&corrupted).await.is_none(), "bad signature");

        // (g)/(h) algorithm shapes outside the asymmetric allow-list:
        // `alg: none` and an HMAC header over the same kid (the
        // public-key-as-secret downgrade). Hand-encoded — the point is the
        // header, not the signature, and rejection must precede any
        // signature work.
        let payload = URL_SAFE_NO_PAD.encode(claims(&issuer).to_string());
        for (arm, header) in [
            ("alg none", r#"{"alg":"none","kid":"key-1"}"#),
            (
                "hmac downgrade",
                r#"{"alg":"HS256","typ":"JWT","kid":"key-1"}"#,
            ),
        ] {
            let crafted = format!(
                "{}.{payload}.{}",
                URL_SAFE_NO_PAD.encode(header),
                URL_SAFE_NO_PAD.encode("forged")
            );
            assert!(verifier.verify(&crafted).await.is_none(), "{arm}");
        }

        // (i) not a JWT at all; and a JWT with no kid never matches.
        assert!(verifier.verify("tok-static-shaped").await.is_none());
        let no_kid = jsonwebtoken::encode(
            &Header::new(super::Algorithm::ES256),
            &claims(&issuer),
            &encoding,
        )
        .expect("mint");
        assert!(verifier.verify(&no_kid).await.is_none(), "kid required");

        // Claim-shape failures: a tenant claim that is not a string list,
        // an empty list, a whitespace-broken id, and a missing name claim.
        for (arm, mutate) in [
            ("tenant claim not a list", json!("acme")),
            ("empty tenant list", json!([])),
            ("whitespace tenant", json!([" acme"])),
            ("wildcard plus id", json!(["*", "acme"])),
        ] {
            let mut bad = claims(&issuer);
            bad["ourios_tenants"] = mutate;
            assert!(
                verifier
                    .verify(&mint(&encoding, "key-1", &bad))
                    .await
                    .is_none(),
                "{arm}"
            );
        }
        let mut no_name = claims(&issuer);
        no_name.as_object_mut().expect("map").remove("sub");
        assert!(
            verifier
                .verify(&mint(&encoding, "key-1", &no_name))
                .await
                .is_none(),
            "an identity the audit surface cannot label is not an identity"
        );

        // None of the rejections above may drive issuer traffic: every arm
        // used a cached kid (or died before key lookup), so the only JWKS
        // fetch on record is construction's. Only an unknown kid refetches
        // (the rotation test's territory).
        assert_eq!(
            jwks_hits.load(Ordering::SeqCst),
            1,
            "invalid tokens must not trigger JWKS refetches"
        );

        // Wildcard resolves to the RFC 0026 all-tenants set.
        let mut wildcard = claims(&issuer);
        wildcard["ourios_tenants"] = json!(["*"]);
        let identity = verifier
            .verify(&mint(&encoding, "key-1", &wildcard))
            .await
            .expect("wildcard verifies");
        assert_eq!(identity.tenants, TenantSet::All);
    }

    /// Scenario RFC0029.6 — JWKS rotation: an unseen `kid` triggers a
    /// refetch and the new key's token verifies without restart; the
    /// withdrawn key's tokens are rejected once the refreshed set drops
    /// it. See `docs/rfcs/0029-oidc-bearer-layer.md` §5.
    #[tokio::test]
    async fn rfc0029_6_jwks_rotation() {
        let (old_encoding, old_jwk) = make_key("key-old");
        let jwks = Arc::new(StdRwLock::new(json!({ "keys": [old_jwk] })));
        let (issuer, _) = serve_issuer(Arc::clone(&jwks)).await;
        let verifier = OidcVerifier::discover(config_for(&issuer))
            .await
            .expect("discover");
        // No throttle injection: `last_refresh` is backdated at
        // construction, so the FIRST unseen-kid miss refreshes under the
        // real production interval — this test proves that. (The later
        // withdrawn-key arm is then throttled, and rejects via the
        // unknown-kid path without a refetch, which is also correct.)

        let old_token = mint(&old_encoding, "key-old", &claims(&issuer));
        assert!(
            verifier.verify(&old_token).await.is_some(),
            "pre-rotation token verifies against the startup JWKS"
        );

        // Rotate: the issuer withdraws key-old and publishes key-new.
        let (new_encoding, new_jwk) = make_key("key-new");
        *jwks.write().expect("jwks lock") = json!({ "keys": [new_jwk] });

        let new_token = mint(&new_encoding, "key-new", &claims(&issuer));
        assert!(
            verifier.verify(&new_token).await.is_some(),
            "unseen kid refetches the JWKS and verifies without restart"
        );
        assert!(
            verifier.verify(&old_token).await.is_none(),
            "the withdrawn key's tokens are rejected once the set drops it"
        );
    }

    /// A JWK published without `alg` (the common issuer shape) pins its
    /// key *family*: RS384/RS512/PS* tokens verify against the same RSA
    /// key, while a cross-family header (the ES/HMAC re-typing shapes)
    /// still rejects.
    #[tokio::test]
    async fn jwk_without_alg_accepts_its_family_only() {
        let (rsa_encoding, rsa_jwk) = make_rsa_key_without_alg("key-rsa");
        let jwks = Arc::new(StdRwLock::new(json!({ "keys": [rsa_jwk] })));
        let (issuer, _) = serve_issuer(jwks).await;
        let verifier = OidcVerifier::discover(config_for(&issuer))
            .await
            .expect("discover");

        for alg in [
            super::Algorithm::RS256,
            super::Algorithm::RS384,
            super::Algorithm::RS512,
        ] {
            let mut header = Header::new(alg);
            header.kid = Some("key-rsa".to_string());
            let token =
                jsonwebtoken::encode(&header, &claims(&issuer), &rsa_encoding).expect("mint rsa");
            assert!(
                verifier.verify(&token).await.is_some(),
                "{alg:?} verifies against the alg-less RSA JWK"
            );
        }

        // Cross-family: an ES256 header selecting the RSA key must reject
        // before any signature work (family pin, not just bad-signature).
        let (ec_encoding, _) = make_key("unpublished");
        let mut header = Header::new(super::Algorithm::ES256);
        header.kid = Some("key-rsa".to_string());
        let cross = jsonwebtoken::encode(&header, &claims(&issuer), &ec_encoding).expect("mint");
        assert!(
            verifier.verify(&cross).await.is_none(),
            "a header must not re-type an RSA key into the EC family"
        );
    }

    /// A refresh that fetches an *empty* usable-key set retains the
    /// cached keys (issuer glitch ≠ total withdrawal): verification of
    /// already-cached kids survives, and the withdrawn-key semantics
    /// resume on the next non-empty refresh.
    #[tokio::test]
    async fn empty_jwks_refresh_retains_the_cached_set() {
        let (encoding, jwk) = make_key("key-1");
        let jwks = Arc::new(StdRwLock::new(json!({ "keys": [jwk] })));
        let (issuer, jwks_hits) = serve_issuer(Arc::clone(&jwks)).await;
        let verifier = OidcVerifier::discover(config_for(&issuer))
            .await
            .expect("discover");

        // The issuer glitches to an empty set; an unseen kid triggers the
        // (backdated-allowed) refresh, which must not wipe the cache.
        *jwks.write().expect("jwks lock") = json!({ "keys": [] });
        let ghost = mint(&encoding, "key-ghost", &claims(&issuer));
        assert!(verifier.verify(&ghost).await.is_none(), "unknown kid");
        assert_eq!(
            jwks_hits.load(Ordering::SeqCst),
            2,
            "the unseen kid did refetch"
        );

        let cached = mint(&encoding, "key-1", &claims(&issuer));
        assert!(
            verifier.verify(&cached).await.is_some(),
            "the cached key survives an empty-JWKS refresh"
        );
    }

    /// An EC JWK whose explicit `alg` disagrees with its curve is
    /// malformed and skipped (the curve is authoritative); an agreeing
    /// `alg` caches normally.
    #[test]
    fn ec_jwk_with_disagreeing_alg_is_skipped() {
        let (_, mut jwk) = make_key("key-ec");
        jwk["alg"] = json!("ES384"); // P-256 curve claiming ES384: malformed
        let set: super::JwkSet = serde_json::from_value(json!({ "keys": [jwk] })).expect("jwk set");
        assert!(
            super::cache_keys(&set).is_empty(),
            "curve/alg disagreement must skip the key"
        );

        let (_, agreeing) = make_key("key-ec2"); // fixture sets alg: ES256
        let set: super::JwkSet =
            serde_json::from_value(json!({ "keys": [agreeing] })).expect("jwk set");
        assert_eq!(super::cache_keys(&set).len(), 1, "agreeing alg caches");
    }

    /// Startup arms: a discovery document naming a different issuer and a
    /// JWKS with no usable keys are construction errors, not degraded
    /// modes (§3.2 — with no cached keys nothing could verify).
    #[tokio::test]
    async fn discovery_rejects_issuer_mismatch_and_unusable_jwks() {
        let (_, jwk) = make_key("key-1");
        let jwks = Arc::new(StdRwLock::new(json!({ "keys": [jwk] })));
        let (issuer, _) = serve_issuer(jwks).await;

        // A just-closed loopback port: immediate connection-refused, no
        // DNS or egress dependency (CI runs with restricted networks).
        let unreachable = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            format!("http://{}", l.local_addr().expect("addr"))
        };
        let mut config = OidcSpec {
            issuer: Some(unreachable),
            audience: Some("ourios".to_string()),
            tenant_claim: Some("ourios_tenants".to_string()),
            name_claim: None,
            clock_skew_secs: None,
        };
        let err = OidcVerifier::discover(build_oidc_config(&config).expect("valid"))
            .await
            .expect_err("unreachable issuer fails startup");
        assert!(err.to_string().contains("fetch"), "{err}");

        config.issuer = Some(issuer.clone());
        let empty = Arc::new(StdRwLock::new(json!({ "keys": [] })));
        let (empty_issuer, _) = serve_issuer(empty).await;
        config.issuer = Some(empty_issuer);
        let err = OidcVerifier::discover(build_oidc_config(&config).expect("valid"))
            .await
            .expect_err("an empty JWKS fails startup");
        assert!(err.to_string().contains("JWKS"), "{err}");

        // Issuer mismatch: the fixture serves under its own address but
        // publishes a document claiming a different issuer — a misdirected
        // or spoofed discovery root fails construction, never silently
        // verifies.
        let (_, jwk) = make_key("key-1");
        let lying = Arc::new(StdRwLock::new(json!({ "keys": [jwk] })));
        let (lying_issuer, _) =
            serve_issuer_claiming(Some("https://somewhere-else.example"), lying).await;
        config.issuer = Some(lying_issuer);
        let err = OidcVerifier::discover(build_oidc_config(&config).expect("valid"))
            .await
            .expect_err("a disagreeing discovery document fails startup");
        assert!(err.to_string().contains("issuer mismatch"), "{err}");

        // And the agreeing fixture constructs.
        config.issuer = Some(issuer);
        assert!(
            OidcVerifier::discover(build_oidc_config(&config).expect("valid"))
                .await
                .is_ok()
        );
    }
}
