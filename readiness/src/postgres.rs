//! Postgres probe and the dedicated health pool (`CONTRACT.md` §2, §6.1,
//! §6.2).
//!
//! A service's own Postgres is normally a **gating** check: it runs inline
//! on every `/readyz` request and its failure answers 503. §6.1 asks gating
//! checks to run on a dedicated one-connection pool, so a saturated traffic
//! pool cannot make `/readyz` hang. [`health_pool`] builds that pool, and the
//! runner's probe timeout (2 s by default) stays the outer bound:
//!
//! ```text
//! let options: PgConnectOptions = database_url.parse()?;
//! let readiness = Readiness::builder("launcher", app_env)
//!     .gate("postgres", PgProbe::new(health_pool(options)))
//!     .build();
//! ```

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;

use crate::model::{CheckStatus, ProbeOutcome};
use crate::probe::Probe;

/// How long [`health_pool`] waits for its one connection. Shorter than the
/// runner's 2 s probe timeout, so an exhausted pool reports `timeout` from
/// the pool rather than from the runner (`CONTRACT.md` §6.1).
const HEALTH_POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(1);

/// The dedicated one-connection pool a gating Postgres check runs on
/// (`CONTRACT.md` §6.1). Connects lazily, so building it never blocks boot
/// and a database that is down at boot shows up as a failing check, not a
/// crash.
pub fn health_pool(options: PgConnectOptions) -> PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .acquire_timeout(HEALTH_POOL_ACQUIRE_TIMEOUT)
        .connect_lazy_with(options)
}

/// `SELECT 1` on a Postgres pool (`CONTRACT.md` §6.2).
///
/// Pass the pool from [`health_pool`] when the check is gating. The probe
/// reports no `target`: the connection string carries a password
/// (`CONTRACT.md` §9).
pub struct PgProbe {
    pool: PgPool,
}

impl PgProbe {
    /// Probe `pool` with `SELECT 1`.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl Probe for PgProbe {
    fn run(&self) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
        Box::pin(async move {
            match sqlx::query_scalar::<_, i32>("SELECT 1")
                .fetch_one(&self.pool)
                .await
            {
                Ok(1) => ProbeOutcome::ok(),
                Ok(_) => ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body"),
                Err(err) => classify_sqlx_error(&err),
            }
        })
    }
}

/// Classify a failed Postgres check (`CONTRACT.md` §3.5, §3.6):
///
/// * the pool could not hand out a connection in time: `unreachable`,
///   `timeout`;
/// * a socket or TLS failure: `unreachable`, `connect` or `tls`;
/// * an unusable connection string: `misconfigured`, `invalid_url`;
/// * anything else, including an error the database returned:
///   `unreachable`, `query_failed`.
///
/// Pure. The error text is never copied into the outcome (`CONTRACT.md`
/// §9).
pub fn classify_sqlx_error(err: &sqlx::Error) -> ProbeOutcome {
    match err {
        sqlx::Error::PoolTimedOut => ProbeOutcome::fail(CheckStatus::Unreachable, "timeout"),
        sqlx::Error::Io(_) => ProbeOutcome::fail(CheckStatus::Unreachable, "connect"),
        sqlx::Error::Tls(_) => ProbeOutcome::fail(CheckStatus::Unreachable, "tls"),
        sqlx::Error::Configuration(_) => {
            ProbeOutcome::fail(CheckStatus::Misconfigured, "invalid_url")
        }
        _ => ProbeOutcome::fail(CheckStatus::Unreachable, "query_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(outcome: &ProbeOutcome) -> (CheckStatus, &str) {
        (outcome.status, outcome.reason.as_deref().unwrap_or(""))
    }

    #[test]
    fn classifies_sqlx_errors() {
        assert_eq!(
            reason(&classify_sqlx_error(&sqlx::Error::PoolTimedOut)),
            (CheckStatus::Unreachable, "timeout")
        );
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(
            reason(&classify_sqlx_error(&sqlx::Error::Io(io))),
            (CheckStatus::Unreachable, "connect")
        );
        assert_eq!(
            reason(&classify_sqlx_error(&sqlx::Error::Configuration(
                "bad dsn".into()
            ))),
            (CheckStatus::Misconfigured, "invalid_url")
        );
        assert_eq!(
            reason(&classify_sqlx_error(&sqlx::Error::RowNotFound)),
            (CheckStatus::Unreachable, "query_failed")
        );
    }
}
