//! Layer 1 of the admin auth model: offline EdDSA verification of an
//! IdP-issued admin access token against the IdP's published JWKS
//! (contract rule 2).
//!
//! Offline *in the steady state*: the key provider caches the JWKS and only
//! makes a round trip on a cache miss (cold start, TTL expiry, unrecognized
//! `kid`).
//!
//! Strictness, all deliberate:
//! - `alg` MUST be `EdDSA`. HS256/RS256/`none` are refused before the
//!   signature is looked at, so a token signed with the *public* key as an
//!   HMAC secret cannot be talked into verifying.
//! - `kid` is REQUIRED — it is how the key is selected.
//! - `exp` is REQUIRED and must be in the future.
//! - `iss` must byte-equal the configured issuer.
//! - `type` must be `admin`.
//! - `aud` is NOT checked (contract rule 2) — a verifier must not start
//!   requiring it until the audience rollout says so.
//! - `verify_strict` rejects non-canonical signature encodings.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::Signature;
use serde::Deserialize;

use crate::access::{normalize_site_access, SiteAccess};
use crate::error::{GuardError, GuardResult};
use crate::jwks::JwksKeyProvider;

/// The only algorithm the IdP mints admin tokens with.
const EXPECTED_ALG: &str = "EdDSA";

/// The `type` claim value that marks an admin access token, as opposed to a
/// refresh token or a player token.
const EXPECTED_TOKEN_TYPE: &str = "admin";

/// Claims taken off a verified admin access token.
#[derive(Debug, Clone)]
pub struct VerifiedAdminToken {
    pub sub: String,
    pub role: String,
    pub site_access: SiteAccess,
}

#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

#[derive(Deserialize)]
struct JwtPayload {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default, rename = "type")]
    token_type: Option<String>,
    #[serde(default, rename = "siteAccess")]
    site_access: Option<serde_json::Value>,
}

/// JWKS-backed EdDSA verifier. Cheap to clone; the cache lives in the key
/// provider behind an `Arc`.
#[derive(Clone)]
pub struct AdminTokenVerifier {
    key_provider: Arc<dyn JwksKeyProvider>,
    expected_issuer: String,
}

impl std::fmt::Debug for AdminTokenVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminTokenVerifier")
            .field("expected_issuer", &self.expected_issuer)
            .finish_non_exhaustive()
    }
}

impl AdminTokenVerifier {
    pub fn new(key_provider: Arc<dyn JwksKeyProvider>, expected_issuer: impl Into<String>) -> Self {
        Self {
            key_provider,
            expected_issuer: expected_issuer.into(),
        }
    }

    /// Verify and decode. Every rejection is `Unauthorized` with a specific
    /// message, including a JWKS transport failure: layer 1 runs on reads
    /// too, and "we cannot check this token" has to mean "do not trust it".
    pub async fn verify(&self, token: &str) -> GuardResult<VerifiedAdminToken> {
        let (header_b64, payload_b64, signature_b64) = split_jwt(token)?;

        let header: JwtHeader = decode_json(header_b64)?;
        if header.alg != EXPECTED_ALG {
            return Err(GuardError::unauthorized(format!(
                "JWT alg '{}' rejected (expected {EXPECTED_ALG})",
                header.alg
            )));
        }
        let kid = header
            .kid
            .ok_or_else(|| GuardError::unauthorized("JWT missing `kid` header"))?;

        let verifying_key = self.key_provider.resolve_key(&kid).await?;

        let signature_bytes = decode_base64url(signature_b64)?;
        let signature_bytes: [u8; ed25519_dalek::SIGNATURE_LENGTH] = signature_bytes
            .as_slice()
            .try_into()
            .map_err(|_| GuardError::unauthorized("JWT signature shape invalid"))?;
        let signature = Signature::from_bytes(&signature_bytes);

        let signing_input = format!("{header_b64}.{payload_b64}");
        verifying_key
            .verify_strict(signing_input.as_bytes(), &signature)
            .map_err(|_| GuardError::unauthorized("JWT signature verification failed"))?;

        let payload: JwtPayload = decode_json(payload_b64)?;

        if payload.token_type.as_deref() != Some(EXPECTED_TOKEN_TYPE) {
            return Err(GuardError::unauthorized(format!(
                "JWT type claim is {:?} (expected {EXPECTED_TOKEN_TYPE:?})",
                payload.token_type
            )));
        }

        match payload.iss.as_deref() {
            Some(iss) if iss == self.expected_issuer => {}
            Some(iss) => {
                return Err(GuardError::unauthorized(format!(
                    "JWT iss '{iss}' does not match expected '{}'",
                    self.expected_issuer
                )))
            }
            None => return Err(GuardError::unauthorized("JWT missing `iss` claim")),
        }

        let exp = payload
            .exp
            .ok_or_else(|| GuardError::unauthorized("JWT missing `exp` claim"))?;
        let now = now_ts();
        if exp <= now {
            return Err(GuardError::unauthorized(format!(
                "JWT expired (exp={exp}, now={now})"
            )));
        }

        let sub = payload
            .sub
            .filter(|sub| !sub.trim().is_empty())
            .ok_or_else(|| GuardError::unauthorized("JWT missing `sub` claim"))?;

        Ok(VerifiedAdminToken {
            sub,
            role: payload.role.unwrap_or_default(),
            site_access: payload
                .site_access
                .as_ref()
                .map(normalize_site_access)
                .unwrap_or_default(),
        })
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64
}

fn split_jwt(token: &str) -> GuardResult<(&str, &str, &str)> {
    let mut parts = token.trim().split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) if !h.is_empty() && !p.is_empty() && !s.is_empty() => {
            Ok((h, p, s))
        }
        _ => Err(GuardError::unauthorized(
            "JWT is not a three-part compact token",
        )),
    }
}

fn decode_base64url(segment: &str) -> GuardResult<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| GuardError::unauthorized("JWT segment is not valid base64url"))
}

fn decode_json<T: for<'de> Deserialize<'de>>(segment: &str) -> GuardResult<T> {
    let bytes = decode_base64url(segment)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| GuardError::unauthorized(format!("JWT segment is not valid JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
    use serde_json::json;

    struct StaticKeys {
        key: Option<VerifyingKey>,
    }

    #[async_trait]
    impl JwksKeyProvider for StaticKeys {
        async fn resolve_key(&self, kid: &str) -> GuardResult<VerifyingKey> {
            self.key
                .ok_or_else(|| GuardError::unauthorized(format!("unknown kid '{kid}'")))
        }
    }

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn verifier(key: Option<VerifyingKey>) -> AdminTokenVerifier {
        AdminTokenVerifier::new(Arc::new(StaticKeys { key }), "maxion-platform.maxion.game")
    }

    fn encode(part: &serde_json::Value) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(part).unwrap())
    }

    /// A token signed by `signing_key()`, with overridable header/payload so
    /// each test can break exactly one thing.
    fn token(header: serde_json::Value, payload: serde_json::Value) -> String {
        let key = signing_key();
        let signing_input = format!("{}.{}", encode(&header), encode(&payload));
        let signature = key.sign(signing_input.as_bytes());
        format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }

    fn good_header() -> serde_json::Value {
        json!({ "alg": "EdDSA", "typ": "JWT", "kid": "ed-20260811" })
    }

    fn good_payload() -> serde_json::Value {
        json!({
            "sub": "admin-1",
            "iss": "maxion-platform.maxion.game",
            "exp": now_ts() + 900,
            "role": "admin",
            "type": "admin",
            "siteAccess": { "maxion-game-back-office": ["launcher-games"] },
        })
    }

    #[tokio::test]
    async fn a_well_formed_token_verifies_and_yields_its_claims() {
        let claims = verifier(Some(signing_key().verifying_key()))
            .verify(&token(good_header(), good_payload()))
            .await
            .unwrap();
        assert_eq!(claims.sub, "admin-1");
        assert_eq!(claims.role, "admin");
        assert_eq!(
            claims.site_access.get("maxion-game-back-office"),
            Some(&vec!["launcher-games".to_string()])
        );
    }

    #[tokio::test]
    async fn a_token_with_no_aud_is_accepted() {
        // Real admin tokens carry no `aud`; enforcing one would reject every
        // token in production. This is the regression guard for that.
        let mut payload = good_payload();
        payload.as_object_mut().unwrap().remove("aud");
        assert!(verifier(Some(signing_key().verifying_key()))
            .verify(&token(good_header(), payload))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_token_with_an_aud_present_is_also_accepted() {
        let mut payload = good_payload();
        payload["aud"] = json!("some-audience");
        assert!(verifier(Some(signing_key().verifying_key()))
            .verify(&token(good_header(), payload))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn alg_is_pinned_to_eddsa() {
        for alg in ["HS256", "RS256", "none", "eddsa"] {
            let mut header = good_header();
            header["alg"] = json!(alg);
            let err = verifier(Some(signing_key().verifying_key()))
                .verify(&token(header, good_payload()))
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("alg"),
                "{alg} should be refused on the alg check, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn a_token_signed_by_another_key_is_refused() {
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let err = verifier(Some(other.verifying_key()))
            .verify(&token(good_header(), good_payload()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("signature"));
    }

    #[tokio::test]
    async fn a_tampered_payload_is_refused() {
        let raw = token(good_header(), good_payload());
        let mut parts: Vec<&str> = raw.split('.').collect();
        let forged = encode(&json!({
            "sub": "admin-1", "iss": "maxion-platform.maxion.game",
            "exp": now_ts() + 900,
            "role": "super_admin", "type": "admin", "siteAccess": {},
        }));
        parts[1] = &forged;
        let err = verifier(Some(signing_key().verifying_key()))
            .verify(&parts.join("."))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("signature"));
    }

    #[tokio::test]
    async fn issuer_type_expiry_and_subject_are_all_required() {
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (json!({ "iss": "someone-else" }), "iss"),
            (json!({ "iss": null }), "iss"),
            (json!({ "type": "refresh" }), "type"),
            (json!({ "type": null }), "type"),
            (json!({ "exp": now_ts() - 1 }), "expired"),
            (json!({ "exp": null }), "exp"),
            (json!({ "sub": null }), "sub"),
            (json!({ "sub": "   " }), "sub"),
        ];
        for (patch, expected) in cases {
            let mut payload = good_payload();
            for (key, value) in patch.as_object().unwrap() {
                if value.is_null() {
                    payload.as_object_mut().unwrap().remove(key);
                } else {
                    payload[key] = value.clone();
                }
            }
            let err = verifier(Some(signing_key().verifying_key()))
                .verify(&token(good_header(), payload))
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "expected the error to mention {expected}, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn a_missing_kid_is_refused_before_the_key_lookup() {
        let mut header = good_header();
        header.as_object_mut().unwrap().remove("kid");
        let err = verifier(Some(signing_key().verifying_key()))
            .verify(&token(header, good_payload()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("kid"));
    }

    #[tokio::test]
    async fn an_unresolvable_kid_fails_closed_rather_than_skipping_the_check() {
        let err = verifier(None)
            .verify(&token(good_header(), good_payload()))
            .await
            .unwrap_err();
        assert!(matches!(err, GuardError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn malformed_tokens_are_refused_without_panicking() {
        let v = verifier(Some(signing_key().verifying_key()));
        for bad in ["", "not-a-jwt", "a.b", "a.b.c.d", "a.b.c", "..", "x..y"] {
            assert!(v.verify(bad).await.is_err(), "{bad:?} must be refused");
        }
    }
}
