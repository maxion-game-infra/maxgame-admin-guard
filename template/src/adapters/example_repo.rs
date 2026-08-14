//! Postgres access for [`crate::domain::ExampleItem`]. The layering
//! (`inbound` calls `adapters`, never raw SQL in a handler) is what
//! `tests/architecture_compliance.rs`-style checks in the fleet enforce —
//! copy that test alongside this file if the new service should keep it.

use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::error::DomainResult;
use crate::domain::ExampleItem;

#[derive(Clone)]
pub struct ExampleRepo {
    pool: PgPool,
}

impl ExampleRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// `public_only = true` is the public route; `false` is the admin one.
    pub async fn list(
        &self,
        public_only: bool,
        limit: i64,
        offset: i64,
    ) -> DomainResult<Vec<ExampleItem>> {
        let items = sqlx::query_as::<_, ExampleItem>(
            r#"
            SELECT id, name, is_public, created_at
            FROM example_items
            WHERE (NOT $1) OR is_public
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(public_only)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(items)
    }

    pub async fn count(&self, public_only: bool) -> DomainResult<i64> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM example_items WHERE (NOT $1) OR is_public")
                .bind(public_only)
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }

    pub async fn create(&self, name: &str, is_public: bool) -> DomainResult<ExampleItem> {
        let item = sqlx::query_as::<_, ExampleItem>(
            r#"
            INSERT INTO example_items (name, is_public)
            VALUES ($1, $2)
            RETURNING id, name, is_public, created_at
            "#,
        )
        .bind(name)
        .bind(is_public)
        .fetch_one(&self.pool)
        .await?;
        Ok(item)
    }

    pub async fn get(&self, id: Uuid) -> DomainResult<Option<ExampleItem>> {
        let item = sqlx::query_as::<_, ExampleItem>(
            "SELECT id, name, is_public, created_at FROM example_items WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(item)
    }
}
