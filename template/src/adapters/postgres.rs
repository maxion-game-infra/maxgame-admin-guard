//! Postgres connection handling.
//!
//! Queries are written as runtime SQL (`sqlx::query_as::<_, T>(..)`), not
//! `sqlx::query!` macros, so the crate compiles without a live database or a
//! checked-in `.sqlx` cache. Correctness is covered by `#[sqlx::test]`
//! integration tests, which run every statement against a real Postgres.

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::domain::error::{DomainError, DomainResult};

/// Open the connection pool.
///
/// `connect_lazy` keeps boot from depending on the database being up;
/// `/readyz` is what reports real connectivity, so a database blip during a
/// rollout does not turn into a crash loop.
pub async fn connect(database_url: &str, max_connections: u32) -> DomainResult<PgPool> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect_lazy(database_url)
        .map_err(|e| DomainError::Internal(format!("cannot configure database pool: {e}")))
}

/// Apply every pending migration. Migrations are compiled into the binary
/// (`build.rs` + `sqlx::migrate!`), so a deployed image carries its own
/// schema.
pub async fn migrate(pool: &PgPool) -> DomainResult<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| DomainError::Internal(format!("migration failed: {e}")))
}
