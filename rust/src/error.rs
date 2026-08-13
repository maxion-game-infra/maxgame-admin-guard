//! The single error type this crate returns.
//!
//! The four variants mirror the status-code distinctions the contract
//! (`../contract/README.md`, rule 5) requires call sites to preserve:
//! `Unauthorized` is "this credential is not usable" (401), `Forbidden` is
//! "this admin may not do this" (403), `Unavailable` is "we could not get a
//! verdict at all" (503), and `Internal` is everything else (500). A host
//! service that already has its own error enum (as `web-platform-backend`
//! and `maxgame-launcher-backend` do) is expected to convert this one into
//! its own with a `From` impl rather than use [`IntoResponse`] directly;
//! `IntoResponse` exists for the crate's own axum middleware and for a
//! consumer with no error type of its own yet.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum GuardError {
    /// Caller is unauthenticated or presented an unusable credential (401).
    #[error("{0}")]
    Unauthorized(String),

    /// Caller is authenticated but not permitted (403).
    #[error("{0}")]
    Forbidden(String),

    /// A dependency needed to reach a verdict at all is unreachable (503).
    /// Distinct from [`GuardError::Unauthorized`] on purpose: the IdP being
    /// down is not the same claim as "this admin is denied."
    #[error("{0}")]
    Unavailable(String),

    /// Anything else, including a guard that is missing its own
    /// configuration (contract rule 8: refuse, do not allow).
    #[error("{0}")]
    Internal(String),
}

impl GuardError {
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        GuardError::Unauthorized(msg.into())
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        GuardError::Forbidden(msg.into())
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        GuardError::Unavailable(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        GuardError::Internal(msg.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            GuardError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            GuardError::Forbidden(_) => StatusCode::FORBIDDEN,
            GuardError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            GuardError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for GuardError {
    fn into_response(self) -> Response {
        let status = self.status();
        let message = self.to_string();
        (status, axum::Json(json!({ "message": message }))).into_response()
    }
}

pub type GuardResult<T> = Result<T, GuardError>;
