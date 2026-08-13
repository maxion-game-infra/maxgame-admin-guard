//! The HTTP client for layer 2: a server-to-server introspection call
//! against the admin IdP, made on every admin *write* (contract rule 4).
//!
//! The point of the call is immediacy. An access token lives for its full
//! lifetime, so without it a deactivated admin — or one whose feature was
//! just revoked — would keep mutating until it expired. With it, the next
//! mutation is refused.
//!
//! Fail-closed semantics are carried by the return type, not by a
//! convention:
//! - `Ok(Active { .. })` is the only path that lets a write through.
//! - `Ok(Inactive { .. })` is a legitimate IdP answer → 401.
//! - `Err(Unavailable)` is "we could not get an answer" → 503.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::circuit_breaker::CircuitBreaker;
use crate::clock::{Clock, SystemClock};
use crate::error::{GuardError, GuardResult};
use crate::introspect::{AdminIntrospection, AdminIntrospector, IntrospectionBody};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
pub const DEFAULT_BREAKER_FAILURE_THRESHOLD: u32 = 5;
pub const DEFAULT_BREAKER_RESET: Duration = Duration::from_secs(30);
pub const DEFAULT_HEADER_NAME: &str = "x-api-key";

pub struct AdminIntrospectClient {
    http: reqwest::Client,
    introspect_url: String,
    header_name: String,
    api_key: String,
    timeout: Duration,
    breaker: CircuitBreaker,
}

impl AdminIntrospectClient {
    pub fn new(
        http: reqwest::Client,
        introspect_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            http,
            introspect_url: introspect_url.into(),
            header_name: DEFAULT_HEADER_NAME.to_string(),
            api_key: api_key.into(),
            timeout: DEFAULT_TIMEOUT,
            breaker: CircuitBreaker::new(DEFAULT_BREAKER_FAILURE_THRESHOLD, DEFAULT_BREAKER_RESET),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the header the API key is sent under. Defaults to
    /// `x-api-key`, matching every IdP this crate talks to today.
    pub fn with_header_name(mut self, header_name: impl Into<String>) -> Self {
        self.header_name = header_name.into();
        self
    }

    /// Override the breaker's threshold and reset window. Defaults to 5
    /// consecutive failures / 30s, per contract rule 7.
    pub fn with_breaker(mut self, failure_threshold: u32, reset_after: Duration) -> Self {
        self.breaker = CircuitBreaker::new(failure_threshold, reset_after);
        self
    }

    /// Build the breaker on an injected clock instead of the wall clock, so
    /// a test can cross the reset window without sleeping.
    pub fn with_breaker_clock(
        mut self,
        failure_threshold: u32,
        reset_after: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        self.breaker = CircuitBreaker::with_clock(failure_threshold, reset_after, clock);
        self
    }
}

impl Default for AdminIntrospectClient {
    fn default() -> Self {
        Self::new(reqwest::Client::new(), "", "").with_breaker_clock(
            DEFAULT_BREAKER_FAILURE_THRESHOLD,
            DEFAULT_BREAKER_RESET,
            Arc::new(SystemClock),
        )
    }
}

#[async_trait]
impl AdminIntrospector for AdminIntrospectClient {
    async fn introspect(&self, token: &str) -> GuardResult<AdminIntrospection> {
        if !self.breaker.allow_request() {
            return Err(GuardError::unavailable(
                "admin IdP introspection is temporarily unavailable",
            ));
        }

        // Only the access token is ever sent — never a refresh token.
        let response = self
            .http
            .post(&self.introspect_url)
            .timeout(self.timeout)
            .header(&self.header_name, &self.api_key)
            .json(&serde_json::json!({ "token": token }))
            .send()
            .await
            .map_err(|e| {
                self.breaker.record_failure();
                tracing::warn!(error = %e, "admin IdP introspect call failed");
                GuardError::unavailable("unable to verify admin session with the IdP")
            })?;

        let status = response.status();
        if !status.is_success() {
            self.breaker.record_failure();
            tracing::warn!(
                status = status.as_u16(),
                "admin IdP introspect returned non-2xx"
            );
            return Err(GuardError::unavailable(format!(
                "admin IdP introspection returned {status}"
            )));
        }

        let body = response.json::<IntrospectionBody>().await.map_err(|e| {
            self.breaker.record_failure();
            tracing::warn!(error = %e, "admin IdP introspect response was malformed");
            GuardError::unavailable("admin IdP introspection response was malformed")
        })?;

        // A 2xx is the IdP working, whatever its verdict — recording
        // success here (rather than only on `active: true`) is what keeps a
        // stream of revoked tokens from tripping the breaker.
        self.breaker.record_success();
        Ok(body.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client_for(server: &MockServer) -> AdminIntrospectClient {
        AdminIntrospectClient::new(
            reqwest::Client::new(),
            format!("{}/api/v1/oauth/introspect", server.uri()),
            "introspect-key",
        )
        .with_timeout(Duration::from_secs(2))
    }

    #[tokio::test]
    async fn an_active_verdict_carries_the_live_role_and_grants() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/oauth/introspect"))
            .and(header("x-api-key", "introspect-key"))
            .and(body_json(json!({ "token": "tok" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "active": true,
                "adminId": "admin-1",
                "role": "admin",
                "siteAccess": { "maxion-game-back-office": ["launcher-coupons"] },
            })))
            .mount(&server)
            .await;

        let verdict = client_for(&server).await.introspect("tok").await.unwrap();
        match verdict {
            AdminIntrospection::Active {
                admin_id,
                role,
                site_access,
            } => {
                assert_eq!(admin_id, "admin-1");
                assert_eq!(role, "admin");
                assert_eq!(
                    site_access.get("maxion-game-back-office"),
                    Some(&vec!["launcher-coupons".to_string()])
                );
            }
            other => panic!("expected active, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_revoked_token_is_an_answer_not_an_outage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "active": false, "reason": "token_revoked" })),
            )
            .mount(&server)
            .await;

        let verdict = client_for(&server).await.introspect("tok").await.unwrap();
        assert_eq!(verdict, AdminIntrospection::inactive("token_revoked"));
    }

    #[tokio::test]
    async fn a_non_2xx_is_unavailable_rather_than_a_denial() {
        for status in [401u16, 403, 500, 502] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;

            let err = client_for(&server)
                .await
                .introspect("tok")
                .await
                .unwrap_err();
            assert!(
                matches!(err, GuardError::Unavailable(_)),
                "{status} should surface as unavailable, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_malformed_body_is_unavailable_rather_than_silently_active() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        assert!(matches!(
            client_for(&server).await.introspect("tok").await,
            Err(GuardError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn an_unreachable_idp_trips_the_breaker_and_keeps_failing_closed() {
        // Port 1 refuses immediately. Dropping a `MockServer` instead would
        // free its port for another test to bind and answer.
        let client = AdminIntrospectClient::new(
            reqwest::Client::new(),
            "http://127.0.0.1:1/api/v1/oauth/introspect",
            "introspect-key",
        )
        .with_timeout(Duration::from_millis(200));

        for _ in 0..DEFAULT_BREAKER_FAILURE_THRESHOLD + 2 {
            assert!(matches!(
                client.introspect("tok").await,
                Err(GuardError::Unavailable(_))
            ));
        }
    }

    #[tokio::test]
    async fn revoked_tokens_do_not_trip_the_breaker() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "active": false, "reason": "x" })),
            )
            .mount(&server)
            .await;

        let client = client_for(&server).await;
        for _ in 0..DEFAULT_BREAKER_FAILURE_THRESHOLD + 3 {
            assert!(client.introspect("tok").await.is_ok());
        }
    }

    #[tokio::test]
    async fn a_fake_clock_lets_the_breaker_half_open_without_sleeping() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let clock = Arc::new(crate::clock::FakeClock::new());
        let client = AdminIntrospectClient::new(
            reqwest::Client::new(),
            format!("{}/api/v1/oauth/introspect", server.uri()),
            "introspect-key",
        )
        .with_breaker_clock(1, Duration::from_secs(30), clock.clone());

        assert!(client.introspect("tok").await.is_err());
        assert!(
            !client.breaker.allow_request(),
            "one failure should open a threshold-1 breaker"
        );
        clock.advance(Duration::from_millis(30_001));
        assert!(client.breaker.allow_request(), "the window has passed");
    }
}
