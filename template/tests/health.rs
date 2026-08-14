//! `/healthz` must answer while the database is unreachable; `/readyz` must
//! not. Getting this backwards turns a database blip into an outage: the
//! liveness probe fails, the orchestrator restarts every healthy replica,
//! and the restarts hit the database that was already struggling.

mod common;

use common::{call, get, router_for};
use sqlx::PgPool;

/// A pool pointed at a port nothing is listening on. `connect_lazy` means
/// building it succeeds and only the first query fails — which is exactly
/// the situation these tests are about.
async fn dead_pool() -> PgPool {
    template_server::adapters::postgres::connect("postgres://postgres:postgres@127.0.0.1:1/x", 1)
        .await
        .expect("connect_lazy succeeds without dialling")
}

#[tokio::test]
async fn healthz_answers_200_while_the_database_is_unreachable() {
    let router = router_for(dead_pool().await, "http://127.0.0.1:1");
    let response = call(&router, get("/healthz")).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body["status"], "ok");
}

#[sqlx::test]
async fn readyz_is_ok_when_the_database_answers(pool: PgPool) {
    let router = router_for(pool, "http://127.0.0.1:1");
    let response = call(&router, get("/readyz")).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body["status"], "ready");
}

#[tokio::test]
async fn readyz_is_503_when_the_database_is_unreachable() {
    let router = router_for(dead_pool().await, "http://127.0.0.1:1");
    let response = call(&router, get("/readyz")).await;
    assert_eq!(response.status, 503);
    assert_eq!(response.body["dependency"], "postgres");
}
