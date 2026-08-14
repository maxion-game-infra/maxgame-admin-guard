pub mod error;
pub mod example;
pub mod health;
pub mod router;

use std::sync::Arc;

use maxion_admin_guard::{AdminGuard, GuardConfig};
use sqlx::PgPool;

use crate::adapters::example_repo::ExampleRepo;
use crate::config::Config;

pub use router::build_router;

/// Shared application state, cloned into every handler by axum's
/// `State` extractor. Every field is cheap to clone (`PgPool` is a
/// connection-pool handle, `Arc<T>` is a pointer) — clone this struct
/// freely.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    /// The whole admin-auth contract in one call — see
    /// `maxion_admin_guard::axum_ext::require_admin`, wired in `router.rs`.
    pub admin_guard: Arc<AdminGuard>,
    pub example_repo: ExampleRepo,
}

impl AppState {
    pub fn new(config: Arc<Config>, pool: PgPool, http: reqwest::Client) -> Self {
        // `Config` already resolved `ADMIN_JWKS_URL`'s and
        // `ADMIN_INTROSPECT_PATH`'s defaults into full URLs (`PLATFORM.md`
        // §3.3), so `GuardConfig::new` is built from those directly rather
        // than through `from_base_url`, which would resolve them a second
        // time.
        let guard_config = GuardConfig::new(
            config.admin_idp.issuer.clone(),
            config.admin_idp.jwks_url.clone(),
            config.admin_idp.introspect_url.clone(),
            config.admin_idp.introspect_api_key.clone(),
        );

        AppState {
            example_repo: ExampleRepo::new(pool.clone()),
            pool,
            admin_guard: Arc::new(AdminGuard::new(guard_config, http)),
            config,
        }
    }
}
