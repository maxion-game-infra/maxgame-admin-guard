//! Datastore probes (`postgres` and `redis` features).
//!
//! The live cases run only when `READINESS_TEST_DATABASE_URL` or
//! `READINESS_TEST_REDIS_URL` is set; otherwise they print why and return.

#![cfg(any(feature = "postgres", feature = "redis"))]

#[cfg(feature = "postgres")]
mod postgres {
    use std::time::{Duration, Instant};

    use maxgame_readiness::{health_pool, PgProbe};
    use maxgame_readiness::{AppEnv, CheckStatus, Probe, Readiness};
    use sqlx::postgres::PgConnectOptions;

    fn unreachable_options() -> PgConnectOptions {
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(1)
            .username("nobody")
            .database("nothing")
    }

    #[tokio::test]
    async fn unreachable_database_is_unreachable() {
        let outcome = PgProbe::new(health_pool(unreachable_options())).run().await;
        assert_eq!(outcome.status, CheckStatus::Unreachable);
        let reason = outcome.reason.as_deref().unwrap_or("");
        assert!(
            matches!(reason, "connect" | "timeout"),
            "unexpected reason {reason}"
        );
    }

    #[tokio::test]
    async fn unreachable_gating_database_answers_503_quickly() {
        let readiness = Readiness::builder("launcher", AppEnv::Development)
            .gate("postgres", PgProbe::new(health_pool(unreachable_options())))
            .probe_timeout(Duration::from_secs(2))
            .build();

        let started = Instant::now();
        let (code, body) = readiness.body().await;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(code.as_u16(), 503);
        assert_eq!(body["status"], "unavailable");
        assert_eq!(body["dependency"], "postgres");
        assert!(body["checks"]["postgres"].get("target").is_none());
    }

    #[tokio::test]
    async fn live_database_is_ok() {
        let Ok(url) = std::env::var("READINESS_TEST_DATABASE_URL") else {
            eprintln!("skipped: READINESS_TEST_DATABASE_URL unset");
            return;
        };
        let options: PgConnectOptions = url.parse().expect("valid database url");
        let outcome = PgProbe::new(health_pool(options)).run().await;
        assert_eq!(outcome.status, CheckStatus::Ok, "{outcome:?}");
    }
}

#[cfg(feature = "redis")]
mod redis_live {
    use maxgame_readiness::RedisProbe;
    use maxgame_readiness::{CheckStatus, Probe};

    #[tokio::test]
    async fn live_redis_is_ok() {
        let Ok(url) = std::env::var("READINESS_TEST_REDIS_URL") else {
            eprintln!("skipped: READINESS_TEST_REDIS_URL unset");
            return;
        };
        let client = redis::Client::open(url).expect("valid redis url");
        let conn = client
            .get_connection_manager()
            .await
            .expect("redis reachable");
        let outcome = RedisProbe::new(conn).run().await;
        assert_eq!(outcome.status, CheckStatus::Ok, "{outcome:?}");
    }

    #[tokio::test]
    async fn redis_that_went_away_is_unreachable() {
        let Ok(url) = std::env::var("READINESS_TEST_REDIS_URL") else {
            eprintln!("skipped: READINESS_TEST_REDIS_URL unset");
            return;
        };
        let client = redis::Client::open(url).expect("valid redis url");
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .expect("redis reachable");
        // Kill our own connection from the server side, then probe it.
        let _: Result<(), _> = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("SKIPME")
            .arg("no")
            .query_async(&mut conn)
            .await;
        let outcome = RedisProbe::new(conn).run().await;
        assert_eq!(outcome.status, CheckStatus::Unreachable, "{outcome:?}");
    }
}
