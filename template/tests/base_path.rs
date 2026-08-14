//! `PLATFORM.md` §5.2 — booting with `BASE_PATH` set must move every route
//! under the prefix except `/healthz`/`/readyz`, which a k8s probe hits
//! directly and so must always answer at the root.

mod common;

use common::{call, get, state_with_env};
use sqlx::PgPool;
use template_server::inbound::build_router;

async fn status(pool: PgPool, base_path: &str, uri: &str) -> u16 {
    let state = state_with_env(pool, "http://127.0.0.1:1", &[("BASE_PATH", base_path)]);
    call(&build_router(state), get(uri)).await.status.as_u16()
}

#[sqlx::test]
async fn healthz_and_readyz_stay_at_the_root_regardless_of_base_path(pool: PgPool) {
    assert_eq!(status(pool.clone(), "/x", "/healthz").await, 200);
    assert_eq!(status(pool, "/x", "/readyz").await, 200);
}

#[sqlx::test]
async fn the_public_route_moves_under_base_path(pool: PgPool) {
    assert_eq!(status(pool.clone(), "/x", "/x/example").await, 200);
    // Reachable but unauthenticated on the admin side proves the route
    // exists under the prefix rather than having vanished — 401, not 404.
    assert_eq!(status(pool, "/x", "/x/admin/example").await, 401);
}

#[sqlx::test]
async fn the_unprefixed_path_is_gone_once_base_path_is_set(pool: PgPool) {
    for uri in ["/example", "/admin/example"] {
        assert_eq!(
            status(pool.clone(), "/x", uri).await,
            404,
            "{uri} should not answer once BASE_PATH is set"
        );
    }
}

#[sqlx::test]
async fn an_unset_base_path_is_behaviourally_identical_to_today(pool: PgPool) {
    assert_eq!(status(pool.clone(), "", "/healthz").await, 200);
    assert_eq!(status(pool.clone(), "", "/example").await, 200);
    assert_eq!(status(pool, "", "/admin/example").await, 401);
}
