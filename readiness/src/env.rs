//! Environment tier (`CONTRACT.md` §3.4): the `/readyz` `env` field is the
//! parsed tier, never the raw `APP_ENV` spelling.

use std::fmt;
use std::str::FromStr;

/// The deployment tier a service runs in.
///
/// Serializes as `development`, `staging` or `production` (`CONTRACT.md`
/// §3.2), which is also what `GET {BASE_PATH}/v1/ping` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppEnv {
    /// A laptop or the dev cluster.
    Development,
    /// The staging cluster.
    Staging,
    /// The production cluster.
    Production,
}

impl AppEnv {
    /// The wire spelling of the tier.
    pub fn as_str(self) -> &'static str {
        match self {
            AppEnv::Development => "development",
            AppEnv::Staging => "staging",
            AppEnv::Production => "production",
        }
    }
}

impl fmt::Display for AppEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returned by [`AppEnv::from_str`] for a value that is not one of the three
/// tiers. Callers fail boot on it instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseAppEnvError;

impl fmt::Display for ParseAppEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected one of development, staging, production")
    }
}

impl std::error::Error for ParseAppEnvError {}

impl FromStr for AppEnv {
    type Err = ParseAppEnvError;

    /// Case-insensitive parse of the three tiers. Anything else is an error:
    /// a laptop and the dev cluster are both `development`, so shorthand such
    /// as `dev` must be normalized by the caller before it reaches here.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "development" => Ok(AppEnv::Development),
            "staging" => Ok(AppEnv::Staging),
            "production" => Ok(AppEnv::Production),
            _ => Err(ParseAppEnvError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_tier() {
        assert_eq!(
            serde_json::to_string(&AppEnv::Production).unwrap(),
            "\"production\""
        );
        assert_eq!(
            serde_json::to_string(&AppEnv::Staging).unwrap(),
            "\"staging\""
        );
        assert_eq!(
            serde_json::to_string(&AppEnv::Development).unwrap(),
            "\"development\""
        );
    }

    #[test]
    fn parses_case_insensitively() {
        assert_eq!("Production".parse::<AppEnv>().unwrap(), AppEnv::Production);
        assert_eq!(
            "DEVELOPMENT".parse::<AppEnv>().unwrap(),
            AppEnv::Development
        );
        assert!("dev".parse::<AppEnv>().is_err());
    }
}
