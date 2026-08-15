//! HTTP JWKS fetch + cache for the admin IdP (contract rule 2, offline
//! verification).
//!
//! Properties worth keeping in mind when reading it:
//!
//! - **TTL comes from the response.** `Cache-Control: max-age` on the JWKS
//!   response wins over the built-in default, so the IdP can shorten its
//!   own rotation window without a redeploy here. A response without one
//!   falls back to [`AdminJwksClientConfig::default_ttl`], and an absurd
//!   value is clamped to [`AdminJwksClientConfig::max_ttl`].
//! - **There is a refetch floor.** A service's admin surface is reachable
//!   by anyone who can set an `Authorization` header, so a burst of tokens
//!   carrying a garbage `kid` must not become a burst of requests at the
//!   IdP. Within the floor window an unknown `kid` fails closed with no
//!   network call.
//! - `RwLock` with double-checked locking, and the write lock held across
//!   the fetch, so a burst of concurrent misses collapses into one round
//!   trip.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::error::{GuardError, GuardResult};

/// Used when the JWKS response carries no `Cache-Control: max-age`.
pub const DEFAULT_JWKS_TTL: Duration = Duration::from_secs(300);

/// Ceiling on a server-supplied TTL. A key set cached for a day would
/// outlive a key rotation and reject every token minted after it.
pub const MAX_JWKS_TTL: Duration = Duration::from_secs(3600);

pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Floor between unknown-`kid`-triggered refetch attempts.
pub const DEFAULT_MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(5);

/// Resolves an admin-IdP signing key by the JWT header's `kid`.
///
/// Separated from the verifier so key *transport* (HTTP + cache) and key
/// *use* (signature check) can be tested apart, and so a host that already
/// has its own JWKS caching can plug it in instead of [`AdminJwksClient`].
#[async_trait]
pub trait JwksKeyProvider: Send + Sync {
    async fn resolve_key(&self, kid: &str) -> GuardResult<VerifyingKey>;
}

#[derive(Clone, Debug)]
pub struct AdminJwksClientConfig {
    pub jwks_url: String,
    pub http_timeout: Duration,
    /// Fallback TTL. A `Cache-Control: max-age` on the response overrides it.
    pub default_ttl: Duration,
    pub max_ttl: Duration,
    pub min_refetch_interval: Duration,
}

impl AdminJwksClientConfig {
    /// Build from the one value that varies per deployment. The rest are
    /// tuned to the IdP's own contract and rarely need overriding.
    pub fn new(jwks_url: impl Into<String>) -> Self {
        Self {
            jwks_url: jwks_url.into(),
            http_timeout: DEFAULT_HTTP_TIMEOUT,
            default_ttl: DEFAULT_JWKS_TTL,
            max_ttl: MAX_JWKS_TTL,
            min_refetch_interval: DEFAULT_MIN_REFETCH_INTERVAL,
        }
    }
}

/// One entry in the JWKS array. Every field is optional at the deserialize
/// level so a heterogeneous key set (say, an RSA key with `n`/`e` instead of
/// `crv`/`x`) does not fail the whole response — unusable entries are
/// skipped in the fetch loop instead.
#[derive(Deserialize)]
struct JwksKey {
    #[serde(default)]
    kty: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    x: Option<String>,
}

#[derive(Deserialize)]
struct JwksResponse {
    keys: Vec<JwksKey>,
}

#[derive(Default)]
struct JwksCache {
    keys: HashMap<String, VerifyingKey>,
    /// Set only on a *successful* fetch; drives the TTL.
    fetched_at: Option<Instant>,
    /// How long the last successful fetch said to cache for.
    ttl: Option<Duration>,
    /// Set on every fetch *attempt*. Drives the refetch floor, so a string
    /// of failures neither resets the TTL clock nor lifts the floor.
    last_attempt_at: Option<Instant>,
    /// Whether the attempt at `last_attempt_at` failed (transport error,
    /// non-2xx, unparseable body). Distinguishes, while the floor is active,
    /// "we have a fresh key set and it just doesn't have this kid" (401)
    /// from "we don't actually know what's in the key set right now" (503)
    /// — contract rule 5. Meaningless when `last_attempt_at` is `None`.
    last_attempt_failed: bool,
}

impl JwksCache {
    fn cached_and_fresh(&self, kid: &str, fallback_ttl: Duration) -> Option<VerifyingKey> {
        let key = self.keys.get(kid)?;
        let fetched_at = self.fetched_at?;
        (fetched_at.elapsed() < self.ttl.unwrap_or(fallback_ttl)).then_some(*key)
    }
}

pub struct AdminJwksClient {
    config: AdminJwksClientConfig,
    http: reqwest::Client,
    cache: RwLock<JwksCache>,
}

impl AdminJwksClient {
    pub fn new(config: AdminJwksClientConfig, http: reqwest::Client) -> Self {
        Self {
            config,
            http,
            cache: RwLock::new(JwksCache::default()),
        }
    }

    /// Fetch and parse the key set. Does not touch the cache — the caller
    /// commits the result, so the write lock's scope stays visible at the
    /// call site.
    async fn fetch(&self) -> GuardResult<(HashMap<String, VerifyingKey>, Duration)> {
        let response = self
            .http
            .get(&self.config.jwks_url)
            .timeout(self.config.http_timeout)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, url = %self.config.jwks_url, "admin JWKS fetch failed");
                GuardError::unavailable("admin JWKS endpoint unreachable")
            })?;

        if !response.status().is_success() {
            tracing::warn!(
                status = response.status().as_u16(),
                url = %self.config.jwks_url,
                "admin JWKS fetch returned non-2xx"
            );
            return Err(GuardError::unavailable(format!(
                "admin JWKS endpoint returned {}",
                response.status()
            )));
        }

        let ttl = max_age(response.headers()).unwrap_or(self.config.default_ttl);
        let body: JwksResponse = response.json().await.map_err(|e| {
            tracing::warn!(error = %e, "admin JWKS response parse failed");
            GuardError::unavailable("admin JWKS response was malformed")
        })?;

        let mut keys = HashMap::new();
        for key in body.keys {
            let (Some(kty), Some(kid), Some(x)) =
                (key.kty.as_deref(), key.kid.as_deref(), key.x.as_deref())
            else {
                continue;
            };
            if kty != "OKP" || key.crv.as_deref() != Some("Ed25519") {
                continue;
            }
            match decode_ed25519_key(x) {
                Ok(verifying_key) => {
                    keys.insert(kid.to_string(), verifying_key);
                }
                // One malformed key must not poison the whole set.
                Err(_) => tracing::warn!(kid = %kid, "skipping malformed admin JWKS key"),
            }
        }
        Ok((keys, ttl.min(self.config.max_ttl)))
    }
}

#[async_trait]
impl JwksKeyProvider for AdminJwksClient {
    async fn resolve_key(&self, kid: &str) -> GuardResult<VerifyingKey> {
        {
            let cache = self.cache.read().await;
            if let Some(key) = cache.cached_and_fresh(kid, self.config.default_ttl) {
                return Ok(key);
            }
        }

        let mut cache = self.cache.write().await;
        // Another task may have refreshed while we waited for the lock.
        if let Some(key) = cache.cached_and_fresh(kid, self.config.default_ttl) {
            return Ok(key);
        }

        if let Some(last) = cache.last_attempt_at {
            if last.elapsed() < self.config.min_refetch_interval {
                if cache.last_attempt_failed {
                    // The floor is holding us back from a *retry*, not from
                    // re-checking a key set we actually have — we don't know
                    // whether this kid is good or bad, so this is an outage,
                    // not a bad credential (contract rule 5).
                    tracing::warn!(
                        kid = %kid,
                        "admin JWKS refetch skipped (floor active, last attempt failed)"
                    );
                    return Err(GuardError::unavailable(
                        "admin JWKS endpoint unreachable (refetch floor active)",
                    ));
                }
                tracing::warn!(kid = %kid, "admin JWKS refetch skipped (floor active)");
                return Err(GuardError::unauthorized(format!(
                    "unknown admin JWT kid '{kid}' (refetch floor active)"
                )));
            }
        }

        cache.last_attempt_at = Some(Instant::now());
        // Fail-closed: on Err the cache is left exactly as it was, so a
        // reachable-again IdP restores service without a restart.
        let (keys, ttl) = match self.fetch().await {
            Ok(fetched) => {
                cache.last_attempt_failed = false;
                fetched
            }
            Err(e) => {
                cache.last_attempt_failed = true;
                return Err(e);
            }
        };
        cache.keys = keys;
        cache.ttl = Some(ttl);
        cache.fetched_at = Some(Instant::now());

        cache.keys.get(kid).copied().ok_or_else(|| {
            GuardError::unauthorized(format!(
                "unknown admin JWT kid '{kid}' (not present in admin JWKS after refresh)"
            ))
        })
    }
}

/// `max-age` from a `Cache-Control` header, if it carries one.
fn max_age(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::CACHE_CONTROL)?.to_str().ok()?;
    value
        .split(',')
        .map(str::trim)
        .find_map(|directive| {
            directive
                .strip_prefix("max-age=")
                .or_else(|| directive.strip_prefix("s-maxage="))
        })
        .and_then(|seconds| seconds.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

fn decode_ed25519_key(x_b64: &str) -> GuardResult<VerifyingKey> {
    let bytes = URL_SAFE_NO_PAD
        .decode(x_b64)
        .or_else(|_| STANDARD.decode(x_b64))
        .map_err(|_| GuardError::internal("admin JWKS key `x` is not valid base64"))?;
    let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        GuardError::internal(format!(
            "admin JWKS key `x` decoded to {} bytes (expected 32)",
            bytes.len()
        ))
    })?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| {
        GuardError::internal(format!("admin JWKS key is not a valid Ed25519 key: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, CACHE_CONTROL};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A client with a short HTTP timeout, so a delayed mock response
    /// actually times out instead of just being slow. Mirrors
    /// `introspect_client`'s `TEST_HTTP_TIMEOUT` pattern.
    fn short_timeout_client(jwks_url: String) -> AdminJwksClient {
        let mut config = AdminJwksClientConfig::new(jwks_url);
        config.http_timeout = Duration::from_millis(100);
        AdminJwksClient::new(config, reqwest::Client::new())
    }

    /// Contract rule 5: a cold cache plus an unreachable JWKS endpoint is a
    /// dependency outage (`Unavailable`/503), not a bad credential.
    #[tokio::test]
    async fn a_transport_failure_on_cold_cache_is_unavailable_not_unauthorized() {
        let client = short_timeout_client("http://127.0.0.1:1/jwks.json".to_string());
        let err = client.resolve_key("any-kid").await.unwrap_err();
        assert!(
            matches!(err, GuardError::Unavailable(_)),
            "expected Unavailable, got {err:?}"
        );
    }

    /// Contract rule 5: a JWKS endpoint answering non-2xx is unreachable for
    /// our purposes; the token is not implicated.
    #[tokio::test]
    async fn a_non_2xx_jwks_response_is_unavailable_not_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = short_timeout_client(format!("{}/jwks.json", server.uri()));
        let err = client.resolve_key("any-kid").await.unwrap_err();
        assert!(
            matches!(err, GuardError::Unavailable(_)),
            "expected Unavailable, got {err:?}"
        );
    }

    /// Contract rule 5: an unparseable key set is a broken dependency, not a
    /// bad credential.
    #[tokio::test]
    async fn a_malformed_jwks_body_is_unavailable_not_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;

        let client = short_timeout_client(format!("{}/jwks.json", server.uri()));
        let err = client.resolve_key("any-kid").await.unwrap_err();
        assert!(
            matches!(err, GuardError::Unavailable(_)),
            "expected Unavailable, got {err:?}"
        );
    }

    /// A valid `{"keys": [...]}` body carrying one Ed25519 JWK under `kid`.
    fn jwks_body_with_key(kid: &str, key: VerifyingKey) -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "OKP",
                "crv": "Ed25519",
                "kid": kid,
                "x": URL_SAFE_NO_PAD.encode(key.to_bytes()),
            }]
        })
    }

    /// The bug this module exists to guard against: `last_attempt_at` is
    /// stamped before the fetch, so a failed fetch used to leave the floor
    /// window answering 401 ("bad credential") instead of 503 ("cannot
    /// tell") for every request until the floor lifted — turning one IdP
    /// blip into a run of false "this token is dead" verdicts. Two requests
    /// back-to-back against a cold cache and an unreachable IdP (well inside
    /// `min_refetch_interval`) must both come back `Unavailable`.
    #[tokio::test]
    async fn every_request_within_the_floor_is_unavailable_when_the_last_attempt_failed() {
        let client = short_timeout_client("http://127.0.0.1:1/jwks.json".to_string());

        let first = client.resolve_key("any-kid").await.unwrap_err();
        assert!(
            matches!(first, GuardError::Unavailable(_)),
            "expected Unavailable on the first (fetching) request, got {first:?}"
        );

        // Still inside min_refetch_interval (5s default) — this must not
        // fall back to "unknown kid" just because the floor is active.
        let second = client.resolve_key("any-kid").await.unwrap_err();
        assert!(
            matches!(second, GuardError::Unavailable(_)),
            "expected Unavailable on the floor-active retry, got {second:?}"
        );
    }

    /// Contract rule 5, the other half: once a key set has actually arrived,
    /// a `kid` it doesn't contain is a bad credential, not an outage — even
    /// though this request is the one that just performed the fetch.
    #[tokio::test]
    async fn a_freshly_fetched_key_set_missing_the_kid_is_unauthorized() {
        let server = MockServer::start().await;
        let present_key = ed25519_dalek::SigningKey::from_bytes(&[5u8; 32]).verifying_key();
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(jwks_body_with_key("present-kid", present_key)),
            )
            .mount(&server)
            .await;

        let client = short_timeout_client(format!("{}/jwks.json", server.uri()));
        let err = client.resolve_key("absent-kid").await.unwrap_err();
        assert!(
            matches!(err, GuardError::Unauthorized(_)),
            "expected Unauthorized, got {err:?}"
        );
    }

    /// Contract rule 5: the floor being active after a *successful* fetch is
    /// unchanged by this fix — a second unknown `kid` inside the floor still
    /// answers 401 without a second network call, it just no longer shares
    /// a code path with "the fetch itself failed".
    #[tokio::test]
    async fn the_floor_after_a_successful_fetch_still_answers_unauthorized_without_refetching() {
        let server = MockServer::start().await;
        let present_key = ed25519_dalek::SigningKey::from_bytes(&[6u8; 32]).verifying_key();
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(jwks_body_with_key("present-kid", present_key)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = short_timeout_client(format!("{}/jwks.json", server.uri()));

        let first = client.resolve_key("absent-kid").await.unwrap_err();
        assert!(matches!(first, GuardError::Unauthorized(_)));

        // Inside the floor: must stay 401, and `.expect(1)` above fails the
        // test on drop if this triggers a second HTTP call.
        let second = client.resolve_key("absent-kid").await.unwrap_err();
        assert!(
            matches!(second, GuardError::Unauthorized(_)),
            "expected Unauthorized (unchanged), got {second:?}"
        );
    }

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CACHE_CONTROL, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn max_age_is_read_out_of_cache_control() {
        assert_eq!(
            max_age(&headers("public, max-age=300")),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            max_age(&headers("s-maxage=60, public")),
            Some(Duration::from_secs(60))
        );
        for absent in ["no-store", "public", "max-age=0", "max-age=abc", ""] {
            assert_eq!(max_age(&headers(absent)), None, "{absent:?}");
        }
        assert_eq!(max_age(&HeaderMap::new()), None);
    }

    #[test]
    fn an_ed25519_jwk_x_decodes_in_either_base64_alphabet() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        let url_safe = URL_SAFE_NO_PAD.encode(key.to_bytes());
        let standard = STANDARD.encode(key.to_bytes());
        assert_eq!(decode_ed25519_key(&url_safe).unwrap(), key);
        assert_eq!(decode_ed25519_key(&standard).unwrap(), key);
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused_rather_than_padded() {
        assert!(decode_ed25519_key(&URL_SAFE_NO_PAD.encode([1u8; 16])).is_err());
        assert!(decode_ed25519_key("not base64!!").is_err());
    }
}
