//! Wire values of one check: status and identity (`CONTRACT.md` §3.4–§3.6).
//!
//! A probe sends no credential and never reports whether one is valid
//! (`CONTRACT.md` §1.1). What is checked about a credential is static, at
//! boot, and lives outside this crate.

use serde::{Deserialize, Serialize};

/// Status of one hop (`CONTRACT.md` §3.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Reached at the configured address, and the response had the expected
    /// shape and identity.
    Ok,
    /// Not checked yet since boot.
    Pending,
    /// The last result is too old to trust (`CONTRACT.md` §6.1).
    Stale,
    /// An optional hop with its env unset or set to `off`.
    NotConfigured,
    /// The configured value is unusable before any network call.
    Misconfigured,
    /// Network error, DNS failure, TLS failure, timeout, or a 5xx.
    Unreachable,
    /// The upstream answered, but it is not the expected service or env.
    WrongTarget,
    /// The upstream answered 2xx or 4xx with a body that is not the expected
    /// shape.
    Unexpected,
}

impl CheckStatus {
    /// The wire spelling (`CONTRACT.md` §3.5).
    pub fn as_str(self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Pending => "pending",
            CheckStatus::Stale => "stale",
            CheckStatus::NotConfigured => "not_configured",
            CheckStatus::Misconfigured => "misconfigured",
            CheckStatus::Unreachable => "unreachable",
            CheckStatus::WrongTarget => "wrong_target",
            CheckStatus::Unexpected => "unexpected",
        }
    }

    /// The statuses that are a failure of the hop rather than a state. These
    /// carry a `reason` (`CONTRACT.md` §3.4) and are never `ok`.
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            CheckStatus::Misconfigured
                | CheckStatus::Unreachable
                | CheckStatus::WrongTarget
                | CheckStatus::Unexpected
        )
    }

    /// Fallback `reason` when a probe returns a failure status without one.
    /// The body must always carry a `reason` for a failure, so the status
    /// code itself is used. Services may add codes (`CONTRACT.md` §3.6), so
    /// this stays schema-valid.
    pub(crate) fn fallback_reason(self) -> Option<&'static str> {
        match self {
            CheckStatus::Misconfigured => Some("misconfigured"),
            CheckStatus::Unreachable => Some("unreachable"),
            CheckStatus::WrongTarget => Some("wrong_target"),
            CheckStatus::Unexpected => Some("unexpected"),
            _ => None,
        }
    }
}

/// The identity a hop is expected to have, and the identity it answered with
/// (`CONTRACT.md` §3.4, §5). Used for `wrong_target`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    /// The callee's service key (`PLATFORM.md` §5.1), e.g. `keyServer`.
    pub service: String,
    /// The callee's tier, e.g. `development`.
    pub env: String,
}

impl Identity {
    /// Build an identity from a service key and a tier.
    pub fn new(service: impl Into<String>, env: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            env: env.into(),
        }
    }
}

/// What one probe run returns.
///
/// `reason` is a machine code from `CONTRACT.md` §3.6 — never free text from
/// an error, which must not be copied into the body (`CONTRACT.md` §9). A
/// probe carries no credential, so nothing about one appears here
/// (`CONTRACT.md` §1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// The check status (`CONTRACT.md` §3.5).
    pub status: CheckStatus,
    /// A short machine code from `CONTRACT.md` §3.6. Required when `status`
    /// is a failure; never free text from an error.
    pub reason: Option<String>,
    /// The status code received, when the upstream answered over HTTP.
    pub http_status: Option<u16>,
    /// The identity the caller compiled in, for `wrong_target`.
    pub expected: Option<Identity>,
    /// The identity the upstream answered with, for `wrong_target`.
    pub observed: Option<Identity>,
}

impl ProbeOutcome {
    /// A successful probe.
    pub fn ok() -> Self {
        Self {
            status: CheckStatus::Ok,
            reason: None,
            http_status: None,
            expected: None,
            observed: None,
        }
    }

    /// A failed probe with a `reason` code from `CONTRACT.md` §3.6.
    pub fn fail(status: CheckStatus, reason: impl Into<String>) -> Self {
        debug_assert!(
            status != CheckStatus::Ok,
            "ProbeOutcome::fail with status ok: use ProbeOutcome::ok"
        );
        Self {
            status,
            reason: Some(reason.into()),
            http_status: None,
            expected: None,
            observed: None,
        }
    }

    /// The HTTP status the upstream answered with, when it answered over
    /// HTTP (`CONTRACT.md` §3.4).
    pub fn with_http_status(mut self, http_status: u16) -> Self {
        self.http_status = Some(http_status);
        self
    }

    /// The identity the caller compiled in, for `wrong_target`.
    pub fn with_expected(mut self, expected: Identity) -> Self {
        self.expected = Some(expected);
        self
    }

    /// The identity the upstream answered with, for `wrong_target`.
    pub fn with_observed(mut self, observed: Identity) -> Self {
        self.observed = Some(observed);
        self
    }

    /// The `reason` that belongs in the body: the probe's own code, or the
    /// status fallback so the body always carries one for a failure.
    pub(crate) fn effective_reason(&self) -> Option<String> {
        self.reason
            .clone()
            .or_else(|| self.status.fallback_reason().map(str::to_owned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&CheckStatus::NotConfigured).unwrap(),
            "\"not_configured\""
        );
        assert_eq!(
            serde_json::to_string(&CheckStatus::WrongTarget).unwrap(),
            "\"wrong_target\""
        );
        assert_eq!(
            serde_json::to_string(&CheckStatus::Unreachable).unwrap(),
            "\"unreachable\""
        );
        assert_eq!(serde_json::to_string(&CheckStatus::Ok).unwrap(), "\"ok\"");
    }

    #[test]
    fn failure_status_without_reason_gets_a_fallback() {
        let outcome = ProbeOutcome {
            reason: None,
            ..ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body")
        };
        assert_eq!(outcome.effective_reason().as_deref(), Some("unexpected"));
    }

    #[test]
    fn builder_setters() {
        let outcome = ProbeOutcome::fail(CheckStatus::WrongTarget, "wrong_env")
            .with_http_status(200)
            .with_expected(Identity::new("keyServer", "development"))
            .with_observed(Identity::new("keyServer", "production"));
        assert_eq!(outcome.http_status, Some(200));
        assert_eq!(
            outcome.expected,
            Some(Identity::new("keyServer", "development"))
        );
        assert_eq!(
            outcome.observed,
            Some(Identity::new("keyServer", "production"))
        );
    }
}
