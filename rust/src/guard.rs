//! The whole contract (`../contract/README.md`) in one place: given a
//! method, a bearer token, and the site/feature a route requires, decide
//! whether the request may proceed.
//!
//! This is the layer [`crate::axum_ext`] wraps for axum consumers and the
//! layer the contract test suite (`tests/contract.rs`) drives directly for
//! every case in `../contract/cases.json`. A host service that already has
//! its own middleware structure — as `maxgame-launcher-backend` does — is
//! free to use the lower-level pieces ([`crate::token::AdminTokenVerifier`],
//! [`crate::introspect_client::AdminIntrospectClient`],
//! [`crate::access::AdminIdentity`]) directly instead of this type.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::http::Method;

use crate::access::AdminIdentity;
use crate::error::{GuardError, GuardResult};
use crate::introspect::{AdminIntrospection, AdminIntrospector};
use crate::introspect_client::{
    AdminIntrospectClient, DEFAULT_BREAKER_FAILURE_THRESHOLD, DEFAULT_BREAKER_RESET,
    DEFAULT_HEADER_NAME, DEFAULT_TIMEOUT,
};
use crate::jwks::{AdminJwksClient, AdminJwksClientConfig, JwksKeyProvider};
use crate::token::AdminTokenVerifier;

/// Everything a guard needs, as explicit values. Built by the host service
/// from whatever it owns (env, a secrets manager, a config file) — this
/// crate never reads an environment variable itself, so a missing value
/// here is a decision the host made, not a default this crate guessed at.
#[derive(Debug, Clone)]
pub struct GuardConfig {
    /// Expected `iss` claim, byte-compared (contract rule 2).
    pub issuer: String,
    /// Absolute URL of the IdP's JWKS document.
    pub jwks_url: String,
    /// Absolute URL of the introspection endpoint.
    pub introspect_url: String,
    /// Header the introspect API key is sent under. `x-api-key` for every
    /// IdP this crate talks to today.
    pub introspect_header: String,
    pub introspect_api_key: String,
    /// Methods that pay for a live introspection call (contract rule 4).
    pub mutating_methods: HashSet<Method>,
    pub breaker_failure_threshold: u32,
    pub breaker_open: Duration,
    pub http_timeout: Duration,
}

impl GuardConfig {
    pub fn new(
        issuer: impl Into<String>,
        jwks_url: impl Into<String>,
        introspect_url: impl Into<String>,
        introspect_api_key: impl Into<String>,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            jwks_url: jwks_url.into(),
            introspect_url: introspect_url.into(),
            introspect_header: DEFAULT_HEADER_NAME.to_string(),
            introspect_api_key: introspect_api_key.into(),
            mutating_methods: default_mutating_methods(),
            breaker_failure_threshold: DEFAULT_BREAKER_FAILURE_THRESHOLD,
            breaker_open: DEFAULT_BREAKER_RESET,
            http_timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Convenience for the common case: one IdP base URL plus its two
    /// well-known relative paths (`jwksPath` / `introspectPath` in the
    /// contract's config), joined the same way every consumer does.
    pub fn from_base_url(
        issuer: impl Into<String>,
        idp_base_url: &str,
        jwks_path: &str,
        introspect_path: &str,
        introspect_api_key: impl Into<String>,
    ) -> Self {
        let base = idp_base_url.trim_end_matches('/');
        Self::new(
            issuer,
            format!("{base}{jwks_path}"),
            format!("{base}{introspect_path}"),
            introspect_api_key,
        )
    }

    pub fn with_introspect_header(mut self, header: impl Into<String>) -> Self {
        self.introspect_header = header.into();
        self
    }

    pub fn with_mutating_methods(mut self, methods: impl IntoIterator<Item = Method>) -> Self {
        self.mutating_methods = methods.into_iter().collect();
        self
    }

    pub fn with_breaker(mut self, failure_threshold: u32, open: Duration) -> Self {
        self.breaker_failure_threshold = failure_threshold;
        self.breaker_open = open;
        self
    }

    pub fn with_http_timeout(mut self, timeout: Duration) -> Self {
        self.http_timeout = timeout;
        self
    }

    pub fn is_mutating(&self, method: &Method) -> bool {
        self.mutating_methods.contains(method)
    }

    /// Contract rule 8: every value needed to reach a verdict must be
    /// present. A guard missing one refuses every request rather than
    /// treating "I cannot check" as "allow."
    pub fn is_configured(&self) -> bool {
        !self.issuer.trim().is_empty()
            && !self.jwks_url.trim().is_empty()
            && !self.introspect_url.trim().is_empty()
            && !self.introspect_api_key.trim().is_empty()
    }
}

fn default_mutating_methods() -> HashSet<Method> {
    [Method::POST, Method::PUT, Method::PATCH, Method::DELETE]
        .into_iter()
        .collect()
}

/// The site and feature a guarded route requires (contract rule 3). A route
/// mounted with no `Requirement` at all is a wiring bug, not "no feature
/// needed" — [`AdminGuard::authorize`] refuses it rather than letting it
/// fall through unguarded (contract case `missing-scope-metadata-refuses`).
#[derive(Debug, Clone)]
pub struct Requirement {
    pub site: String,
    pub feature: String,
}

impl Requirement {
    pub fn new(site: impl Into<String>, feature: impl Into<String>) -> Self {
        Self {
            site: site.into(),
            feature: feature.into(),
        }
    }
}

/// The outcome of a successful [`AdminGuard::authorize`] call.
#[derive(Debug, Clone)]
pub struct AuthorizedAdmin(pub AdminIdentity);

/// Wires [`AdminTokenVerifier`] and an [`AdminIntrospector`] together and
/// runs the eight rules in `../contract/README.md` in order.
pub struct AdminGuard {
    config: GuardConfig,
    verifier: AdminTokenVerifier,
    introspector: Arc<dyn AdminIntrospector>,
}

impl AdminGuard {
    /// Build with the real adapters: JWKS fetched and cached over `http`,
    /// live introspection posted over `http` behind a circuit breaker.
    pub fn new(config: GuardConfig, http: reqwest::Client) -> Self {
        let jwks_provider: Arc<dyn JwksKeyProvider> = Arc::new(AdminJwksClient::new(
            AdminJwksClientConfig::new(config.jwks_url.clone()),
            http.clone(),
        ));
        let verifier = AdminTokenVerifier::new(jwks_provider, config.issuer.clone());
        let introspector: Arc<dyn AdminIntrospector> = Arc::new(
            AdminIntrospectClient::new(
                http,
                config.introspect_url.clone(),
                config.introspect_api_key.clone(),
            )
            .with_header_name(config.introspect_header.clone())
            .with_timeout(config.http_timeout)
            .with_breaker(config.breaker_failure_threshold, config.breaker_open),
        );
        Self {
            config,
            verifier,
            introspector,
        }
    }

    /// Build with the ports supplied directly — for tests that need a fake
    /// clock behind the breaker, or a host that already has its own JWKS
    /// cache / introspection client and only wants the authorization rules.
    pub fn with_ports(
        config: GuardConfig,
        verifier: AdminTokenVerifier,
        introspector: Arc<dyn AdminIntrospector>,
    ) -> Self {
        Self {
            config,
            verifier,
            introspector,
        }
    }

    pub fn config(&self) -> &GuardConfig {
        &self.config
    }

    /// Run the eight rules in order:
    /// 1. Refuse outright if this guard is not configured (rule 8).
    /// 2. Require a bearer credential (rule 1).
    /// 3. Verify it offline against the JWKS (rule 2).
    /// 4. On a mutating method, replace role/grants with a live
    ///    introspection verdict (rule 4); an inactive verdict is 401, a
    ///    failure to obtain one at all is 503 (rule 5).
    /// 5. Refuse a route with no site/feature requirement wired at all.
    /// 6. Refuse a non-super-admin with empty `siteAccess` before any
    ///    feature check (rule 6).
    /// 7. Require the specific feature on the specific site (rule 3).
    pub async fn authorize(
        &self,
        method: &Method,
        bearer: Option<&str>,
        require: Option<&Requirement>,
    ) -> GuardResult<AuthorizedAdmin> {
        if !self.config.is_configured() {
            return Err(GuardError::unavailable(
                "admin auth guard is not configured (missing IdP base URL, issuer or introspect key)",
            ));
        }

        let token = bearer.ok_or_else(|| GuardError::unauthorized("Invalid token"))?;
        let verified = self.verifier.verify(token).await?;

        let mut identity = AdminIdentity {
            admin_id: verified.sub,
            role: verified.role,
            site_access: verified.site_access,
            token: token.to_string(),
        };

        if self.config.is_mutating(method) {
            // Errors here are `Unavailable` (503) and pass straight
            // through: the IdP being unreachable is not a claim about this
            // admin.
            match self.introspector.introspect(&identity.token).await? {
                AdminIntrospection::Active {
                    admin_id,
                    role,
                    site_access,
                } => {
                    identity.admin_id = admin_id;
                    identity.role = role;
                    identity.site_access = site_access;
                }
                AdminIntrospection::Inactive { reason } => {
                    return Err(GuardError::unauthorized(format!(
                        "Admin session is no longer active ({reason})"
                    )));
                }
            }
        }

        let Some(require) = require else {
            return Err(GuardError::forbidden(
                "Admin route is missing required site/feature metadata",
            ));
        };

        // Contract rule 6: refused here, before the feature check.
        if !identity.has_any_site_access() {
            return Err(GuardError::forbidden(
                "No access assigned. Contact your administrator.",
            ));
        }

        if !identity.has_feature(&require.site, &require.feature) {
            return Err(GuardError::forbidden(
                "Access denied. Required feature permission is missing.",
            ));
        }

        Ok(AuthorizedAdmin(identity))
    }
}
