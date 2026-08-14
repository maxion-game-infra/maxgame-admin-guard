//! The two example routes end to end: the public listing, the admin listing
//! (`page`/`take` + `ListMeta`), the feature gate, and the auth split
//! (`401` for a bad credential, `503` for an IdP outage — contract rule 5).

mod common;

use common::{
    call, get, get_as, mint_token, mock_idp, mount_active_introspection,
    mount_broken_introspection, post_as, router_for, state_for,
};
use serde_json::json;
use sqlx::PgPool;
use template_server::inbound::build_router;
use template_server::inbound::example::{EXAMPLE_FEATURE, EXAMPLE_SITE};

async fn seed(pool: &PgPool, name: &str, is_public: bool) {
    sqlx::query("INSERT INTO example_items (name, is_public) VALUES ($1, $2)")
        .bind(name)
        .bind(is_public)
        .execute(pool)
        .await
        .expect("seed insert");
}

fn grant() -> serde_json::Value {
    json!({ EXAMPLE_SITE: [EXAMPLE_FEATURE] })
}

#[sqlx::test]
async fn the_public_route_needs_no_token_and_lists_only_public_items(pool: PgPool) {
    seed(&pool, "public one", true).await;
    seed(&pool, "private one", false).await;

    let router = router_for(pool, "http://127.0.0.1:1");
    let response = call(&router, get("/example")).await;

    assert_eq!(response.status, 200);
    let items = response.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "public one");
    // camelCase on the wire, per PLATFORM.md's convention.
    assert!(items[0].get("isPublic").is_some());
}

#[sqlx::test]
async fn the_admin_route_needs_a_bearer_token(pool: PgPool) {
    let idp = mock_idp().await;
    let router = build_router(state_for(pool, &idp.uri()));

    let response = call(&router, get("/admin/example")).await;
    assert_eq!(response.status, 401);
}

#[sqlx::test]
async fn the_admin_route_refuses_a_caller_without_the_feature(pool: PgPool) {
    let idp = mock_idp().await;
    mount_active_introspection(&idp, "admin-1", "admin", json!({})).await;
    let router = build_router(state_for(pool, &idp.uri()));

    let token = mint_token("admin-1", "admin", json!({}));
    // GET does not introspect, so the token's own (empty) siteAccess is what
    // gets checked — no grant at all is refused before any feature check
    // (contract rule 6).
    let response = call(&router, get_as("/admin/example", &token)).await;
    assert_eq!(response.status, 403);
}

#[sqlx::test]
async fn an_admin_with_the_feature_lists_every_item_paginated(pool: PgPool) {
    for i in 0..13 {
        seed(&pool, &format!("item {i}"), i % 2 == 0).await;
    }
    let idp = mock_idp().await;
    let router = build_router(state_for(pool, &idp.uri()));
    let token = mint_token("admin-1", "admin", grant());

    let response = call(&router, get_as("/admin/example?page=2&take=5", &token)).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body["items"].as_array().unwrap().len(), 5);
    assert_eq!(response.body["meta"]["page"], 2);
    assert_eq!(response.body["meta"]["take"], 5);
    assert_eq!(response.body["meta"]["itemCount"], 13);
    assert_eq!(response.body["meta"]["pageCount"], 3);
    assert_eq!(response.body["meta"]["hasNextPage"], true);
    assert_eq!(response.body["meta"]["hasPreviousPage"], true);
}

#[sqlx::test]
async fn an_empty_admin_list_still_reports_one_page(pool: PgPool) {
    let idp = mock_idp().await;
    let router = build_router(state_for(pool, &idp.uri()));
    let token = mint_token("admin-1", "admin", grant());

    let response = call(&router, get_as("/admin/example", &token)).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body["meta"]["itemCount"], 0);
    assert_eq!(response.body["meta"]["pageCount"], 1);
}

#[sqlx::test]
async fn creating_an_item_is_a_mutation_and_pays_for_live_introspection(pool: PgPool) {
    let idp = mock_idp().await;
    mount_active_introspection(&idp, "admin-1", "admin", grant()).await;
    let router = build_router(state_for(pool.clone(), &idp.uri()));
    let token = mint_token("admin-1", "admin", grant());

    let response = call(
        &router,
        post_as("/admin/example", &token, json!({ "name": "new item" })),
    )
    .await;
    assert_eq!(response.status, 201);
    assert_eq!(response.body["name"], "new item");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM example_items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn a_mutation_from_a_deactivated_admin_is_401_not_a_stale_pass(pool: PgPool) {
    let idp = mock_idp().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(common::INTROSPECT_PATH))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "active": false,
            "reason": "token_revoked",
        })))
        .mount(&idp)
        .await;
    let router = build_router(state_for(pool, &idp.uri()));
    let token = mint_token("admin-1", "admin", grant());

    let response = call(
        &router,
        post_as("/admin/example", &token, json!({ "name": "x" })),
    )
    .await;
    assert_eq!(response.status, 401);
}

/// Contract rule 5: an IdP that cannot answer introspection is a 503, never
/// a 401 — the two mean different things, and only 503 should page anyone.
#[sqlx::test]
async fn a_mutation_the_idp_cannot_verify_is_503_not_401(pool: PgPool) {
    let idp = mock_idp().await;
    mount_broken_introspection(&idp).await;
    let router = build_router(state_for(pool, &idp.uri()));
    let token = mint_token("admin-1", "admin", grant());

    let response = call(
        &router,
        post_as("/admin/example", &token, json!({ "name": "x" })),
    )
    .await;
    assert_eq!(response.status, 503);
}

/// Same rule, offline side: an unreachable JWKS is 503, not 401, even on a
/// read that never reaches introspection.
#[sqlx::test]
async fn a_read_when_the_jwks_is_unreachable_is_503_not_401(pool: PgPool) {
    // No mock server at all — every fetch fails closed.
    let router = build_router(state_for(pool, "http://127.0.0.1:1"));
    let token = mint_token("admin-1", "admin", grant());

    let response = call(&router, get_as("/admin/example", &token)).await;
    assert_eq!(response.status, 503);
}
