//! The two routes this template ships, mounted by
//! [`crate::inbound::router::build_router`]:
//!
//! - `GET /example` — **public**, no admin token. Lists only
//!   `is_public = true` rows.
//! - `GET /admin/example` — **admin, feature-gated**. `page`/`take` +
//!   `{items, meta}` per `PLATFORM.md` §2.1 — copy this pagination shape
//!   for any new admin list endpoint.
//! - `POST /admin/example` — **admin, feature-gated**. A mutation, so the
//!   admin-guard middleware wired in `router.rs` runs live introspection on
//!   it, not just the offline JWKS check `GET /admin/example` gets.
//!
//! Replace `EXAMPLE_SITE`/`EXAMPLE_FEATURE` with a real site key from
//! `PLATFORM.md` §D6's path map and a real feature key from the IdP's
//! catalog (`GET /api/v1/sites`) — see the workspace `CLAUDE.md`'s
//! "Invariant สำคัญ" section on where that catalog actually lives.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use maxion_admin_guard::VerifiedAdmin;
use serde::{Deserialize, Serialize};

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::ExampleItem;
use crate::inbound::AppState;

/// Stand-in for a real site key — see the module doc comment.
pub const EXAMPLE_SITE: &str = "maxion-game-back-office";
/// Stand-in for a real feature key.
pub const EXAMPLE_FEATURE: &str = "example-management";

const DEFAULT_TAKE: i64 = 10;
const MAX_TAKE: i64 = 100;

pub fn public_routes() -> Router<AppState> {
    Router::new().route("/example", get(list_public))
}

pub fn admin_routes() -> Router<AppState> {
    Router::new().route("/admin/example", get(list_admin).post(create_admin))
}

// ── query / body ─────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct RawAdminListQuery {
    pub page: Option<String>,
    pub take: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminListQuery {
    pub page: i64,
    pub take: i64,
}

impl AdminListQuery {
    pub fn offset(&self) -> i64 {
        (self.page - 1) * self.take
    }
}

impl TryFrom<RawAdminListQuery> for AdminListQuery {
    type Error = DomainError;

    fn try_from(raw: RawAdminListQuery) -> DomainResult<Self> {
        let page = match raw.page {
            None => 1,
            Some(v) => bounded_number("page", &v, 1, None)?,
        };
        let take = match raw.take {
            None => DEFAULT_TAKE,
            Some(v) => bounded_number("take", &v, 1, Some(MAX_TAKE))?,
        };
        Ok(AdminListQuery { page, take })
    }
}

fn bounded_number(field: &str, raw: &str, min: i64, max: Option<i64>) -> DomainResult<i64> {
    let value: i64 = raw
        .parse()
        .map_err(|_| DomainError::BadRequest(format!("{field} must be a number")))?;
    if value < min {
        return Err(DomainError::BadRequest(format!(
            "{field} must not be less than {min}"
        )));
    }
    if let Some(max) = max {
        if value > max {
            return Err(DomainError::BadRequest(format!(
                "{field} must not be greater than {max}"
            )));
        }
    }
    Ok(value)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateExampleBody {
    pub name: String,
    #[serde(default)]
    pub is_public: bool,
}

// ── responses ─────────────────────────────────────────────────────────

/// `PLATFORM.md` §2.1's admin pagination envelope. Copy this struct
/// verbatim into any new admin list endpoint — the field names and
/// `page_count`'s `|| 1` on an empty result are the platform standard, not
/// a choice this service made.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListMeta {
    pub page: i64,
    pub take: i64,
    pub item_count: i64,
    pub page_count: i64,
    pub has_next_page: bool,
    pub has_previous_page: bool,
}

impl ListMeta {
    pub fn new(query: &AdminListQuery, item_count: i64) -> Self {
        let page_count = ((item_count + query.take - 1) / query.take).max(1);
        ListMeta {
            page: query.page,
            take: query.take,
            item_count,
            page_count,
            has_next_page: query.page < page_count,
            has_previous_page: query.page > 1,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AdminListResponse {
    pub items: Vec<ExampleItem>,
    pub meta: ListMeta,
}

/// Intentionally minimal — no established public-pagination shape exists
/// yet for a from-scratch service (`PLATFORM.md` §2.2). If this route grows
/// past a small dataset, follow whichever cursor pattern the rest of your
/// service settles on.
#[derive(Debug, Serialize)]
pub struct PublicListResponse {
    pub items: Vec<ExampleItem>,
}

// ── handlers ──────────────────────────────────────────────────────────

async fn list_public(State(state): State<AppState>) -> DomainResult<impl IntoResponse> {
    let items = state.example_repo.list(true, 100, 0).await?;
    Ok(Json(PublicListResponse { items }))
}

async fn list_admin(
    State(state): State<AppState>,
    Query(raw): Query<RawAdminListQuery>,
    _admin: VerifiedAdmin,
) -> DomainResult<impl IntoResponse> {
    let query = AdminListQuery::try_from(raw)?;
    let items = state
        .example_repo
        .list(false, query.take, query.offset())
        .await?;
    let item_count = state.example_repo.count(false).await?;
    Ok(Json(AdminListResponse {
        items,
        meta: ListMeta::new(&query, item_count),
    }))
}

async fn create_admin(
    State(state): State<AppState>,
    _admin: VerifiedAdmin,
    Json(body): Json<CreateExampleBody>,
) -> DomainResult<impl IntoResponse> {
    if body.name.trim().is_empty() {
        return Err(DomainError::BadRequest("name must not be empty".into()));
    }
    let item = state
        .example_repo
        .create(&body.name, body.is_public)
        .await?;
    Ok((StatusCode::CREATED, Json(item)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_query_is_the_first_page_of_ten() {
        let q = AdminListQuery::try_from(RawAdminListQuery::default()).unwrap();
        assert_eq!(q.page, 1);
        assert_eq!(q.take, 10);
        assert_eq!(q.offset(), 0);
    }

    #[test]
    fn take_is_clamped_and_bad_values_are_400() {
        assert!(AdminListQuery::try_from(RawAdminListQuery {
            page: None,
            take: Some("0".into()),
        })
        .is_err());
        assert!(AdminListQuery::try_from(RawAdminListQuery {
            page: None,
            take: Some("101".into()),
        })
        .is_err());
        assert!(AdminListQuery::try_from(RawAdminListQuery {
            page: Some("abc".into()),
            take: None,
        })
        .is_err());
    }

    #[test]
    fn an_empty_result_still_reports_one_page() {
        let query = AdminListQuery { page: 1, take: 10 };
        let meta = ListMeta::new(&query, 0);
        assert_eq!(meta.page_count, 1);
        assert!(!meta.has_next_page);
        assert!(!meta.has_previous_page);
    }

    #[test]
    fn page_count_rounds_up_and_next_previous_are_correct_mid_list() {
        let query = AdminListQuery { page: 2, take: 3 };
        let meta = ListMeta::new(&query, 13);
        assert_eq!(meta.page_count, 5);
        assert!(meta.has_next_page);
        assert!(meta.has_previous_page);
    }
}
