//! Step-up — the email second factor's service half (lean ROADMAP §S).
//!
//! A critical route opts in with one call, in one of two modes:
//! - **session** — [`require_step_up`]: the caller's live introspection
//!   carried a non-null `stepUpElevatedUntil`
//!   ([`crate::introspect::AdminIntrospector::introspect_with_step_up`]);
//! - **single-use** — [`require_single_use_step_up`]: the request carries
//!   `X-Step-Up-Token`, and the auth server spends it ([`StepUpConsumer`])
//!   for this admin, action and resource. Call it BEFORE the action: a
//!   token is spent whether or not what follows succeeds.
//!
//! Either refusal is [`StepUpError::Required`] → `403` with
//! `{"error":"step_up_required","mode","action","resource","detail"}` (the
//! step-up wire contract, lean ROADMAP §S). A consume the auth server did
//! not answer is [`StepUpError::Unavailable`] → `503`.

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::time::Duration;

use crate::error::{GuardError, GuardResult};
use crate::introspect_client::{DEFAULT_HEADER_NAME, DEFAULT_TIMEOUT};

/// The request header a single-use token rides in.
pub const STEP_UP_TOKEN_HEADER: &str = "x-step-up-token";

/// The auth server's consume endpoint, relative to its base URL.
pub const CONSUME_PATH: &str = "/v1/external/step-up/consume";

/// The auth server's fixed action list (`maxgame-admin-auth-server`
/// `src/domain/step_up.rs` `STEP_UP_ACTIONS`). A single-use token can only
/// be issued for one of these.
pub mod action {
    pub const SIGNER_APPROVE: &str = "signer.approve";
    pub const SIGNER_RETIRE: &str = "signer.retire";
    pub const SAFE_RETIRE: &str = "safe.retire";
    pub const CUSTODY_DESTROY: &str = "custody.destroy";
    pub const ADDRESS_BOOK_CHANGE: &str = "address_book.change";
    pub const ROLE_GRANT: &str = "role.grant";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepUpMode {
    Session,
    SingleUse,
}

impl StepUpMode {
    pub fn as_str(self) -> &'static str {
        match self {
            StepUpMode::Session => "session",
            StepUpMode::SingleUse => "single_use",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StepUpError {
    /// `403 step_up_required`.
    #[error("step-up required ({}) for {action} on {resource}: {detail}", mode.as_str())]
    Required {
        mode: StepUpMode,
        action: String,
        resource: String,
        detail: String,
    },
    /// `503 step_up_unavailable`: the consume call got no answer it could
    /// read, so nothing was authorised.
    #[error("{0}")]
    Unavailable(String),
}

impl StepUpError {
    fn required(mode: StepUpMode, action: &str, resource: &str, detail: &str) -> Self {
        StepUpError::Required {
            mode,
            action: action.to_string(),
            resource: resource.to_string(),
            detail: detail.to_string(),
        }
    }
}

impl IntoResponse for StepUpError {
    fn into_response(self) -> Response {
        match self {
            StepUpError::Required {
                mode,
                action,
                resource,
                detail,
            } => (
                StatusCode::FORBIDDEN,
                axum::Json(json!({
                    "error": "step_up_required",
                    "mode": mode.as_str(),
                    "action": action,
                    "resource": resource,
                    "detail": detail,
                })),
            )
                .into_response(),
            StepUpError::Unavailable(detail) => (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({ "error": "step_up_unavailable", "detail": detail })),
            )
                .into_response(),
        }
    }
}

/// Session mode. `elevated_until` is what the caller's live introspection
/// answered ([`crate::introspect::AdminIntrospector::introspect_with_step_up`]).
/// Passes on any `Some`; no clock is compared here. The auth server answers
/// `null` once the window has passed (`maxgame-admin-auth-server`
/// `src/inbound/oauth/introspect.rs`, the `stepUpElevatedUntil` field doc:
/// "A consumer treats non-null as "elevated" and compares no clocks of its
/// own"), so the expiry is the auth server's to decide.
pub fn require_step_up(
    elevated_until: Option<i64>,
    action: &str,
    resource: &str,
) -> Result<(), StepUpError> {
    match elevated_until {
        Some(_) => Ok(()),
        None => Err(StepUpError::required(
            StepUpMode::Session,
            action,
            resource,
            "this action needs a step-up — confirm with an emailed code",
        )),
    }
}

/// The auth server's answer to "spend this token".
#[async_trait]
pub trait StepUpConsumer: Send + Sync {
    /// `Ok(true)`: spent, for exactly these four values. `Ok(false)`: the
    /// auth server refused it (409 — unknown, spent, expired, or a
    /// mismatch). `Err`: no answer that could be read.
    async fn consume(
        &self,
        step_up_token: &str,
        admin_id: &str,
        action: &str,
        resource: &str,
    ) -> GuardResult<bool>;
}

/// Single-use mode. `admin_id` MUST be the one the caller's own live
/// introspection answered — never a value from the request. The token is
/// read from [`STEP_UP_TOKEN_HEADER`] and spent here.
pub async fn require_single_use_step_up(
    consumer: &dyn StepUpConsumer,
    headers: &HeaderMap,
    admin_id: &str,
    action: &str,
    resource: &str,
) -> Result<(), StepUpError> {
    let token = headers
        .get(STEP_UP_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let Some(token) = token else {
        return Err(StepUpError::required(
            StepUpMode::SingleUse,
            action,
            resource,
            "this action is irreversible and needs its own emailed code",
        ));
    };
    match consumer.consume(token, admin_id, action, resource).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(StepUpError::required(
            StepUpMode::SingleUse,
            action,
            resource,
            "token rejected or spent",
        )),
        Err(e) => Err(StepUpError::Unavailable(format!(
            "cannot spend the step-up token right now: {e}"
        ))),
    }
}

/// `POST {auth server}/v1/external/step-up/consume`, authenticated with the
/// same `x-api-key` credential as introspection.
pub struct StepUpConsumeClient {
    http: reqwest::Client,
    consume_url: String,
    header_name: String,
    api_key: String,
    timeout: Duration,
}

impl StepUpConsumeClient {
    pub fn new(
        http: reqwest::Client,
        consume_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            http,
            consume_url: consume_url.into(),
            header_name: DEFAULT_HEADER_NAME.to_string(),
            api_key: api_key.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_header_name(mut self, header_name: impl Into<String>) -> Self {
        self.header_name = header_name.into();
        self
    }
}

#[async_trait]
impl StepUpConsumer for StepUpConsumeClient {
    async fn consume(
        &self,
        step_up_token: &str,
        admin_id: &str,
        action: &str,
        resource: &str,
    ) -> GuardResult<bool> {
        let response = self
            .http
            .post(&self.consume_url)
            .timeout(self.timeout)
            .header(&self.header_name, &self.api_key)
            .json(&json!({
                "step_up_token": step_up_token,
                "admin_id": admin_id,
                "action": action,
                "resource": resource,
            }))
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "step-up consume call failed");
                GuardError::unavailable("unable to reach the auth server's step-up consume")
            })?;

        let status = response.status();
        if status == StatusCode::CONFLICT {
            return Ok(false);
        }
        if !status.is_success() {
            tracing::warn!(status = status.as_u16(), "step-up consume returned non-2xx");
            return Err(GuardError::unavailable(format!(
                "step-up consume returned {status}"
            )));
        }
        // 200 must say so: anything else is not an answer we can act on.
        let body: serde_json::Value = response.json().await.map_err(|e| {
            tracing::warn!(error = %e, "step-up consume response was malformed");
            GuardError::unavailable("step-up consume response was malformed")
        })?;
        if body["consumed"] == json!(true) {
            Ok(true)
        } else {
            Err(GuardError::unavailable(
                "step-up consume answered 2xx without consumed:true",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const NOW: i64 = 1_800_000_000;

    async fn body_of(err: StepUpError) -> (StatusCode, serde_json::Value) {
        let response = err.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn no_elevation_is_403_step_up_required_with_the_exact_body() {
        let err = require_step_up(None, action::SIGNER_APPROVE, "sig-1").unwrap_err();
        let (status, body) = body_of(err).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            json!({
                "error": "step_up_required",
                "mode": "session",
                "action": "signer.approve",
                "resource": "sig-1",
                "detail": "this action needs a step-up — confirm with an emailed code",
            })
        );
    }

    /// A window that has passed reaches the guard as the auth server's
    /// `null`, and is refused like no elevation at all.
    #[tokio::test]
    async fn an_elapsed_window_answered_as_null_is_refused() {
        let err = require_step_up(None, action::SIGNER_RETIRE, "s").unwrap_err();
        let (status, body) = body_of(err).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "step_up_required");
        assert_eq!(body["mode"], "session");
    }

    /// No clock of our own: any non-null value passes, even one that reads
    /// as long past here — the auth server already decided it is live.
    #[test]
    fn any_non_null_elevation_passes_with_no_clock_comparison() {
        for until in [NOW + 1, NOW, NOW - 1, 0, i64::MIN] {
            assert!(
                require_step_up(Some(until), action::SIGNER_APPROVE, "s").is_ok(),
                "{until}"
            );
        }
    }

    /// Records every call; answers from a fixed script.
    struct FakeConsumer {
        answer: fn() -> GuardResult<bool>,
        calls: Mutex<Vec<(String, String, String, String)>>,
    }

    impl FakeConsumer {
        fn new(answer: fn() -> GuardResult<bool>) -> Self {
            Self {
                answer,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl StepUpConsumer for FakeConsumer {
        async fn consume(
            &self,
            token: &str,
            admin_id: &str,
            action: &str,
            resource: &str,
        ) -> GuardResult<bool> {
            self.calls.lock().unwrap().push((
                token.into(),
                admin_id.into(),
                action.into(),
                resource.into(),
            ));
            (self.answer)()
        }
    }

    fn with_token(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(STEP_UP_TOKEN_HEADER, token.parse().unwrap());
        h
    }

    #[tokio::test]
    async fn single_use_without_the_header_is_403_single_use_and_spends_nothing() {
        let consumer = FakeConsumer::new(|| Ok(true));
        for headers in [HeaderMap::new(), with_token("   ")] {
            let err = require_single_use_step_up(
                &consumer,
                &headers,
                "admin-1",
                action::CUSTODY_DESTROY,
                "sig-1",
            )
            .await
            .unwrap_err();
            let (status, body) = body_of(err).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(
                body,
                json!({
                    "error": "step_up_required",
                    "mode": "single_use",
                    "action": "custody.destroy",
                    "resource": "sig-1",
                    "detail": "this action is irreversible and needs its own emailed code",
                })
            );
        }
        assert!(consumer.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_accepted_token_passes_and_is_spent_once_for_the_given_admin() {
        let consumer = FakeConsumer::new(|| Ok(true));
        require_single_use_step_up(
            &consumer,
            &with_token("tok-1"),
            "admin-1",
            action::CUSTODY_DESTROY,
            "sig-1",
        )
        .await
        .unwrap();
        assert_eq!(
            *consumer.calls.lock().unwrap(),
            vec![(
                "tok-1".to_string(),
                "admin-1".to_string(),
                "custody.destroy".to_string(),
                "sig-1".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn a_refused_token_is_403_single_use_token_rejected_or_spent() {
        let consumer = FakeConsumer::new(|| Ok(false));
        let err = require_single_use_step_up(
            &consumer,
            &with_token("tok-1"),
            "admin-1",
            action::CUSTODY_DESTROY,
            "sig-1",
        )
        .await
        .unwrap_err();
        let (status, body) = body_of(err).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "step_up_required");
        assert_eq!(body["mode"], "single_use");
        assert_eq!(body["detail"], "token rejected or spent");
    }

    #[tokio::test]
    async fn a_consume_with_no_answer_is_503_fail_closed() {
        let consumer = FakeConsumer::new(|| Err(GuardError::unavailable("down")));
        let err = require_single_use_step_up(
            &consumer,
            &with_token("tok-1"),
            "admin-1",
            action::CUSTODY_DESTROY,
            "sig-1",
        )
        .await
        .unwrap_err();
        let (status, body) = body_of(err).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "step_up_unavailable");
    }

    // ── the HTTP client ────────────────────────────────────────────────────

    fn client_for(server: &MockServer) -> StepUpConsumeClient {
        StepUpConsumeClient::new(
            reqwest::Client::new(),
            format!("{}{CONSUME_PATH}", server.uri()),
            "introspect-key",
        )
        .with_timeout(Duration::from_secs(2))
    }

    #[tokio::test]
    async fn the_client_posts_the_four_values_with_the_introspect_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(CONSUME_PATH))
            .and(header("x-api-key", "introspect-key"))
            .and(body_json(json!({
                "step_up_token": "tok-1",
                "admin_id": "admin-1",
                "action": "custody.destroy",
                "resource": "sig-1",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "consumed": true })))
            .expect(1)
            .mount(&server)
            .await;
        assert!(client_for(&server)
            .consume("tok-1", "admin-1", "custody.destroy", "sig-1")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_409_is_a_refusal_not_an_outage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(json!({ "error": "step_up_token_rejected", "message": "x" })),
            )
            .mount(&server)
            .await;
        assert!(!client_for(&server)
            .consume("tok-1", "admin-1", "custody.destroy", "sig-1")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn any_other_answer_is_unavailable() {
        for (status, body) in [
            (401u16, json!({ "error": "unauthorized" })),
            (422, json!("bad")),
            (500, json!({})),
            (200, json!({ "consumed": false })),
            (200, json!({})),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body.clone()))
                .mount(&server)
                .await;
            let err = client_for(&server)
                .consume("tok-1", "admin-1", "custody.destroy", "sig-1")
                .await
                .unwrap_err();
            assert!(
                matches!(err, GuardError::Unavailable(_)),
                "{status} {body}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_auth_server_is_unavailable() {
        // Port 1 refuses immediately (see introspect_client's own test).
        let client = StepUpConsumeClient::new(
            reqwest::Client::new(),
            format!("http://127.0.0.1:1{CONSUME_PATH}"),
            "introspect-key",
        )
        .with_timeout(Duration::from_millis(200));
        assert!(matches!(
            client
                .consume("tok-1", "admin-1", "custody.destroy", "sig-1")
                .await,
            Err(GuardError::Unavailable(_))
        ));
    }
}
