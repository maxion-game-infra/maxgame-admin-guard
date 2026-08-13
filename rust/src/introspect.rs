//! The admin IdP's answer to "is this token still good, and what does this
//! admin hold right now?" (contract rule 4).
//!
//! Wire shape: the subject is `adminId`, not `sub`, and the grants are
//! `siteAccess` — see `$claimNotes` in `../contract/cases.json`.

use async_trait::async_trait;
use serde::Deserialize;

use crate::access::{normalize_site_access, SiteAccess};
use crate::error::GuardResult;

/// Parsed introspection verdict. Only [`AdminIntrospection::Active`] lets a
/// write through; `Inactive` is a legitimate answer and becomes a 401.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminIntrospection {
    Active {
        admin_id: String,
        role: String,
        site_access: SiteAccess,
    },
    Inactive {
        reason: String,
    },
}

impl AdminIntrospection {
    pub fn inactive(reason: impl Into<String>) -> Self {
        AdminIntrospection::Inactive {
            reason: reason.into(),
        }
    }
}

/// Raw introspection body. Every field is optional so a response that is
/// merely *missing* something is read as inactive rather than failing the
/// parse — a parse failure would surface as 503 ("IdP is broken") when the
/// honest reading is 401 ("this token is not usable").
#[derive(Debug, Deserialize)]
pub struct IntrospectionBody {
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default, rename = "adminId")]
    pub admin_id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default, rename = "siteAccess")]
    pub site_access: Option<serde_json::Value>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl From<IntrospectionBody> for AdminIntrospection {
    fn from(body: IntrospectionBody) -> Self {
        if body.active != Some(true) {
            return AdminIntrospection::inactive(
                body.reason.unwrap_or_else(|| "inactive".to_string()),
            );
        }
        // `active: true` with no subject is a malformed answer. Reading it
        // as active would authorize a write against an admin we cannot name
        // in the audit trail.
        let Some(admin_id) = body.admin_id.filter(|id| !id.trim().is_empty()) else {
            return AdminIntrospection::inactive("malformed");
        };
        AdminIntrospection::Active {
            admin_id,
            role: body.role.unwrap_or_default(),
            site_access: body
                .site_access
                .as_ref()
                .map(normalize_site_access)
                .unwrap_or_default(),
        }
    }
}

/// Layer 2 of the admin auth model: asks the IdP whether a token is still
/// alive, and what the admin's grants are *right now*.
///
/// Fail-closed is structural here. `Ok(_)` means the IdP answered —
/// including [`AdminIntrospection::Inactive`], which is a real answer and
/// becomes a 401. `Err` means we could not get an answer at all (transport,
/// non-2xx, open breaker) and becomes a 503: "the IdP is down" is not the
/// same claim as "this admin is denied."
#[async_trait]
pub trait AdminIntrospector: Send + Sync {
    async fn introspect(&self, token: &str) -> GuardResult<AdminIntrospection>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> AdminIntrospection {
        serde_json::from_value::<IntrospectionBody>(value)
            .unwrap()
            .into()
    }

    #[test]
    fn an_active_verdict_carries_the_live_grants() {
        let verdict = parse(json!({
            "active": true,
            "adminId": "admin-1",
            "username": "someone",
            "role": "admin",
            "siteAccess": { "maxion-game-back-office": ["launcher-games"] },
            "tokenVersion": 3,
        }));
        match verdict {
            AdminIntrospection::Active {
                admin_id,
                role,
                site_access,
            } => {
                assert_eq!(admin_id, "admin-1");
                assert_eq!(role, "admin");
                assert_eq!(
                    site_access.get("maxion-game-back-office"),
                    Some(&vec!["launcher-games".to_string()])
                );
            }
            other => panic!("expected active, got {other:?}"),
        }
    }

    #[test]
    fn every_shape_that_is_not_plainly_active_is_inactive() {
        for body in [
            json!({ "active": false, "reason": "token_revoked" }),
            json!({ "active": null }),
            json!({}),
            // active but unnamed: cannot be attributed, so not usable.
            json!({ "active": true, "role": "super_admin" }),
            json!({ "active": true, "adminId": "  " }),
        ] {
            assert!(
                matches!(parse(body.clone()), AdminIntrospection::Inactive { .. }),
                "{body} must read as inactive"
            );
        }
    }

    #[test]
    fn the_reason_is_preserved_for_the_log_line() {
        assert_eq!(
            parse(json!({ "active": false, "reason": "token_revoked" })),
            AdminIntrospection::inactive("token_revoked")
        );
        assert_eq!(
            parse(json!({ "active": false })),
            AdminIntrospection::inactive("inactive")
        );
    }
}
