//! Admin identity and the site/feature authorization rules (contract rules
//! 3 and 6, and the shape-normalisation section of `../contract/README.md`).
//!
//! Ported from `web-platform-backend`'s `src/libs/admin-access/admin-access.ts`
//! and reproduced identically in `maxgame-launcher-backend`'s
//! `src/domain/admin.rs` and `maxgame-key-server`'s `src/infrastructure/idp_jwt.rs`
//! before this crate existed. A host service's own site/feature constants
//! (e.g. launcher's `MAXION_GAME_SITE` and `features::LAUNCHER_GAMES`) are
//! not this crate's concern — they are plain strings to it.

use std::collections::HashMap;

/// Role value carried by the `role` claim for an unrestricted admin.
pub const ROLE_SUPER_ADMIN: &str = "super_admin";

/// `siteAccess`: site id → the feature keys granted on that site.
pub type SiteAccess = HashMap<String, Vec<String>>;

/// A verified admin, as a host service's handlers see them.
#[derive(Debug, Clone)]
pub struct AdminIdentity {
    /// `sub` (or the introspection verdict's `adminId`) — the IdP admin id.
    pub admin_id: String,
    /// `role` — `"super_admin" | "admin"`.
    pub role: String,
    /// Normalized `siteAccess`. On a write this is the *live* map from
    /// introspection, not the (up to token-lifetime stale) one in the token.
    pub site_access: SiteAccess,
    /// The raw bearer token, kept so a handler that must call another
    /// service on the admin's behalf can forward it.
    pub token: String,
}

impl AdminIdentity {
    pub fn is_super_admin(&self) -> bool {
        self.role == ROLE_SUPER_ADMIN
    }

    /// Whether this admin may reach a route scoped to `site` with no
    /// particular feature required.
    pub fn has_site(&self, site: &str) -> bool {
        self.is_super_admin() || self.site_access.contains_key(site)
    }

    /// Whether this admin holds `feature` on `site`. Super admins bypass.
    pub fn has_feature(&self, site: &str, feature: &str) -> bool {
        if self.is_super_admin() {
            return true;
        }
        self.site_access
            .get(site)
            .is_some_and(|granted| granted.iter().any(|key| key == feature))
    }

    /// Contract rule 6: a non-super-admin with an empty `siteAccess` has
    /// been granted nothing and is refused before any feature check runs.
    pub fn has_any_site_access(&self) -> bool {
        self.is_super_admin() || !self.site_access.is_empty()
    }
}

/// Normalize the `siteAccess` claim into `site → sorted, deduped features`.
///
/// Accepts both shapes the platform has minted (`../contract/README.md`,
/// "Shape normalisation"): the current `{ site: ["feature", …] }` and the
/// legacy `{ site: { feature: "edit" | "readonly" } }`, where the *keys* are
/// the grants. Anything else for a site (a string, a number, null)
/// contributes no entry at all rather than an empty one.
pub fn normalize_site_access(raw: &serde_json::Value) -> SiteAccess {
    let mut out = SiteAccess::new();
    let Some(map) = raw.as_object() else {
        return out;
    };

    for (site, value) in map {
        let mut features: Vec<String> = match value {
            serde_json::Value::Array(items) => items
                .iter()
                .map(|item| match item {
                    // `map(String)` in JS stringifies non-strings rather
                    // than dropping them; a numeric grant is nonsense either
                    // way, but every implementation must agree on what it
                    // becomes.
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
            serde_json::Value::Object(entries) => entries.keys().cloned().collect(),
            _ => continue,
        };
        features.sort();
        features.dedup();
        out.insert(site.clone(), features);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn admin(role: &str, access: serde_json::Value) -> AdminIdentity {
        AdminIdentity {
            admin_id: "admin-1".into(),
            role: role.into(),
            site_access: normalize_site_access(&access),
            token: "token".into(),
        }
    }

    #[test]
    fn super_admin_bypasses_every_site_and_feature_check() {
        let sa = admin(ROLE_SUPER_ADMIN, json!({}));
        assert!(sa.has_any_site_access());
        assert!(sa.has_site("zone4-back-office"));
        assert!(sa.has_feature("zone4-back-office", "anything"));
    }

    #[test]
    fn a_plain_admin_needs_the_exact_feature_on_the_exact_site() {
        let a = admin(
            "admin",
            json!({ "zone4-back-office": ["presale-stock", "news-management"] }),
        );
        assert!(a.has_feature("zone4-back-office", "presale-stock"));
        assert!(a.has_feature("zone4-back-office", "news-management"));
        assert!(!a.has_feature("zone4-back-office", "presale-export"));

        // The same key granted on a different site does not carry over.
        let elsewhere = admin("admin", json!({ "mu-back-office": ["presale-stock"] }));
        assert!(!elsewhere.has_feature("zone4-back-office", "presale-stock"));
        assert!(!elsewhere.has_site("zone4-back-office"));
    }

    #[test]
    fn an_admin_with_no_grants_at_all_is_refused_before_any_feature_check() {
        assert!(!admin("admin", json!({})).has_any_site_access());
        assert!(admin("admin", json!({ "zone4-back-office": [] })).has_any_site_access());
    }

    #[test]
    fn legacy_object_grants_are_read_as_their_keys() {
        let legacy = admin(
            "admin",
            json!({ "zone4-back-office": { "news-management": "edit", "presale-stock": "readonly" } }),
        );
        assert!(legacy.has_feature("zone4-back-office", "news-management"));
        assert!(legacy.has_feature("zone4-back-office", "presale-stock"));
        assert!(!legacy.has_feature("zone4-back-office", "presale-export"));
    }

    #[test]
    fn features_are_sorted_and_deduped_and_junk_sites_are_dropped() {
        let access = normalize_site_access(&json!({
            "a": ["b", "a", "b"],
            "b": "not-a-grant",
            "c": null,
        }));
        assert_eq!(
            access.get("a"),
            Some(&vec!["a".to_string(), "b".to_string()])
        );
        assert!(!access.contains_key("b"));
        assert!(!access.contains_key("c"));
    }

    #[test]
    fn a_non_object_site_access_claim_grants_nothing() {
        assert!(normalize_site_access(&json!(null)).is_empty());
        assert!(normalize_site_access(&json!("super_admin")).is_empty());
        assert!(normalize_site_access(&json!([1, 2])).is_empty());
    }
}
