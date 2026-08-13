//! Axum integration: a middleware-producing function plus an extractor, for
//! a consumer that wants the whole [`AdminGuard`] contract wired into a
//! router with no code of its own.
//!
//! `maxgame-launcher-backend` does not use this module — it already had its
//! own two-layer middleware structure (verify, then a per-feature check)
//! before this crate existed, with its own message text its tests assert
//! on, so it keeps assembling [`crate::token::AdminTokenVerifier`] and
//! [`crate::introspect_client::AdminIntrospectClient`] itself. This module
//! is for a service standing up admin auth for the first time (the
//! `maxgame-key-server` / `maxgame-auth-server` direct-connect work this
//! crate exists for).
//!
//! ```ignore
//! let guard = Arc::new(AdminGuard::new(config, http_client));
//! let router = Router::new()
//!     .route("/admin/games", get(list_games).post(create_game))
//!     .route_layer(from_fn(require_admin(guard, "maxion-game-back-office", "launcher-games")));
//!
//! async fn list_games(VerifiedAdmin(admin): VerifiedAdmin) -> Json<..> { .. }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Request};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;

use crate::access::AdminIdentity;
use crate::error::GuardError;
use crate::guard::{AdminGuard, Requirement};

/// What the middleware this module produces returns.
pub type GuardFuture = Pin<Box<dyn Future<Output = Result<Response, GuardError>> + Send>>;

/// The bearer token from an `Authorization` header, if there is one.
///
/// Split on `"Bearer "` rather than parsed strictly: a client that sends
/// the scheme in another case is rejected either way, just one step later
/// (offline verification fails to find a usable token at all).
pub fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once("Bearer "))
        .map(|(_, token)| token.trim())
        .filter(|token| !token.is_empty())
}

/// Build middleware that runs the whole [`AdminGuard`] contract for `site` +
/// `feature`, and on success inserts the verified [`AdminIdentity`] into the
/// request's extensions for [`VerifiedAdmin`] to pull back out.
///
/// Returned as a plain function so it can be handed to
/// [`axum::middleware::from_fn`] once per route (or route subtree) that
/// needs `feature`.
pub fn require_admin(
    guard: Arc<AdminGuard>,
    site: impl Into<String>,
    feature: impl Into<String>,
) -> impl Fn(Request, Next) -> GuardFuture + Clone + Send + Sync + 'static {
    let requirement = Requirement::new(site, feature);
    move |request: Request, next: Next| {
        let guard = guard.clone();
        let requirement = requirement.clone();
        Box::pin(async move {
            let method = request.method().clone();
            let bearer = bearer_token(request.headers()).map(str::to_string);
            let authorized = guard
                .authorize(&method, bearer.as_deref(), Some(&requirement))
                .await?;

            let mut request = request;
            request.extensions_mut().insert(authorized.0);
            Ok(next.run(request).await)
        })
    }
}

/// The verified admin, for handlers that need to know who is calling.
/// Only populated on a route mounted behind [`require_admin`].
#[derive(Debug, Clone)]
pub struct VerifiedAdmin(pub AdminIdentity);

impl<S: Send + Sync> FromRequestParts<S> for VerifiedAdmin {
    type Rejection = GuardError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AdminIdentity>()
            .cloned()
            .map(VerifiedAdmin)
            .ok_or_else(|| {
                GuardError::unauthorized(
                    "admin identity is missing; this route is not behind require_admin",
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn the_bearer_token_is_read_out_of_the_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer abc.def.ghi"),
        );
        assert_eq!(bearer_token(&headers), Some("abc.def.ghi"));

        for absent in ["", "Bearer ", "Bearer    ", "abc.def.ghi", "Basic abc"] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", HeaderValue::from_str(absent).unwrap());
            assert_eq!(bearer_token(&headers), None, "{absent:?}");
        }
        assert_eq!(bearer_token(&HeaderMap::new()), None);
    }
}
