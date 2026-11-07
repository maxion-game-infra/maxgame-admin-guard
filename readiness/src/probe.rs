//! The [`Probe`] trait, a closure adapter, and target sanitisation.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::model::ProbeOutcome;

/// One hop check, run by the caller (`CONTRACT.md` §6.2).
///
/// A probe uses the same client and URL the request path uses, has no side
/// effects, and sends no credential (`CONTRACT.md` §1.1, §6.2). It runs
/// either inline on every `/readyz` request
/// ([`ReadinessBuilder::gate`](crate::ReadinessBuilder::gate)) or in the
/// background ([`ReadinessBuilder::report`](crate::ReadinessBuilder::report)).
pub trait Probe: Send + Sync + 'static {
    /// Run the check once.
    fn run(&self) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>>;

    /// The sanitized target that belongs in the body: scheme, host, port and
    /// path only (`CONTRACT.md` §3.4, §9). `None` when the hop has no single
    /// target to name.
    fn target(&self) -> Option<String> {
        None
    }

    /// Dedupe key: hops whose probes return the same `Some(key)` share one
    /// run per cycle (`CONTRACT.md` §6.3 rule 3). `None` (the default) means
    /// the hop is never deduplicated.
    fn dedupe_key(&self) -> Option<String> {
        None
    }

    /// A longer minimum interval for this hop, such as 300 s for an external
    /// provider or 60 s for a chain RPC (`CONTRACT.md` §6.3 rule 4). `None`
    /// (the default) means the global interval applies.
    fn min_interval(&self) -> Option<Duration> {
        None
    }
}

/// Adapter that turns any `Fn() -> impl Future<Output = ProbeOutcome>` into a
/// [`Probe`].
///
/// ```
/// use std::time::Duration;
/// use maxgame_readiness::{FnProbe, Probe, ProbeOutcome};
///
/// let probe = FnProbe::new(|| async { ProbeOutcome::ok() })
///     .target("http://key-server.platform.svc:8092/keys")
///     .dedupe_key("key-server:8092")
///     .min_interval(Duration::from_secs(60));
/// assert_eq!(
///     Probe::target(&probe).as_deref(),
///     Some("http://key-server.platform.svc:8092/keys")
/// );
/// ```
pub struct FnProbe<F> {
    f: std::sync::Arc<F>,
    target: Option<String>,
    dedupe_key: Option<String>,
    min_interval: Option<Duration>,
}

impl<F> FnProbe<F> {
    /// Wrap a probe closure. The closure and its future must be `Send` and
    /// `'static` because the background runner owns them.
    pub fn new(f: F) -> Self {
        Self {
            f: std::sync::Arc::new(f),
            target: None,
            dedupe_key: None,
            min_interval: None,
        }
    }

    /// See [`Probe::target`].
    pub fn target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// See [`Probe::dedupe_key`].
    pub fn dedupe_key(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }

    /// See [`Probe::min_interval`].
    pub fn min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = Some(interval);
        self
    }
}

impl<F, Fut> Probe for FnProbe<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ProbeOutcome> + Send + 'static,
{
    fn run(&self) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
        let f = self.f.clone();
        Box::pin(async move { f().await })
    }

    fn target(&self) -> Option<String> {
        self.target.clone()
    }

    fn dedupe_key(&self) -> Option<String> {
        self.dedupe_key.clone()
    }

    fn min_interval(&self) -> Option<Duration> {
        self.min_interval
    }
}

/// Reduce a configured URL to what may appear in the body: scheme, host,
/// port and path (`CONTRACT.md` §9). Userinfo, query string and fragment are
/// stripped; anything that does not parse as `scheme://host[:port][/path]`
/// yields `None`, so the field is omitted rather than leaking.
///
/// ```
/// use maxgame_readiness::sanitize_target;
///
/// assert_eq!(
///     sanitize_target("https://user:secret@auth.internal:8091/base/v1/ping?x=1#frag"),
///     Some("https://auth.internal:8091/base/v1/ping".to_string())
/// );
/// assert_eq!(sanitize_target("not a url"), None);
/// ```
pub fn sanitize_target(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() || url.contains(char::is_whitespace) {
        return None;
    }

    // scheme
    let (scheme, rest) = url.split_once(':')?;
    let mut scheme_chars = scheme.chars();
    let first = scheme_chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !scheme_chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    let rest = rest.strip_prefix("//")?;

    // authority ends at the first '/', '?' or '#'; the path ends at the first
    // '?' or '#'. Everything after '?' or '#' is dropped (CONTRACT.md §9).
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, rest) = rest.split_at(authority_len);
    let path = rest.split(['?', '#']).next().unwrap_or("");

    // strip userinfo: everything up to the last '@' before the path
    let host_port = match authority.rsplit_once('@') {
        Some((_userinfo, host_port)) => host_port,
        None => authority,
    };
    if host_port.is_empty() {
        return None;
    }

    let host = split_host(host_port)?;
    if host.is_empty() {
        return None;
    }
    if host.starts_with('[') {
        // IPv6 literal: keep whatever is inside the brackets
        if !host.ends_with(']') || host.len() < 3 {
            return None;
        }
    } else if host
        .chars()
        .any(|c| matches!(c, '/' | '?' | '#' | '@' | '[' | ']'))
    {
        return None;
    }
    if let Some(port) = port(host_port) {
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }

    let mut sanitized = format!("{}://{}", scheme.to_ascii_lowercase(), host_port);
    if !path.is_empty() {
        sanitized.push_str(path);
    }
    Some(sanitized)
}

/// Split `host[:port]` into the host part, tolerating an IPv6 literal.
fn split_host(host_port: &str) -> Option<&str> {
    if let Some(rest) = host_port.strip_prefix('[') {
        let end = rest.find(']')?;
        return Some(&host_port[..end + 2]);
    }
    match host_port.rsplit_once(':') {
        Some((host, _port)) => Some(host),
        None => Some(host_port),
    }
}

fn port(host_port: &str) -> Option<&str> {
    if host_port.starts_with('[') {
        let end = host_port.find(']')?;
        return host_port[end + 1..].strip_prefix(':');
    }
    host_port.rsplit_once(':').map(|(_, port)| port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_scheme_host_port_and_path() {
        assert_eq!(
            sanitize_target("http://auth-server.platform.svc:8091/accounts"),
            Some("http://auth-server.platform.svc:8091/accounts".to_string())
        );
        assert_eq!(
            sanitize_target("HTTPS://admin-auth.internal/.well-known/jwks.json"),
            Some("https://admin-auth.internal/.well-known/jwks.json".to_string())
        );
        assert_eq!(
            sanitize_target("redis://127.0.0.1:6379"),
            Some("redis://127.0.0.1:6379".to_string())
        );
        assert_eq!(
            sanitize_target("postgres://db.internal:5432/app"),
            Some("postgres://db.internal:5432/app".to_string())
        );
    }

    #[test]
    fn strips_userinfo_query_and_fragment() {
        assert_eq!(
            sanitize_target("postgres://user:pass@db.internal:5432/app?sslmode=require"),
            Some("postgres://db.internal:5432/app".to_string())
        );
        assert_eq!(
            sanitize_target("http://h.example/p?q=1#frag"),
            Some("http://h.example/p".to_string())
        );
        assert_eq!(
            sanitize_target("http://user@h.example"),
            Some("http://h.example".to_string())
        );
        assert_eq!(
            sanitize_target("http://h.example?q=1"),
            Some("http://h.example".to_string())
        );
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(sanitize_target(""), None);
        assert_eq!(sanitize_target("not a url"), None);
        assert_eq!(sanitize_target("db.internal:5432"), None, "no scheme");
        assert_eq!(sanitize_target("http://"), None, "no host");
        assert_eq!(sanitize_target("mailto:someone@example.com"), None);
        assert_eq!(sanitize_target("http://host:notaport/"), None);
        assert_eq!(
            sanitize_target("1http://host/"),
            None,
            "scheme must start alpha"
        );
        assert_eq!(sanitize_target("http://ho st/"), None);
    }

    #[test]
    fn accepts_ipv6_literals() {
        assert_eq!(
            sanitize_target("http://[::1]:8545"),
            Some("http://[::1]:8545".to_string())
        );
        assert_eq!(
            sanitize_target("http://[::1]"),
            Some("http://[::1]".to_string())
        );
        assert_eq!(sanitize_target("http://[::1"), None);
    }
}
