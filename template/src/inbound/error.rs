//! One place where domain failures become HTTP responses.
//!
//! The body shape is `PLATFORM.md` §1.1's platform-wide envelope:
//! `{"statusCode": 403, "message": "...", "error": "Forbidden"}`. Every
//! admin-facing service on the platform answers failures this way (`code`
//! below is additive — see §1.1) so a client written against one service
//! reads every other service's errors the same way.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::domain::error::DomainError;

/// The failure body every error route answers with.
///
/// A type rather than a `json!` literal, so nothing can drift between what
/// this comment claims and what the impl below actually writes.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    #[serde(rename = "statusCode")]
    pub status_code: u16,
    /// Human-readable detail. Never carries internal detail: a 500 always
    /// reads `Internal server error`.
    pub message: String,
    /// The status's reason phrase (`PLATFORM.md` §1.2) — a client should
    /// branch on `code` below if this service adds one, not on this field
    /// or on `message`.
    pub error: &'static str,
}

impl DomainError {
    pub fn status(&self) -> StatusCode {
        match self {
            DomainError::BadRequest(_) => StatusCode::BAD_REQUEST,
            DomainError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            DomainError::Forbidden(_) => StatusCode::FORBIDDEN,
            DomainError::NotFound(_) => StatusCode::NOT_FOUND,
            DomainError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            DomainError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for DomainError {
    fn into_response(self) -> Response {
        let status = self.status();

        // Internal detail can name a table, a column, or a connection
        // string. It belongs in the log, not in the response.
        let message = match &self {
            DomainError::Internal(detail) => {
                tracing::error!(error = %detail, "request failed");
                "Internal server error".to_string()
            }
            other => other.to_string(),
        };

        let body = Json(ErrorBody {
            status_code: status.as_u16(),
            message,
            error: status.canonical_reason().unwrap_or("Error"),
        });

        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn body_of(err: DomainError) -> (StatusCode, serde_json::Value) {
        let response = err.into_response();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn every_variant_answers_the_platform_envelope() {
        let cases = [
            (DomainError::BadRequest("x".into()), 400, "Bad Request"),
            (DomainError::Unauthorized("x".into()), 401, "Unauthorized"),
            (DomainError::Forbidden("x".into()), 403, "Forbidden"),
            (DomainError::NotFound("x".into()), 404, "Not Found"),
            (
                DomainError::Unavailable("x".into()),
                503,
                "Service Unavailable",
            ),
            (
                DomainError::Internal("x".into()),
                500,
                "Internal Server Error",
            ),
        ];
        for (err, status, reason) in cases {
            let (actual, body) = body_of(err).await;
            assert_eq!(actual.as_u16(), status);
            assert_eq!(body["statusCode"], status);
            assert_eq!(body["error"], reason);
            assert!(body["message"].is_string());
        }
    }

    #[tokio::test]
    async fn a_500_never_leaks_its_internal_detail() {
        let (_, body) = body_of(DomainError::Internal(
            "database error: relation \"secret_table\" does not exist".into(),
        ))
        .await;
        assert_eq!(body["message"], "Internal server error");
    }

    #[tokio::test]
    async fn a_503_does_say_what_is_unavailable() {
        let (_, body) = body_of(DomainError::Unavailable(
            "unable to verify admin session with the IdP".into(),
        ))
        .await;
        assert_eq!(
            body["message"],
            "unable to verify admin session with the IdP"
        );
    }

    #[tokio::test]
    async fn a_guard_error_keeps_its_status_on_the_way_through() {
        for (guard, status) in [
            (maxion_admin_guard::GuardError::unauthorized("no"), 401),
            (maxion_admin_guard::GuardError::forbidden("no"), 403),
            (maxion_admin_guard::GuardError::unavailable("down"), 503),
        ] {
            let (actual, _) = body_of(DomainError::from(guard)).await;
            assert_eq!(actual.as_u16(), status);
        }
    }
}
