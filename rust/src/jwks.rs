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
//! - **Single flight, and the cache lock is never held across the fetch.**
//!   A burst of concurrent misses still collapses into one round trip, but
//!   it does so by having one task fetch while the rest wait on a
//!   [`Notify`] — not by making them queue for a write lock somebody is
//!   holding for the length of an HTTP timeout. The distinction only
//!   shows up in the failure mode that matters: an endpoint that accepts
//!   the connection and then says nothing. Under the old arrangement each
//!   waiter inherited the lock in turn, found the floor already lapsed
//!   (the previous attempt had taken longer than the floor itself), and
//!   fetched again — one queue, one upstream call per request, every admin
//!   request in the pod stuck in it.
//! - **A stale key beats a dead admin surface.** Layer 1 runs on *every*
//!   admin request, reads included, so a JWKS outage outliving the TTL used
//!   to take the entire admin surface down. Within
//!   [`DEFAULT_JWKS_STALE_GRACE`] a key we already hold is served with a
//!   `warn` rather than a 503. See that constant for the trade-off.
//! - **A breaker fronts the fetch**, the same one the introspect path uses.
//!   Layer 1 is the busier of the two and had none.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use tokio::sync::{Notify, RwLock};

use crate::circuit_breaker::CircuitBreaker;
use crate::clock::Clock;
use crate::error::{GuardError, GuardResult};

/// Used when the JWKS response carries no `Cache-Control: max-age`.
pub const DEFAULT_JWKS_TTL: Duration = Duration::from_secs(300);

/// Ceiling on a server-supplied TTL. A key set cached for a day would
/// outlive a key rotation and reject every token minted after it.
pub const MAX_JWKS_TTL: Duration = Duration::from_secs(3600);

/// How far past its TTL a cached key may still be served, and only while
/// the IdP cannot be reached to say otherwise.
///
/// This is a deliberate, bounded departure from "expired cache means no
/// answer", and it is worth stating why rather than tuning by feel. The two
/// events being traded against each other have very different frequencies.
/// A JWKS endpoint unreachable for longer than one TTL is an ordinary
/// dependency blip — a rollout, a restart, a partition — and without a
/// grace window it takes the *whole* admin surface down, reads included,
/// because offline verification runs on every request. A key withdrawn from
/// the key set is rare, and deliberate.
///
/// What the window actually costs is narrow. An Ed25519 *public* key is not
/// a secret; a token verified by a stale one still has to carry a valid
/// signature and pass `exp`, `iss` and `type`; and a compromised session is
/// cut off by introspection on the next mutation whichever key signed it.
/// The exposure is only "the IdP rotated a key out and this pod has not
/// managed to notice yet".
///
/// 15 minutes is picked to sit clearly on both sides of that: long enough
/// to ride out any outage worth staying up through, short enough that an
/// operator withdrawing a key has a stated upper bound instead of an
/// open-ended one. Every second of it is logged at `warn` with the `kid`,
/// so it is visible while it is happening rather than afterwards. Set
/// [`AdminJwksClientConfig::stale_grace`] to zero to opt out entirely.
pub const DEFAULT_JWKS_STALE_GRACE: Duration = Duration::from_secs(900);

pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Floor between unknown-`kid`-triggered refetch attempts.
pub const DEFAULT_MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(5);

/// Consecutive failed fetches before the JWKS breaker opens, and how long it
/// stays open. The same numbers the introspect path uses (contract rule 7),
/// for the same reason: past this many failures in a row the next attempt is
/// not diagnosis, it is just another task parked on a timeout.
pub const DEFAULT_JWKS_BREAKER_FAILURE_THRESHOLD: u32 = 5;
pub const DEFAULT_JWKS_BREAKER_RESET: Duration = Duration::from_secs(30);

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
    /// How far past the TTL a cached key may still be served while the IdP
    /// is unreachable. Zero disables stale serving, restoring the older
    /// "TTL lapsed and the IdP is down, so 503" behaviour. See
    /// [`DEFAULT_JWKS_STALE_GRACE`].
    pub stale_grace: Duration,
    pub breaker_failure_threshold: u32,
    pub breaker_reset: Duration,
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
            stale_grace: DEFAULT_JWKS_STALE_GRACE,
            breaker_failure_threshold: DEFAULT_JWKS_BREAKER_FAILURE_THRESHOLD,
            breaker_reset: DEFAULT_JWKS_BREAKER_RESET,
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

    /// A key held past its TTL but still inside `grace`. Kept separate from
    /// [`JwksCache::cached_and_fresh`] on purpose: "is this key current?"
    /// and "is this key still worth serving during an outage?" are different
    /// questions, and a call site that confuses them would be serving stale
    /// keys when it did not mean to.
    fn cached_within_grace(
        &self,
        kid: &str,
        fallback_ttl: Duration,
        grace: Duration,
    ) -> Option<VerifyingKey> {
        let key = self.keys.get(kid)?;
        let fetched_at = self.fetched_at?;
        let horizon = self.ttl.unwrap_or(fallback_ttl).saturating_add(grace);
        (fetched_at.elapsed() < horizon).then_some(*key)
    }
}

pub struct AdminJwksClient {
    config: AdminJwksClientConfig,
    http: reqwest::Client,
    cache: RwLock<JwksCache>,
    /// Single flight. Set while exactly one task is out fetching; the rest
    /// join it on `fetch_settled` rather than pile up on the write lock.
    fetch_in_flight: AtomicBool,
    /// Woken once per settled fetch attempt, whichever way it settled.
    fetch_settled: Notify,
    breaker: CircuitBreaker,
}

impl AdminJwksClient {
    pub fn new(config: AdminJwksClientConfig, http: reqwest::Client) -> Self {
        let breaker = CircuitBreaker::new(config.breaker_failure_threshold, config.breaker_reset);
        Self {
            config,
            http,
            cache: RwLock::new(JwksCache::default()),
            fetch_in_flight: AtomicBool::new(false),
            fetch_settled: Notify::new(),
            breaker,
        }
    }

    /// Build the breaker on an injected clock instead of the wall clock, so
    /// a test can cross the reset window without sleeping. Mirrors
    /// [`crate::introspect_client::AdminIntrospectClient::with_breaker_clock`].
    pub fn with_breaker_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.breaker = CircuitBreaker::with_clock(
            self.config.breaker_failure_threshold,
            self.config.breaker_reset,
            clock,
        );
        self
    }

    /// A key we already hold, past its TTL but inside the grace window, for
    /// the paths whose only other answer is `Unavailable` on a request we
    /// could in fact have served.
    ///
    /// Only ever consulted when a fetch *failed* or never happened. A fetch
    /// that succeeds replaces the key set outright, so a `kid` the IdP has
    /// rotated out can never come back through here — which is the property
    /// that keeps the grace window bounded by the outage rather than open
    /// ended.
    fn stale_key(
        &self,
        cache: &JwksCache,
        kid: &str,
        reason: &'static str,
    ) -> Option<VerifyingKey> {
        if self.config.stale_grace.is_zero() {
            return None;
        }
        let key =
            cache.cached_within_grace(kid, self.config.default_ttl, self.config.stale_grace)?;
        tracing::warn!(
            kid = %kid,
            reason,
            grace_secs = self.config.stale_grace.as_secs(),
            "serving a stale admin JWKS key: past its TTL and the key set could not be refreshed"
        );
        Some(key)
    }

    /// The answer for a task that joined an in-flight fetch instead of
    /// performing one. It reads the outcome the leader committed rather
    /// than re-deciding anything, which is what stops a queue of waiters
    /// from turning into a queue of fetches.
    async fn answer_from_cache(&self, kid: &str) -> GuardResult<VerifyingKey> {
        let cache = self.cache.read().await;
        if let Some(key) = cache.cached_and_fresh(kid, self.config.default_ttl) {
            return Ok(key);
        }
        if !cache.last_attempt_failed {
            // The fetch we waited on succeeded, and the key set it brought
            // back does not carry this kid. That is a verdict on the
            // credential, not an outage (contract rule 5).
            return Err(GuardError::unauthorized(format!(
                "unknown admin JWT kid '{kid}' (not present in admin JWKS after refresh)"
            )));
        }
        // It failed, or was abandoned. We do not know what is in the key
        // set, so this is an outage — unless we still hold a usable key.
        match self.stale_key(&cache, kid, "joined a failed jwks fetch") {
            Some(key) => Ok(key),
            None => Err(GuardError::unavailable("admin JWKS endpoint unreachable")),
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

/// Held by the one task performing a fetch, and released — flag cleared,
/// joiners woken — on every way out of that fetch.
///
/// A guard rather than two statements at the end of the happy path because
/// `resolve_key` can be dropped mid-fetch: axum drops the whole handler
/// future when a client disconnects, and a leaked `fetch_in_flight` would
/// strand every joiner behind a leader that no longer exists. The join has
/// no deadline of its own, so that has to be impossible rather than
/// unlikely.
///
/// The flag is cleared *before* the notification on purpose — it is the half
/// of the handshake a joiner re-reads after registering, so that a joiner
/// which registered a moment too late still notices the fetch has settled
/// instead of waiting for a notification that has already been sent.
struct FetchLease<'a> {
    client: &'a AdminJwksClient,
}

impl Drop for FetchLease<'_> {
    fn drop(&mut self) {
        self.client.fetch_in_flight.store(false, Ordering::Release);
        self.client.fetch_settled.notify_waiters();
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

        // Everything between here and the `drop(cache)` below runs under the
        // write lock and contains no `.await`. That is the point of the
        // shape: the lock decides *who* fetches, it is never held *while*
        // anybody fetches.
        let mut cache = self.cache.write().await;

        // Another task may have refreshed while we waited for the lock.
        if let Some(key) = cache.cached_and_fresh(kid, self.config.default_ttl) {
            return Ok(key);
        }

        if self.fetch_in_flight.load(Ordering::Acquire) {
            // Join the fetch already out rather than start a second one.
            //
            // `notify_waiters` stores no permit, so a task that registers
            // after the notification has gone out would sleep until the
            // *next* fetch settles — which, if no further request comes in,
            // is never. Register first and then re-read the flag: the leader
            // clears it before it notifies, so a flag still set on this
            // second read means the notification has not gone out yet and we
            // are already in the list to receive it. `Notify`'s own lock
            // serializes registration against notification, which is what
            // gives that second read something to see.
            let mut settled = std::pin::pin!(self.fetch_settled.notified());
            settled.as_mut().enable();
            let still_fetching = self.fetch_in_flight.load(Ordering::Acquire);
            drop(cache);
            if still_fetching {
                settled.await;
            }
            return self.answer_from_cache(kid).await;
        }

        if let Some(last) = cache.last_attempt_at {
            if last.elapsed() < self.config.min_refetch_interval {
                if cache.last_attempt_failed {
                    // The floor is holding us back from a *retry*, not from
                    // re-checking a key set we actually have — we don't know
                    // whether this kid is good or bad, so this is an outage,
                    // not a bad credential (contract rule 5). Unless we do
                    // still hold this key, in which case serving it beats
                    // refusing a request we could answer.
                    if let Some(key) = self.stale_key(&cache, kid, "refetch floor active") {
                        return Ok(key);
                    }
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

        // A run of failures long enough to open the breaker means the next
        // attempt would not be diagnosis, only another task parked on the
        // HTTP timeout. Same reasoning as the introspect path, except this
        // one runs on reads too, which is why it needed the breaker more.
        //
        // `permit` is held from here to the end of the function on purpose.
        // While the breaker is half-open it *is* the probe, and this
        // function has an `.await` and several `return`s between here and
        // the outcome being reported; carrying it as a value means the probe
        // is handed back on all of them, cancellation included. Reporting an
        // outcome does not release it — being dropped does.
        let Some(permit) = self.breaker.allow_request() else {
            if let Some(key) = self.stale_key(&cache, kid, "jwks circuit open") {
                return Ok(key);
            }
            return Err(GuardError::unavailable(
                "admin JWKS endpoint is temporarily unavailable",
            ));
        };

        cache.last_attempt_at = Some(Instant::now());
        // Pessimistic on purpose: an attempt counts as failed until it
        // proves otherwise. A leader cancelled mid-fetch then leaves the
        // cache saying "we do not know what is in the key set" (503) rather
        // than "we looked, and this kid is not in it" (401).
        cache.last_attempt_failed = true;
        self.fetch_in_flight.store(true, Ordering::Release);
        drop(cache);

        let lease = FetchLease { client: self };
        let fetched = self.fetch().await;

        let mut cache = self.cache.write().await;
        // Fail-closed: on Err the cached key set is left exactly as it was,
        // so a reachable-again IdP restores service without a restart — and
        // within the grace window so does the key set we still hold.
        let answer = match fetched {
            Ok((keys, ttl)) => {
                permit.record_success();
                cache.last_attempt_failed = false;
                cache.keys = keys;
                cache.ttl = Some(ttl);
                cache.fetched_at = Some(Instant::now());
                cache.keys.get(kid).copied().ok_or_else(|| {
                    GuardError::unauthorized(format!(
                        "unknown admin JWT kid '{kid}' (not present in admin JWKS after refresh)"
                    ))
                })
            }
            Err(e) => {
                permit.record_failure();
                match self.stale_key(&cache, kid, "jwks fetch failed") {
                    Some(key) => Ok(key),
                    None => Err(e),
                }
            }
        };

        // Order matters: joiners read the cache directly, so the commit has
        // to be visible before the lease wakes them.
        drop(cache);
        drop(lease);
        answer
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

    /// A client whose cache goes stale almost immediately and whose refetch
    /// floor is short enough to stay out of the way, so a test can reach the
    /// "cache is warm but past its TTL" state in milliseconds.
    fn quickly_stale_client(jwks_url: String) -> AdminJwksClientConfig {
        let mut config = AdminJwksClientConfig::new(jwks_url);
        config.http_timeout = Duration::from_millis(200);
        config.default_ttl = Duration::from_millis(50);
        config.min_refetch_interval = Duration::from_millis(10);
        config
    }

    /// SEC-01. The failure mode this is really about is not a refused
    /// connection but an endpoint that accepts the TCP connection and then
    /// says nothing, because that is what a hung pod or a partition looks
    /// like. The old code held the cache's write lock across the fetch, so
    /// each waiter inherited the lock in turn, found the refetch floor
    /// already lapsed — the previous attempt had taken longer than the floor
    /// itself — and started its own fetch. Every admin request in the pod,
    /// reads included, ended up in one queue costing an HTTP timeout each
    /// and firing one upstream call each.
    ///
    /// The refetch floor is switched off here, which is the point rather
    /// than a convenience: the finding is that the floor cannot protect
    /// anything once an attempt outlives it, and zero is that limiting case.
    /// What is left holding the upstream to one call is the single flight
    /// itself, so `.expect(1)` measures exactly the property under test.
    /// The elapsed-time bound corroborates it at four times a single
    /// timeout, loose enough that a loaded machine does not make it flap.
    ///
    /// Note also that 23 of these 24 answers come from tasks that joined the
    /// fetch rather than performed it, so this covers a joiner reading a
    /// failed attempt as an outage (503) rather than as a bad credential
    /// (401).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_misses_collapse_into_one_fetch_rather_than_a_queue_of_them() {
        const CALLERS: usize = 24;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(2_000)))
            .expect(1)
            .mount(&server)
            .await;

        let mut config = quickly_stale_client(format!("{}/jwks.json", server.uri()));
        config.http_timeout = Duration::from_millis(200);
        config.min_refetch_interval = Duration::ZERO;
        let client = Arc::new(AdminJwksClient::new(config, reqwest::Client::new()));

        let started = Instant::now();
        let mut callers = Vec::with_capacity(CALLERS);
        for _ in 0..CALLERS {
            let client = client.clone();
            callers.push(tokio::spawn(
                async move { client.resolve_key("any-kid").await },
            ));
        }
        for caller in callers {
            let err = caller.await.unwrap().unwrap_err();
            assert!(
                matches!(err, GuardError::Unavailable(_)),
                "expected Unavailable, got {err:?}"
            );
        }

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(800),
            "{CALLERS} callers took {elapsed:?}; serialized behind one another they would have \
             cost a 200ms timeout each"
        );
    }

    /// The hazard single flight introduces and has to close: the task that
    /// went out to fetch can simply vanish. Axum drops the whole handler
    /// future when a client disconnects, and a leader dropped mid-fetch
    /// without releasing its claim would leave every later caller waiting on
    /// a fetch that will never settle — a hang, which is worse than the
    /// pile-up being fixed. It also pins the pessimistic
    /// `last_attempt_failed`: an abandoned attempt tells us nothing about
    /// the key set, so what follows is an outage (503), not a verdict on the
    /// credential (401).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_leader_cancelled_mid_fetch_does_not_strand_the_callers_behind_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;

        let mut config = AdminJwksClientConfig::new(format!("{}/jwks.json", server.uri()));
        // Long enough that the fetch is unambiguously still in flight when
        // the leader is cancelled out from under it.
        config.http_timeout = Duration::from_secs(30);
        let client = Arc::new(AdminJwksClient::new(config, reqwest::Client::new()));

        let leader = tokio::spawn({
            let client = client.clone();
            async move { client.resolve_key("any-kid").await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        leader.abort();
        let _ = leader.await;

        let answer = tokio::time::timeout(Duration::from_secs(5), client.resolve_key("any-kid"))
            .await
            .expect("a cancelled leader must not strand the callers behind it");
        let err = answer.unwrap_err();
        assert!(
            matches!(err, GuardError::Unavailable(_)),
            "an abandoned attempt is 'we do not know', not 'this kid is bad': got {err:?}"
        );
    }

    /// SEC-02. Offline verification runs on every admin request, so a JWKS
    /// endpoint that stays down past the TTL used to take the whole admin
    /// surface with it — reads included — while a perfectly usable key sat
    /// in memory. Inside the grace window that key is served instead.
    #[tokio::test]
    async fn a_failed_refetch_serves_the_key_we_still_hold_rather_than_503() {
        let server = MockServer::start().await;
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jwks_body_with_key("warm", key)))
            .mount(&server)
            .await;

        let config = quickly_stale_client(format!("{}/jwks.json", server.uri()));
        let client = AdminJwksClient::new(config, reqwest::Client::new());
        assert_eq!(client.resolve_key("warm").await.unwrap(), key);

        // Past the TTL and the floor, with the endpoint now broken.
        tokio::time::sleep(Duration::from_millis(120)).await;
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert_eq!(
            client.resolve_key("warm").await.unwrap(),
            key,
            "a stale key beats refusing a request we could answer"
        );
    }

    /// The same sequence with the grace window switched off, which is both
    /// the knob's test and a statement of what the old behaviour was.
    #[tokio::test]
    async fn a_zero_grace_window_restores_the_fail_closed_on_ttl_lapse_behaviour() {
        let server = MockServer::start().await;
        let key = ed25519_dalek::SigningKey::from_bytes(&[8u8; 32]).verifying_key();
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jwks_body_with_key("warm", key)))
            .mount(&server)
            .await;

        let mut config = quickly_stale_client(format!("{}/jwks.json", server.uri()));
        config.stale_grace = Duration::ZERO;
        let client = AdminJwksClient::new(config, reqwest::Client::new());
        assert_eq!(client.resolve_key("warm").await.unwrap(), key);

        tokio::time::sleep(Duration::from_millis(120)).await;
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = client.resolve_key("warm").await.unwrap_err();
        assert!(
            matches!(err, GuardError::Unavailable(_)),
            "expected Unavailable, got {err:?}"
        );
    }

    /// The property that keeps the grace window honest: it is a fallback for
    /// a key set we could not *reach*, never for one we reached and found
    /// this `kid` gone from. A rotation is the IdP telling us the key is
    /// withdrawn, and it must win over anything still in memory — contract
    /// rule 5's 401 half, on a warm cache rather than a cold one.
    #[tokio::test]
    async fn a_rotated_out_kid_is_unauthorized_even_with_a_stale_copy_still_cached() {
        let server = MockServer::start().await;
        let old_key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let new_key = ed25519_dalek::SigningKey::from_bytes(&[10u8; 32]).verifying_key();
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(jwks_body_with_key("retiring", old_key)),
            )
            .mount(&server)
            .await;

        let config = quickly_stale_client(format!("{}/jwks.json", server.uri()));
        let client = AdminJwksClient::new(config, reqwest::Client::new());
        assert_eq!(client.resolve_key("retiring").await.unwrap(), old_key);

        tokio::time::sleep(Duration::from_millis(120)).await;
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(jwks_body_with_key("current", new_key)),
            )
            .mount(&server)
            .await;

        let err = client.resolve_key("retiring").await.unwrap_err();
        assert!(
            matches!(err, GuardError::Unauthorized(_)),
            "a key the IdP has withdrawn is a dead credential, not an outage: got {err:?}"
        );
        assert_eq!(client.resolve_key("current").await.unwrap(), new_key);
    }

    /// SEC-05. Layer 1 runs on every admin request and had no breaker at
    /// all, so a down IdP kept collecting one HTTP timeout per unknown-kid
    /// request forever. `.expect(2)` fails the test on drop if a call gets
    /// out past the threshold.
    #[tokio::test]
    async fn the_breaker_stops_the_jwks_path_after_a_run_of_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(500))
            .expect(2)
            .mount(&server)
            .await;

        let mut config = AdminJwksClientConfig::new(format!("{}/jwks.json", server.uri()));
        config.http_timeout = Duration::from_millis(200);
        // The floor is not what is being tested here; take it out of the way
        // so every call would otherwise reach the network.
        config.min_refetch_interval = Duration::ZERO;
        config.breaker_failure_threshold = 2;
        config.breaker_reset = Duration::from_secs(300);
        let client = AdminJwksClient::new(config, reqwest::Client::new());

        for attempt in 0..6 {
            let err = client.resolve_key("any-kid").await.unwrap_err();
            assert!(
                matches!(err, GuardError::Unavailable(_)),
                "attempt {attempt}: expected Unavailable, got {err:?}"
            );
        }
    }

    /// VS-01, end to end and on the busiest path in the crate. Offline
    /// verification runs on every admin request, reads included, so a
    /// breaker that cannot reopen here is the whole admin surface down.
    ///
    /// The sequence is the one that showed up in review, and none of its
    /// steps are exotic: the IdP fails, the breaker opens, the window
    /// passes, and the request that gets handed the probe is cut short
    /// before it can report anything. Cutting it short is the ordinary case
    /// — the service wraps its handlers in a request timeout shorter than
    /// the worst-case JWKS fetch, and a client that closes the connection
    /// does the same thing to the future. The probe has to survive that,
    /// because the state it leaves behind outlives the outage: with it lost,
    /// the IdP recovering changes nothing, since nothing is allowed out to
    /// notice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_probe_cancelled_mid_fetch_does_not_leave_the_admin_surface_shut() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let key = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]).verifying_key();
        let mut config = AdminJwksClientConfig::new(format!("{}/jwks.json", server.uri()));
        config.http_timeout = Duration::from_secs(5);
        // Neither the floor nor the grace window is what is under test; take
        // both out of the way so the breaker is the only thing that can
        // refuse, and so a pass cannot come from a cached key.
        config.min_refetch_interval = Duration::ZERO;
        config.stale_grace = Duration::ZERO;
        config.breaker_failure_threshold = 1;
        config.breaker_reset = Duration::from_secs(30);
        let clock = Arc::new(crate::clock::FakeClock::new());
        let client =
            AdminJwksClient::new(config, reqwest::Client::new()).with_breaker_clock(clock.clone());

        // One failed fetch opens a threshold-1 breaker.
        assert!(matches!(
            client.resolve_key("signing-kid").await,
            Err(GuardError::Unavailable(_))
        ));

        // The window passes, so this caller is lent the probe — and is then
        // dropped while the fetch is still outstanding.
        clock.advance(Duration::from_millis(30_001));
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(jwks_body_with_key("signing-kid", key))
                    .set_delay(Duration::from_secs(30)),
            )
            .mount(&server)
            .await;
        let cut_short = tokio::time::timeout(
            Duration::from_millis(150),
            client.resolve_key("signing-kid"),
        )
        .await;
        assert!(
            cut_short.is_err(),
            "the fetch has to still be in flight when the caller is dropped"
        );

        // The IdP is answering normally again. The next request must be able
        // to find that out.
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(jwks_body_with_key("signing-kid", key)),
            )
            .mount(&server)
            .await;
        assert_eq!(
            client
                .resolve_key("signing-kid")
                .await
                .expect("a cancelled probe must not outlive the outage it was sent to measure"),
            key
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
