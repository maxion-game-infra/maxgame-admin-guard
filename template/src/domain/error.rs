//! The single error type crossing every boundary in this service.
//!
//! Handlers map these onto HTTP status codes and the platform's envelope in
//! one place (`inbound::error`), so nothing below the router needs to know
//! about axum or `PLATFORM.md` §1.1's wire shape.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
    /// Caller sent something structurally wrong (400).
    #[error("{0}")]
    BadRequest(String),

    /// Caller is unauthenticated or presented an unusable credential (401).
    #[error("{0}")]
    Unauthorized(String),

    /// Caller is authenticated but not permitted (403).
    #[error("{0}")]
    Forbidden(String),

    /// Target does not exist (404).
    #[error("{0}")]
    NotFound(String),

    /// A dependency needed to answer at all is unreachable (503).
    ///
    /// Distinct from [`DomainError::Internal`] on purpose: the IdP being
    /// down is not a claim about the caller's credential, and a client
    /// should retry a 503 where it would report a 500 as a bug.
    #[error("{0}")]
    Unavailable(String),

    /// Anything the caller cannot fix (500).
    #[error("{0}")]
    Internal(String),
}

impl DomainError {
    pub fn internal(msg: impl Into<String>) -> Self {
        DomainError::Internal(msg.into())
    }
}

/// `maxion-admin-guard`'s error carries the same four-way status split this
/// type does (401/403/503/500) — see its `GuardError` doc — so the
/// conversion is a straight relabelling with no message rewriting.
impl From<maxion_admin_guard::GuardError> for DomainError {
    fn from(err: maxion_admin_guard::GuardError) -> Self {
        match err {
            maxion_admin_guard::GuardError::Unauthorized(msg) => DomainError::Unauthorized(msg),
            maxion_admin_guard::GuardError::Forbidden(msg) => DomainError::Forbidden(msg),
            maxion_admin_guard::GuardError::Unavailable(msg) => DomainError::Unavailable(msg),
            maxion_admin_guard::GuardError::Internal(msg) => DomainError::Internal(msg),
        }
    }
}

impl From<sqlx::Error> for DomainError {
    fn from(err: sqlx::Error) -> Self {
        match err {
            sqlx::Error::RowNotFound => DomainError::NotFound("record not found".into()),
            other => DomainError::Internal(format!("database error: {other}")),
        }
    }
}

pub type DomainResult<T> = Result<T, DomainError>;
