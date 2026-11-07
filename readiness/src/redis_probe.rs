//! Redis `PING` probe (`CONTRACT.md` §6.2).
//!
//! Works with any async connection the service already holds, such as
//! `redis::aio::ConnectionManager` or `redis::aio::MultiplexedConnection`.
//! Whether it is gating or reporting is the service's call (`CONTRACT.md`
//! §2): gate only when every route needs Redis.

use std::future::Future;
use std::pin::Pin;

use redis::aio::ConnectionLike;

use crate::model::{CheckStatus, ProbeOutcome};
use crate::probe::Probe;

/// `PING` on a Redis connection; `PONG` is `ok` (`CONTRACT.md` §6.2).
///
/// The probe reports no `target`: the Redis URL can carry a password
/// (`CONTRACT.md` §9).
pub struct RedisProbe<C> {
    conn: C,
}

impl<C> RedisProbe<C>
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    /// Probe `conn` with `PING`. The connection is cloned per run, which is
    /// cheap for the multiplexed connection types.
    pub fn new(conn: C) -> Self {
        Self { conn }
    }
}

impl<C> Probe for RedisProbe<C>
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    fn run(&self) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
        Box::pin(async move {
            let mut conn = self.conn.clone();
            match redis::cmd("PING").query_async::<String>(&mut conn).await {
                Ok(reply) if reply == "PONG" => ProbeOutcome::ok(),
                Ok(_) => ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body"),
                Err(err) => classify_redis_error(&err),
            }
        })
    }
}

/// Classify a failed Redis check (`CONTRACT.md` §3.5, §3.6):
///
/// * a timeout: `unreachable`, `timeout`;
/// * a refused, dropped or failed socket: `unreachable`, `connect`;
/// * Redis refused our authentication: `unexpected`, `unexpected_status`.
///   Probes do not check credentials (`CONTRACT.md` §1.1), but Redis
///   authenticates at connect time and that step cannot be skipped. The
///   connection did reach a Redis, which is why this is `unexpected` and not
///   `unreachable`;
/// * anything else: `unreachable`, `query_failed`.
///
/// Pure. The error text is never copied into the outcome (`CONTRACT.md`
/// §9).
pub fn classify_redis_error(err: &redis::RedisError) -> ProbeOutcome {
    if err.is_timeout() {
        return ProbeOutcome::fail(CheckStatus::Unreachable, "timeout");
    }
    if err.is_connection_refusal() || err.is_connection_dropped() || err.is_io_error() {
        return ProbeOutcome::fail(CheckStatus::Unreachable, "connect");
    }
    if err.kind() == redis::ErrorKind::AuthenticationFailed {
        return ProbeOutcome::fail(CheckStatus::Unexpected, "unexpected_status");
    }
    ProbeOutcome::fail(CheckStatus::Unreachable, "query_failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(outcome: &ProbeOutcome) -> (CheckStatus, &str) {
        (outcome.status, outcome.reason.as_deref().unwrap_or(""))
    }

    #[test]
    fn classifies_redis_errors() {
        let refused = redis::RedisError::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert_eq!(
            reason(&classify_redis_error(&refused)),
            (CheckStatus::Unreachable, "connect")
        );
        let timed_out = redis::RedisError::from(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        ));
        assert_eq!(
            reason(&classify_redis_error(&timed_out)),
            (CheckStatus::Unreachable, "timeout")
        );
        let auth: redis::RedisError =
            (redis::ErrorKind::AuthenticationFailed, "auth failed").into();
        assert_eq!(
            reason(&classify_redis_error(&auth)),
            (CheckStatus::Unexpected, "unexpected_status")
        );
        let other: redis::RedisError = (redis::ErrorKind::UnexpectedReturnType, "type").into();
        assert_eq!(
            reason(&classify_redis_error(&other)),
            (CheckStatus::Unreachable, "query_failed")
        );
    }
}
