pub mod error;

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

/// Stand-in domain type behind both routes this template ships. Replace it
/// (and `adapters::example_repo`, `inbound::example`) with a real one —
/// nothing else in the template depends on its shape.
#[derive(Debug, Clone, sqlx::FromRow, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExampleItem {
    pub id: Uuid,
    pub name: String,
    pub is_public: bool,
    pub created_at: DateTime<Utc>,
}
