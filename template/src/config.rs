//! Configuration, read once at boot from the environment.
//!
//! Loading is written against an [`EnvSource`] rather than `std::env`
//! directly, so what is required, what defaults, and what a deployed
//! environment refuses are all testable without mutating process-global
//! state. This is the platform's standard shape
//! (`maxion-admin-guard/contract/PLATFORM.md` §3) — copy it into a new
//! service unchanged except for `PORT`'s default and anything domain-specific
//! you add.

use std::collections::BTreeMap;
use std::str::FromStr;

use crate::domain::error::{DomainError, DomainResult};

/// Where the platform's admin IdP publishes its keys, relative to its base
/// URL. `ADMIN_JWKS_URL` overrides it wholesale when an IdP does not.
const DEFAULT_JWKS_PATH: &str = "/.well-known/jwks.json";

/// Introspection path on `maxgame-admin-auth-server`.
const DEFAULT_INTROSPECT_PATH: &str = "/api/v1/oauth/introspect";

/// Shortest secret a deployed environment will boot with — what
/// `openssl rand -base64 32` produces at its shortest useful encoding.
const MIN_DEPLOYED_SECRET_LEN: usize = 32;

/// Which environment this process is running in.
///
/// The load-bearing distinction is **development versus deployed**, not
/// development versus production. Staging holds real credentials and is
/// reachable from the internet, so every guardrail in [`Config::validate`]
/// applies to it exactly as it does to production. It is a separate variant
/// rather than being folded into `Production` only so logs can say which one
/// is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv {
    /// A developer's machine.
    Development,
    Staging,
    Production,
}

impl AppEnv {
    pub fn is_production(&self) -> bool {
        matches!(self, AppEnv::Production)
    }

    /// Whether this is a developer's machine — where CORS may mirror the
    /// request origin and every deployment guardrail is relaxed.
    ///
    /// Everything that protects a deployed environment must be keyed on the
    /// negation of this, never on [`AppEnv::is_production`]: a check written
    /// "production only" silently exempts staging, which is exactly where a
    /// misconfiguration gets found first.
    pub fn is_dev(&self) -> bool {
        matches!(self, AppEnv::Development)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            AppEnv::Development => "development",
            AppEnv::Staging => "staging",
            AppEnv::Production => "production",
        }
    }
}

impl FromStr for AppEnv {
    type Err = DomainError;

    fn from_str(s: &str) -> DomainResult<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "production" | "prod" => Ok(AppEnv::Production),
            "staging" | "uat" => Ok(AppEnv::Staging),
            // Deliberately NOT "test": it names a deployed QA environment at
            // least as often as a local one, and guessing wrong hands an
            // internet-facing deployment the relaxed rules.
            "development" | "dev" | "local" => Ok(AppEnv::Development),
            other => Err(DomainError::Internal(format!(
                "unknown APP_ENV '{other}' (expected development, dev, local, staging, uat, \
                 production or prod)"
            ))),
        }
    }
}

/// Where to read an environment variable from.
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct SystemEnv;

impl EnvSource for SystemEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }
}

impl EnvSource for BTreeMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        BTreeMap::get(self, key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
}

/// The admin identity provider whose tokens this service accepts. Field
/// names and defaults match `PLATFORM.md` §3.3's standard IdP-consumer set
/// byte for byte — every Rust service on the platform reads the same five
/// variable names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminIdpConfig {
    pub base_url: String,
    pub issuer: String,
    pub jwks_url: String,
    pub introspect_url: String,
    pub introspect_api_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub app_env: AppEnv,
    pub host: String,
    pub port: u16,
    /// Gateway prefix (`PLATFORM.md` §5). Empty = mounted at root, today's
    /// behaviour. `/healthz` and `/readyz` stay at the root regardless.
    pub base_path: String,

    pub database_url: String,
    pub database_max_connections: u32,

    pub admin_idp: AdminIdpConfig,

    pub cors_allowed_origins: Vec<String>,
}

impl Config {
    pub fn from_env() -> DomainResult<Self> {
        Config::load(&SystemEnv)
    }

    pub fn load(env: &impl EnvSource) -> DomainResult<Self> {
        let app_env: AppEnv = required(env, "APP_ENV")?.parse()?;

        let idp_base_url = required(env, "ADMIN_IDP_BASE_URL")?
            .trim_end_matches('/')
            .to_string();

        let config = Config {
            app_env,
            host: env.get("HOST").unwrap_or_else(|| "0.0.0.0".into()),
            // Pick the next free port for a real service — see
            // `PLATFORM.md` §3.1's port table before claiming one.
            port: parse_or(env, "PORT", 8097)?,
            base_path: env
                .get("BASE_PATH")
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_string(),

            database_url: required(env, "DATABASE_URL")?,
            database_max_connections: parse_or(env, "DATABASE_MAX_CONNECTIONS", 10)?,

            admin_idp: AdminIdpConfig {
                issuer: required(env, "ADMIN_JWT_ISSUER")?,
                jwks_url: env
                    .get("ADMIN_JWKS_URL")
                    .unwrap_or_else(|| format!("{idp_base_url}{DEFAULT_JWKS_PATH}")),
                introspect_url: {
                    let path = env
                        .get("ADMIN_INTROSPECT_PATH")
                        .unwrap_or_else(|| DEFAULT_INTROSPECT_PATH.into());
                    format!("{idp_base_url}{path}")
                },
                introspect_api_key: required(env, "ADMIN_INTROSPECT_API_KEY")?,
                base_url: idp_base_url,
            },

            cors_allowed_origins: csv_list(
                env.get("CORS_ALLOWED_ORIGINS").unwrap_or_default().as_str(),
            ),
        };

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> DomainResult<()> {
        if !self.base_path.is_empty() && !self.base_path.starts_with('/') {
            return Err(DomainError::Internal(format!(
                "BASE_PATH must start with '/' when set, got '{}'",
                self.base_path
            )));
        }
        if self.cors_allowed_origins.iter().any(|o| o == "*") {
            return Err(DomainError::Internal(
                "CORS_ALLOWED_ORIGINS must not contain the literal '*'; list each browser \
                 origin that may call this service"
                    .into(),
            ));
        }

        // Deployed, not merely production: staging holds real credentials
        // and is reachable from the internet, so every guardrail here
        // applies to it exactly as it does to production.
        if !self.app_env.is_dev() {
            let env = self.app_env.as_str();

            if !self.admin_idp.jwks_url.starts_with("https://") {
                return Err(DomainError::Internal(format!(
                    "ADMIN_JWKS_URL must be https in {env}: the signing keys arrive over it, \
                     so a plaintext fetch is a token-forgery path"
                )));
            }
            if self.cors_allowed_origins.is_empty() {
                return Err(DomainError::Internal(format!(
                    "CORS_ALLOWED_ORIGINS must list at least one origin in {env}; only \
                     development may leave it empty"
                )));
            }
            if self.admin_idp.introspect_api_key.len() < MIN_DEPLOYED_SECRET_LEN {
                return Err(DomainError::Internal(format!(
                    "ADMIN_INTROSPECT_API_KEY must be at least {MIN_DEPLOYED_SECRET_LEN} \
                     characters in {env} (generate with `openssl rand -base64 32`)"
                )));
            }
        }

        Ok(())
    }

    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn required(env: &impl EnvSource, key: &str) -> DomainResult<String> {
    env.get(key)
        .ok_or_else(|| DomainError::Internal(format!("{key} is required but not set")))
}

fn parse_or<T>(env: &impl EnvSource, key: &str, default: T) -> DomainResult<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match env.get(key) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<T>()
            .map_err(|e| DomainError::Internal(format!("{key} is not a valid value: {e}"))),
    }
}

fn csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> BTreeMap<String, String> {
        [
            ("APP_ENV", "development"),
            ("ADMIN_IDP_BASE_URL", "https://api.maxion.game"),
            ("ADMIN_JWT_ISSUER", "maxion-platform.maxion.game"),
            ("ADMIN_INTROSPECT_API_KEY", "test-introspect-key"),
            ("DATABASE_URL", "postgres://u:p@localhost:5432/db"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn deployed(app_env: &str) -> BTreeMap<String, String> {
        let mut env = minimal();
        env.insert("APP_ENV".into(), app_env.into());
        env.insert(
            "CORS_ALLOWED_ORIGINS".into(),
            "https://backoffice.maxion.game".into(),
        );
        env.insert(
            "ADMIN_INTROSPECT_API_KEY".into(),
            "a".repeat(MIN_DEPLOYED_SECRET_LEN),
        );
        env
    }

    #[test]
    fn defaults_cover_everything_except_the_required_values() {
        let config = Config::load(&minimal()).unwrap();
        assert_eq!(config.app_env, AppEnv::Development);
        assert_eq!(config.port, 8097);
        assert_eq!(config.base_path, "");
        assert!(config.cors_allowed_origins.is_empty());
        assert_eq!(
            config.admin_idp.jwks_url,
            "https://api.maxion.game/.well-known/jwks.json"
        );
        assert_eq!(
            config.admin_idp.introspect_url,
            "https://api.maxion.game/api/v1/oauth/introspect"
        );
    }

    #[test]
    fn app_env_is_required_with_no_default() {
        let mut env = minimal();
        env.remove("APP_ENV");
        let err = Config::load(&env).unwrap_err().to_string();
        assert!(err.contains("APP_ENV"), "{err}");
    }

    #[test]
    fn staging_and_uat_are_deployed_environments_not_development() {
        for value in ["staging", "uat", "STAGING", " Staging "] {
            let config = Config::load(&deployed(value)).unwrap();
            assert_eq!(config.app_env, AppEnv::Staging, "{value}");
            assert!(
                !config.app_env.is_dev(),
                "{value} is not a developer machine"
            );
            assert!(!config.app_env.is_production(), "{value} is not production");
        }
    }

    /// The point of the three-tier split: every guardrail below must fire
    /// identically for staging and production, keyed on `!is_dev()`.
    #[test]
    fn every_deployed_environment_gets_the_same_guardrails() {
        for app_env in ["staging", "uat", "production"] {
            let mut plaintext_jwks = deployed(app_env);
            plaintext_jwks.insert(
                "ADMIN_JWKS_URL".into(),
                "http://api.maxion.game/.well-known/jwks.json".into(),
            );
            let err = Config::load(&plaintext_jwks).unwrap_err().to_string();
            assert!(err.contains("ADMIN_JWKS_URL"), "{app_env}: {err}");

            let mut no_cors = deployed(app_env);
            no_cors.remove("CORS_ALLOWED_ORIGINS");
            let err = Config::load(&no_cors).unwrap_err().to_string();
            assert!(err.contains("CORS_ALLOWED_ORIGINS"), "{app_env}: {err}");

            let mut short_key = deployed(app_env);
            short_key.insert("ADMIN_INTROSPECT_API_KEY".into(), "short".into());
            let err = Config::load(&short_key).unwrap_err().to_string();
            assert!(err.contains("ADMIN_INTROSPECT_API_KEY"), "{app_env}: {err}");

            assert!(
                Config::load(&deployed(app_env)).is_ok(),
                "{app_env} with every guardrail satisfied must boot"
            );
        }
    }

    #[test]
    fn a_literal_star_cors_origin_is_refused_in_every_environment() {
        let mut env = minimal();
        env.insert("CORS_ALLOWED_ORIGINS".into(), "*".into());
        let err = Config::load(&env).unwrap_err().to_string();
        assert!(err.contains("CORS_ALLOWED_ORIGINS"), "{err}");
    }

    #[test]
    fn base_path_must_start_with_a_slash_when_set() {
        let mut env = minimal();
        env.insert("BASE_PATH".into(), "no-slash".into());
        let err = Config::load(&env).unwrap_err().to_string();
        assert!(err.contains("BASE_PATH"), "{err}");

        let mut env = minimal();
        env.insert("BASE_PATH".into(), "/example".into());
        assert_eq!(Config::load(&env).unwrap().base_path, "/example");
    }

    #[test]
    fn a_trailing_slash_on_base_path_is_trimmed() {
        let mut env = minimal();
        env.insert("BASE_PATH".into(), "/example/".into());
        assert_eq!(Config::load(&env).unwrap().base_path, "/example");
    }
}
