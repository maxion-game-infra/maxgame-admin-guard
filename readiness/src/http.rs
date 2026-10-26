//! HTTP hop probes (`CONTRACT.md` §6.2) and the classification they share
//! (`CONTRACT.md` §3.5, §3.6).
//!
//! Every probe here is **connection and configuration only**: it sends no
//! credential — no `Authorization`, no `x-api-key`, nothing — and never
//! reports whether one is valid (`CONTRACT.md` §1.1). Pass the service's
//! own `reqwest::Client` **without** default credential headers: a client
//! whose `default_headers` carry a credential would leak it to whatever
//! address a misconfigured URL happens to point at.
//!
//! A probe uses the same client and URL the request path uses
//! (`CONTRACT.md` §6.2), marks every request with
//! [`PROBE_HEADER`](crate::PROBE_HEADER) so the upstream can keep the
//! traffic out of its metrics and rate-limit budgets (`CONTRACT.md` §6.3
//! rule 1), reads at most 64 KiB of any body, and turns what happened into
//! a [`ProbeOutcome`] carrying a `CONTRACT.md` §3.6 reason code — never
//! error text (`CONTRACT.md` §9). The per-check 2 s timeout of §6.1 is the
//! runner's bound ([`Readiness::run_reporting_once`](crate::Readiness),
//! `probe_timeout`), not the probe's.
//!
//! Register the probes as **reporting** hops; an upstream HTTP hop MUST NOT
//! be gating (`CONTRACT.md` §2):
//!
//! ```text
//! let readiness = Readiness::builder("launcher", AppEnv::Development)
//!     .report(
//!         "key_server",
//!         PingProbe::new(
//!             client,                            // no credential headers
//!             "http://key-server.platform.svc:8092/keys",
//!             "launcher",                       // the caller's service key
//!             Identity::new("keyServer", "development"),
//!         ),
//!     )
//!     .build();
//! ```
//!
//! The status classification of a probe never merges `unexpected` and
//! `unreachable` (`CONTRACT.md` §3.5): a wrong answer — an unexpected 4xx,
//! a body that is not the expected shape, the wrong identity — means "our
//! configuration points at the wrong place", while a 5xx or a network
//! error means "we cannot tell".

use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::Value;

use crate::model::{CheckStatus, Identity, ProbeOutcome};
use crate::probe::{sanitize_target, Probe};
use crate::PROBE_HEADER;

/// The most a probe reads of an upstream body (`CONTRACT.md` §6.2): a probe
/// has no side effects and spends no budget, so it must not download an SPA
/// shell or an error page, and every body this contract parses is a small
/// JSON document. A longer body is `unexpected` with reason `bad_body`
/// wherever the body matters.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// The minimum interval of a chain RPC hop (`CONTRACT.md` §6.3 rule 4).
const CHAIN_RPC_MIN_INTERVAL: Duration = Duration::from_secs(60);

// ----------------------------------------------------------------------
// Classification (CONTRACT.md §3.5, §3.6)
// ----------------------------------------------------------------------

/// Classify a transport failure of a probe request (`CONTRACT.md` §3.5,
/// §3.6):
///
/// * a timeout — connect, read or the whole request — is `unreachable`
///   with reason `timeout`;
/// * a body that cannot be read or decoded is `unexpected` with reason
///   `bad_body`: the upstream answered, its answer just is not a body the
///   probe can parse;
/// * a connect error is `unreachable`, and its `source()` chain says which
///   reason: `dns` (the host does not resolve), `tls` (the certificate or
///   the handshake failed) or `connect` (anything else);
/// * anything else — a request that could not be built, a redirect the
///   client refused to follow — is `unreachable` with reason `connect`.
///
/// Pure: no clock and no I/O. The error text is never copied into the
/// outcome (`CONTRACT.md` §9); only the reason codes above appear.
pub fn classify_transport_error(err: &reqwest::Error) -> ProbeOutcome {
    if err.is_timeout() {
        return ProbeOutcome::fail(CheckStatus::Unreachable, "timeout");
    }
    if err.is_body() || err.is_decode() {
        return ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body");
    }
    if err.is_connect() {
        return ProbeOutcome::fail(CheckStatus::Unreachable, connect_reason(err));
    }
    ProbeOutcome::fail(CheckStatus::Unreachable, "connect")
}

/// Which `reason` code a connect error carries (`CONTRACT.md` §3.6). The
/// resolver, the TLS stack and the socket each report through their own
/// error type, so the `source()` chain is walked and matched on its
/// messages: the DNS markers (`dns error`, `failed to lookup address`,
/// `nodename nor servname`) win over the TLS markers (`certificate`,
/// `InvalidCertificate`, `tls`), and a chain matching neither is a plain
/// `connect`. Pure.
fn connect_reason(err: &(dyn Error + 'static)) -> &'static str {
    let mut messages: Vec<String> = Vec::new();
    let mut source = Some(err);
    while let Some(current) = source {
        messages.push(current.to_string().to_ascii_lowercase());
        source = current.source();
    }
    if messages.iter().any(|message| {
        message.contains("dns error")
            || message.contains("failed to lookup address")
            || message.contains("nodename nor servname")
    }) {
        return "dns";
    }
    if messages.iter().any(|message| {
        message.contains("invalidcertificate")
            || message.contains("certificate")
            || message.contains("tls")
    }) {
        return "tls";
    }
    "connect"
}

/// Classify an HTTP status a probe does not accept, when reading the body
/// cannot change the answer (`CONTRACT.md` §3.5, §3.6):
///
/// * a 5xx is `unreachable` with reason `upstream_5xx` — we cannot tell
///   whether our configuration is right (`CONTRACT.md` §3.5);
/// * a 404 is `unexpected` with reason `not_found` — the path or the
///   callee's `BASE_PATH` is wrong;
/// * every other answer below 500, including a 3xx the client did not
///   follow and a 2xx that is not the one status the probe accepts, is
///   `unexpected` with reason `unexpected_status`.
///
/// Pure. The outcome carries the status it classified.
///
/// ```
/// use maxgame_readiness::{classify_error_status, CheckStatus};
///
/// let upstream = classify_error_status(503);
/// assert_eq!(upstream.status, CheckStatus::Unreachable);
/// assert_eq!(upstream.reason.as_deref(), Some("upstream_5xx"));
/// assert_eq!(upstream.http_status, Some(503));
///
/// let not_found = classify_error_status(404);
/// assert_eq!(not_found.status, CheckStatus::Unexpected);
/// assert_eq!(not_found.reason.as_deref(), Some("not_found"));
///
/// let redirect = classify_error_status(302);
/// assert_eq!(redirect.status, CheckStatus::Unexpected);
/// assert_eq!(redirect.reason.as_deref(), Some("unexpected_status"));
/// ```
pub fn classify_error_status(status: u16) -> ProbeOutcome {
    if (500..=599).contains(&status) {
        ProbeOutcome::fail(CheckStatus::Unreachable, "upstream_5xx").with_http_status(status)
    } else if status == 404 {
        ProbeOutcome::fail(CheckStatus::Unexpected, "not_found").with_http_status(status)
    } else {
        ProbeOutcome::fail(CheckStatus::Unexpected, "unexpected_status").with_http_status(status)
    }
}

// ----------------------------------------------------------------------
// URL handling
// ----------------------------------------------------------------------

/// Parse a configured URL into a URL a probe may request: absolute `http`
/// or `https` with a host, nothing else. `None` is what a probe reports as
/// `misconfigured` with reason `invalid_url` (`CONTRACT.md` §3.5, §3.6, §7)
/// from [`Probe::run`], never a constructor panic.
fn parse_http_url(raw: &str) -> Option<url::Url> {
    let url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return None;
    }
    Some(url)
}

/// The URL a probe requests for a configured hop URL. Userinfo is stripped
/// — it is a credential, and a probe sends none (`CONTRACT.md` §1.1) — and
/// so is the fragment, which is never sent over HTTP anyway. The query
/// stays, because it is part of the address the request path uses.
fn request_url(raw: &str) -> Option<String> {
    let mut url = parse_http_url(raw)?;
    // Both fail only for URLs that cannot be a base, already rejected above.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    Some(url.into())
}

/// The `GET {BASE_PATH}/v1/ping` URL for a configured base URL
/// (`CONTRACT.md` §5.3): the base includes the callee's `BASE_PATH`, such
/// as `http://key-server:8092/keys`, and the ping route hangs off its end.
/// A trailing `/` on the base must not produce `//`, and userinfo, query
/// and fragment are dropped: a fleet base URL is
/// `scheme://host[:port][/base_path]`, and a credential hiding in one is
/// not sent (`CONTRACT.md` §1.1).
fn ping_url(base: &str) -> Option<String> {
    let mut url = parse_http_url(base)?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let base_path = url.path().trim_end_matches('/');
    let ping_path = if base_path.is_empty() {
        "/v1/ping".to_owned()
    } else {
        format!("{base_path}/v1/ping")
    };
    url.set_path(&ping_path);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

// ----------------------------------------------------------------------
// Body reading and body classification
// ----------------------------------------------------------------------

/// What reading a response body up to [`MAX_BODY_BYTES`] came to.
enum BodyRead {
    /// The whole body, which is at most [`MAX_BODY_BYTES`] bytes long.
    Complete(Vec<u8>),
    /// The body is longer than [`MAX_BODY_BYTES`]: whatever the upstream is
    /// sending, it is not the small JSON answer a probe of this contract
    /// expects.
    TooLarge,
}

/// Read the body chunk by chunk and stop at [`MAX_BODY_BYTES`]
/// (`CONTRACT.md` §6.2: a probe must not download an SPA shell or an error
/// page). The rest of the body is never read; dropping the response closes
/// the connection instead.
async fn read_body_bounded(mut response: reqwest::Response) -> Result<BodyRead, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Ok(BodyRead::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(BodyRead::Complete(body))
}

/// Send a probe request, classifying everything up to the status line: a
/// transport failure becomes the outcome it deserves (`CONTRACT.md` §3.5),
/// and a received response is handed to the caller, which knows which
/// statuses it accepts and whether the body matters.
async fn send_probe(request: reqwest::RequestBuilder) -> Result<reqwest::Response, ProbeOutcome> {
    match request.send().await {
        Ok(response) => Ok(response),
        Err(err) => Err(classify_transport_error(&err)),
    }
}

/// The `misconfigured` / `invalid_url` outcome for a configured URL that is
/// not an absolute http(s) URL (`CONTRACT.md` §3.5, §3.6, §7).
fn invalid_url() -> ProbeOutcome {
    ProbeOutcome::fail(CheckStatus::Misconfigured, "invalid_url")
}

/// What a `/v1/ping` body says, against the identity the caller compiled in
/// (`CONTRACT.md` §5.3). Pure; the caller attaches the HTTP status.
///
/// * JSON object with string `service` and `env`, both matching: `ok`;
/// * `service` differs: `wrong_target` with reason `wrong_service`, checked
///   first, even when the env differs too;
/// * `env` differs: `wrong_target` with reason `wrong_env` — this is what
///   catches dev pointing at prod, and the reverse;
/// * not JSON, not an object, or a missing or non-string field:
///   `unexpected` with reason `bad_body` — this catches the SPA shell.
fn classify_ping_body(body: &[u8], expected: &Identity) -> ProbeOutcome {
    let observed: PingBody = match serde_json::from_slice(body) {
        Ok(parsed) => parsed,
        Err(_) => return ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body"),
    };
    let observed = Identity::new(observed.service, observed.env);
    if observed.service != expected.service {
        return ProbeOutcome::fail(CheckStatus::WrongTarget, "wrong_service")
            .with_expected(expected.clone())
            .with_observed(observed);
    }
    if observed.env != expected.env {
        return ProbeOutcome::fail(CheckStatus::WrongTarget, "wrong_env")
            .with_expected(expected.clone())
            .with_observed(observed);
    }
    ProbeOutcome::ok()
}

/// The `GET {BASE_PATH}/v1/ping` body (`CONTRACT.md` §5.2): a JSON object
/// with exactly the two identity fields. Unknown keys are ignored here —
/// the callee MUST NOT send them, but a caller must not stop recognizing a
/// callee over an additive field.
#[derive(serde::Deserialize)]
struct PingBody {
    service: String,
    env: String,
}

/// What a JWKS body says (`CONTRACT.md` §6.2). Pure; the caller attaches
/// the HTTP status.
///
/// * JSON with an array `keys` of at least one key: `ok`;
/// * `keys` present but empty: `unexpected` with reason `no_keys`;
/// * not JSON, not an object, no `keys` array: `unexpected` with reason
///   `bad_body`.
///
/// Which algorithms the keys use is the verifier's business, not the
/// probe's: the probe has none of the verifier's configuration.
fn classify_jwks_body(body: &[u8]) -> ProbeOutcome {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(parsed) => parsed,
        Err(_) => return ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body"),
    };
    match parsed.get("keys").and_then(Value::as_array) {
        Some(keys) if keys.is_empty() => ProbeOutcome::fail(CheckStatus::Unexpected, "no_keys"),
        Some(_) => ProbeOutcome::ok(),
        None => ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body"),
    }
}

/// Parse an `eth_chainId` result: a `0x`-prefixed hex quantity, so `0x61`
/// is chain 97. `None` for anything else — no prefix, an empty quantity, a
/// sign, a non-hex digit, or more bits than a `u64` holds.
fn parse_chain_id(hex: &str) -> Option<u64> {
    let digits = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X"))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

/// What an `eth_chainId` response says, against the chain the caller
/// configured (`CONTRACT.md` §6.2). Pure; the caller attaches the HTTP
/// status.
///
/// * a `result` quantity equal to the configured chain: `ok`;
/// * a `result` quantity for another chain: `wrong_target` with reason
///   `wrong_chain`, a code beyond §3.6, which a service MAY add;
/// * no `result` string — including a JSON-RPC `error` answer: `unexpected`
///   with reason `bad_body`.
fn classify_chain_body(body: &[u8], expected: u64) -> ProbeOutcome {
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(parsed) => parsed,
        Err(_) => return ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body"),
    };
    let Some(result) = parsed.get("result").and_then(Value::as_str) else {
        return ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body");
    };
    match parse_chain_id(result) {
        Some(chain_id) if chain_id == expected => ProbeOutcome::ok(),
        Some(_) => ProbeOutcome::fail(CheckStatus::WrongTarget, "wrong_chain"),
        None => ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body"),
    }
}

// ----------------------------------------------------------------------
// Probes (CONTRACT.md §6.2)
// ----------------------------------------------------------------------

/// Probe for a fleet HTTP service hop (`CONTRACT.md` §5, §6.2): `GET
/// {base_url}/v1/ping`, compared with the identity the caller compiled in.
///
/// The configured base URL **includes** the callee's `BASE_PATH`
/// (`CONTRACT.md` §5.1), such as `http://key-server:8092/keys`; the probe
/// requests `{base}/v1/ping`, so a wrong or missing prefix answers 404,
/// which is `unexpected` (`not_found`) rather than `ok` — a generic 200 is
/// no proof, because several hosts answer an SPA shell for every unknown
/// path. A 200 with the wrong `service` or `env` is `wrong_target`, which
/// is what catches a base URL pointing at another service, or at the prod
/// cluster from dev.
///
/// Uses the same `reqwest::Client` the request path uses. Pass a client
/// **without** default `Authorization` or `x-api-key` headers: this probe
/// sends no credential and never reports whether one is valid
/// (`CONTRACT.md` §1.1).
pub struct PingProbe {
    client: reqwest::Client,
    base_url: String,
    caller: String,
    expected: Identity,
}

impl PingProbe {
    /// Probe `GET {base_url}/v1/ping`, expecting `expected`. `caller` is
    /// the caller's own service key, sent as the
    /// [`PROBE_HEADER`](crate::PROBE_HEADER) value (`CONTRACT.md` §6.3
    /// rule 1). A `base_url` that is not an absolute http(s) URL is not
    /// rejected here; [`Probe::run`] reports it as `misconfigured` with
    /// reason `invalid_url`.
    pub fn new(
        client: reqwest::Client,
        base_url: impl Into<String>,
        caller: impl Into<String>,
        expected: Identity,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            caller: caller.into(),
            expected,
        }
    }
}

impl Probe for PingProbe {
    fn run(&self) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
        Box::pin(async move {
            let Some(url) = ping_url(&self.base_url) else {
                return invalid_url();
            };
            let response =
                match send_probe(self.client.get(url).header(PROBE_HEADER, &self.caller)).await {
                    Ok(response) => response,
                    Err(outcome) => return outcome,
                };
            let status = response.status().as_u16();
            if status != 200 {
                return classify_error_status(status);
            }
            match read_body_bounded(response).await {
                Ok(BodyRead::Complete(body)) => {
                    classify_ping_body(&body, &self.expected).with_http_status(status)
                }
                Ok(BodyRead::TooLarge) => {
                    ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body").with_http_status(status)
                }
                Err(err) => classify_transport_error(&err).with_http_status(status),
            }
        })
    }

    fn target(&self) -> Option<String> {
        sanitize_target(&self.base_url)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(format!("GET {}", self.base_url))
    }
}

/// Probe for a JWKS hop (`CONTRACT.md` §6.2): `GET` the configured JWKS
/// URL, such as `http://admin-auth.platform.svc:8090/.well-known/jwks.json`.
///
/// `ok` requires a 200 whose body parses as a JWK Set with at least one
/// key. An empty `keys` array is `unexpected` (`no_keys`); anything that is
/// not JSON with a `keys` array — an HTML page, a JSON error — is
/// `unexpected` (`bad_body`); a 404 is `not_found`; a 5xx is `unreachable`
/// (`upstream_5xx`).
///
/// Uses the same `reqwest::Client` the request path uses. Pass a client
/// **without** default `Authorization` or `x-api-key` headers: this probe
/// sends no credential and never reports whether one is valid
/// (`CONTRACT.md` §1.1).
pub struct JwksProbe {
    client: reqwest::Client,
    url: String,
    caller: String,
}

impl JwksProbe {
    /// Probe `GET url`. `caller` is the caller's own service key, sent as
    /// the [`PROBE_HEADER`](crate::PROBE_HEADER) value (`CONTRACT.md` §6.3
    /// rule 1). A `url` that is not an absolute http(s) URL is not rejected
    /// here; [`Probe::run`] reports it as `misconfigured` with reason
    /// `invalid_url`.
    pub fn new(client: reqwest::Client, url: impl Into<String>, caller: impl Into<String>) -> Self {
        Self {
            client,
            url: url.into(),
            caller: caller.into(),
        }
    }
}

impl Probe for JwksProbe {
    fn run(&self) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
        Box::pin(async move {
            let Some(url) = request_url(&self.url) else {
                return invalid_url();
            };
            let response =
                match send_probe(self.client.get(url).header(PROBE_HEADER, &self.caller)).await {
                    Ok(response) => response,
                    Err(outcome) => return outcome,
                };
            let status = response.status().as_u16();
            if status != 200 {
                return classify_error_status(status);
            }
            match read_body_bounded(response).await {
                Ok(BodyRead::Complete(body)) => classify_jwks_body(&body).with_http_status(status),
                Ok(BodyRead::TooLarge) => {
                    ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body").with_http_status(status)
                }
                Err(err) => classify_transport_error(&err).with_http_status(status),
            }
        })
    }

    fn target(&self) -> Option<String> {
        sanitize_target(&self.url)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(format!("GET {}", self.url))
    }
}

/// Probe for a full endpoint URL behind auth (`CONTRACT.md` §6.2): the
/// admin IdP introspect endpoint, key-server `/v1/external/verify` and the
/// like. `POST` the configured URL with an empty JSON body and **no
/// credential**.
///
/// A 401 or 403 from the endpoint's own auth layer is `ok`: an
/// unauthenticated request that comes back as the endpoint's own rejection
/// proves the route exists at that path on a service that enforces auth —
/// the address is right (`CONTRACT.md` §6.2). A 404 is `unexpected`
/// (`not_found`): the path or prefix is wrong. A 2xx is `unexpected`
/// (`unexpected_status`), because an auth-protected route answering an
/// unauthenticated request means the URL points somewhere else.
///
/// Where the same base serves `/v1/ping`, prefer [`PingProbe`]
/// (`CONTRACT.md` §6.2).
///
/// Uses the same `reqwest::Client` the request path uses. Pass a client
/// **without** default `Authorization` or `x-api-key` headers: this probe
/// sends no credential and never reports whether one is valid
/// (`CONTRACT.md` §1.1) — that is the whole point of the 401/403 answer.
pub struct AuthEndpointProbe {
    client: reqwest::Client,
    url: String,
    caller: String,
}

impl AuthEndpointProbe {
    /// Probe `POST url` with an empty JSON body. `caller` is the caller's
    /// own service key, sent as the [`PROBE_HEADER`](crate::PROBE_HEADER)
    /// value (`CONTRACT.md` §6.3 rule 1). A `url` that is not an absolute
    /// http(s) URL is not rejected here; [`Probe::run`] reports it as
    /// `misconfigured` with reason `invalid_url`.
    pub fn new(client: reqwest::Client, url: impl Into<String>, caller: impl Into<String>) -> Self {
        Self {
            client,
            url: url.into(),
            caller: caller.into(),
        }
    }
}

impl Probe for AuthEndpointProbe {
    fn run(&self) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
        Box::pin(async move {
            let Some(url) = request_url(&self.url) else {
                return invalid_url();
            };
            let request = self
                .client
                .post(url)
                .header(PROBE_HEADER, &self.caller)
                .json(&serde_json::json!({}));
            let response = match send_probe(request).await {
                Ok(response) => response,
                Err(outcome) => return outcome,
            };
            let status = response.status().as_u16();
            match status {
                // The endpoint's own auth layer answered: the address is
                // right (CONTRACT.md §6.2).
                401 | 403 => ProbeOutcome::ok().with_http_status(status),
                _ => classify_error_status(status),
            }
        })
    }

    fn target(&self) -> Option<String> {
        sanitize_target(&self.url)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(format!("POST {}", self.url))
    }
}

/// Probe for a chain RPC hop (`CONTRACT.md` §6.2): a JSON-RPC
/// `eth_chainId` call, `POST`ed to the configured RPC URL.
///
/// `ok` requires a 200 whose `result` quantity equals the configured chain
/// id. A parsed `result` for another chain is `wrong_target` with reason
/// `wrong_chain`, a code beyond the §3.6 list, which a service MAY add. A
/// missing or unparsable `result` is `unexpected` (`bad_body`), a 404 is
/// `not_found`, and a 5xx is `unreachable` (`upstream_5xx`).
///
/// The hop's minimum interval is 60 s (`CONTRACT.md` §6.3 rule 4).
///
/// Uses the same `reqwest::Client` the request path uses. Pass a client
/// **without** default `Authorization` or `x-api-key` headers: this probe
/// sends no credential and never reports whether one is valid
/// (`CONTRACT.md` §1.1).
///
/// RPC URLs that embed an API key in the path (nodereal style) are
/// requested as configured, because that is the address the request path
/// uses — but [`ChainIdProbe::with_target`] replaces the `target` reported
/// in the body, since [`sanitize_target`] keeps the path and no part of a
/// secret may appear there (`CONTRACT.md` §9).
pub struct ChainIdProbe {
    client: reqwest::Client,
    url: String,
    caller: String,
    expected_chain_id: u64,
    display_target: Option<String>,
}

impl ChainIdProbe {
    /// Probe `eth_chainId` against `url`, expecting `expected_chain_id`.
    /// `caller` is the caller's own service key, sent as the
    /// [`PROBE_HEADER`](crate::PROBE_HEADER) value (`CONTRACT.md` §6.3
    /// rule 1). A `url` that is not an absolute http(s) URL is not rejected
    /// here; [`Probe::run`] reports it as `misconfigured` with reason
    /// `invalid_url`.
    pub fn new(
        client: reqwest::Client,
        url: impl Into<String>,
        caller: impl Into<String>,
        expected_chain_id: u64,
    ) -> Self {
        Self {
            client,
            url: url.into(),
            caller: caller.into(),
            expected_chain_id,
            display_target: None,
        }
    }

    /// Replace the `target` the hop reports, for a configured URL whose
    /// path embeds an API key (nodereal style): the request still goes to
    /// the configured URL, but the body shows `target` instead
    /// (`CONTRACT.md` §9). The replacement is sanitized like any target.
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.display_target = Some(target.into());
        self
    }
}

impl Probe for ChainIdProbe {
    fn run(&self) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
        Box::pin(async move {
            let Some(url) = request_url(&self.url) else {
                return invalid_url();
            };
            let rpc = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_chainId",
                "params": [],
            });
            let request = self
                .client
                .post(url)
                .header(PROBE_HEADER, &self.caller)
                .json(&rpc);
            let response = match send_probe(request).await {
                Ok(response) => response,
                Err(outcome) => return outcome,
            };
            let status = response.status().as_u16();
            if status != 200 {
                return classify_error_status(status);
            }
            match read_body_bounded(response).await {
                Ok(BodyRead::Complete(body)) => {
                    classify_chain_body(&body, self.expected_chain_id).with_http_status(status)
                }
                Ok(BodyRead::TooLarge) => {
                    ProbeOutcome::fail(CheckStatus::Unexpected, "bad_body").with_http_status(status)
                }
                Err(err) => classify_transport_error(&err).with_http_status(status),
            }
        })
    }

    fn target(&self) -> Option<String> {
        display_target(&self.display_target, &self.url)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(format!("POST {}", self.url))
    }

    fn min_interval(&self) -> Option<Duration> {
        // Chain RPC is probed at most every 60 s (CONTRACT.md §6.3 rule 4).
        Some(CHAIN_RPC_MIN_INTERVAL)
    }
}

/// Probe for an R2 / S3 endpoint or a third-party provider
/// (`CONTRACT.md` §6.2): an unauthenticated `HEAD` ([`ReachProbe::head`])
/// or `GET` ([`ReachProbe::get`]) on the configured URL.
///
/// Any HTTP answer below 500 — including the 403 an unsigned request to
/// object storage normally gets, and a redirect — is `ok`: it proves the
/// host is reachable at the configured address (`CONTRACT.md` §6.2: "any
/// HTTP answer below 500"). DNS, connect and TLS failures and a 5xx are
/// `unreachable`. This checks the account endpoint and bucket host only;
/// signing keys are not checked (`CONTRACT.md` §1.1).
///
/// Uses the same `reqwest::Client` the request path uses. Pass a client
/// **without** default `Authorization` or `x-api-key` headers: this probe
/// sends no credential and never reports whether one is valid
/// (`CONTRACT.md` §1.1).
pub struct ReachProbe {
    client: reqwest::Client,
    url: String,
    caller: String,
    method: reqwest::Method,
    min_interval: Option<Duration>,
    display_target: Option<String>,
}

impl ReachProbe {
    /// Probe an unauthenticated `HEAD url`, the shape an unsigned object
    /// storage probe takes (`CONTRACT.md` §6.2). `caller` is the caller's
    /// own service key, sent as the [`PROBE_HEADER`](crate::PROBE_HEADER)
    /// value (`CONTRACT.md` §6.3 rule 1). A `url` that is not an absolute
    /// http(s) URL is not rejected here; [`Probe::run`] reports it as
    /// `misconfigured` with reason `invalid_url`.
    pub fn head(
        client: reqwest::Client,
        url: impl Into<String>,
        caller: impl Into<String>,
    ) -> Self {
        Self::with_method(client, url, caller, reqwest::Method::HEAD)
    }

    /// Probe an unauthenticated `GET url`, for providers whose health
    /// endpoint does not answer `HEAD` (`CONTRACT.md` §6.2).
    pub fn get(client: reqwest::Client, url: impl Into<String>, caller: impl Into<String>) -> Self {
        Self::with_method(client, url, caller, reqwest::Method::GET)
    }

    fn with_method(
        client: reqwest::Client,
        url: impl Into<String>,
        caller: impl Into<String>,
        method: reqwest::Method,
    ) -> Self {
        Self {
            client,
            url: url.into(),
            caller: caller.into(),
            method,
            min_interval: None,
            display_target: None,
        }
    }

    /// A longer minimum interval for this hop, such as the 300 s of an
    /// external provider (`CONTRACT.md` §6.3 rule 4). `None` (the default)
    /// leaves the hop on the global interval.
    pub fn min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = Some(interval);
        self
    }

    /// Replace the `target` the hop reports, for a configured URL that
    /// embeds a key in its path or query: the request still goes to the
    /// configured URL, but the body shows `target` instead
    /// (`CONTRACT.md` §9). The replacement is sanitized like any target.
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.display_target = Some(target.into());
        self
    }
}

impl Probe for ReachProbe {
    fn run(&self) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + '_>> {
        Box::pin(async move {
            let Some(url) = request_url(&self.url) else {
                return invalid_url();
            };
            let response = match send_probe(
                self.client
                    .request(self.method.clone(), url)
                    .header(PROBE_HEADER, &self.caller),
            )
            .await
            {
                Ok(response) => response,
                Err(outcome) => return outcome,
            };
            let status = response.status().as_u16();
            if (500..=599).contains(&status) {
                ProbeOutcome::fail(CheckStatus::Unreachable, "upstream_5xx")
                    .with_http_status(status)
            } else {
                ProbeOutcome::ok().with_http_status(status)
            }
        })
    }

    fn target(&self) -> Option<String> {
        display_target(&self.display_target, &self.url)
    }

    fn dedupe_key(&self) -> Option<String> {
        Some(format!("{} {}", self.method, self.url))
    }

    fn min_interval(&self) -> Option<Duration> {
        self.min_interval
    }
}

/// The `target` of a probe whose configured URL may embed a secret: the
/// caller-provided display target when there is one, sanitized like any
/// target (`CONTRACT.md` §9); the configured URL otherwise.
fn display_target(override_target: &Option<String>, url: &str) -> Option<String> {
    match override_target {
        Some(target) => sanitize_target(target),
        None => sanitize_target(url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error chain with fixed messages, so [`connect_reason`] can be
    /// tested against the real shapes of resolver, TLS and socket errors
    /// without making network calls.
    #[derive(Debug)]
    struct Chained {
        message: &'static str,
        next: Option<Box<Chained>>,
    }

    impl Chained {
        /// Build a chain whose first message is `messages[0]` and whose
        /// `source()` is the rest, like a reqwest error wrapping a
        /// hyper-util error wrapping the OS error.
        fn chain(messages: &[&'static str]) -> Box<Chained> {
            let mut next = None;
            for &message in messages.iter().rev() {
                next = Some(Box::new(Chained { message, next }));
            }
            next.expect("at least one message")
        }
    }

    impl Error for Chained {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.next
                .as_ref()
                .map(|next| next as &(dyn Error + 'static))
        }
    }

    impl std::fmt::Display for Chained {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.message)
        }
    }

    #[test]
    fn connect_reason_recognises_dns() {
        // The Linux resolver message.
        let linux = Chained::chain(&[
            "error sending request for url (http://no-such.host/)",
            "client error (Connect)",
            "tcp connect error",
            "failed to lookup address information: Temporary failure in name resolution",
        ]);
        assert_eq!(connect_reason(&*linux), "dns");

        // The macOS resolver message.
        let macos = Chained::chain(&[
            "error sending request for url (http://no-such.host/)",
            "io: nodename nor servname provided, or not known",
        ]);
        assert_eq!(connect_reason(&*macos), "dns");

        // The hickory-resolver message.
        let hickory = Chained::chain(&[
            "client error (Connect)",
            "dns error: failed to lookup address information: ...",
        ]);
        assert_eq!(connect_reason(&*hickory), "dns");
    }

    #[test]
    fn connect_reason_recognises_tls() {
        let certificate = Chained::chain(&[
            "error sending request for url (https://host/)",
            "client error (Connect)",
            "invalid peer certificate: UnknownIssuer",
        ]);
        assert_eq!(connect_reason(&*certificate), "tls");

        let handshake = Chained::chain(&[
            "error sending request for url (https://host/)",
            "rustls: tls handshake eof",
        ]);
        assert_eq!(connect_reason(&*handshake), "tls");

        let rustls_code = Chained::chain(&[
            "error sending request for url (https://host/)",
            "InvalidCertificate(UnknownIssuer)",
        ]);
        assert_eq!(connect_reason(&*rustls_code), "tls");
    }

    #[test]
    fn connect_reason_defaults_to_connect() {
        let refused = Chained::chain(&[
            "error sending request for url (http://host:1/)",
            "client error (Connect)",
            "tcp connect error: Connection refused (os error 61)",
        ]);
        assert_eq!(connect_reason(&*refused), "connect");

        // An empty chain cannot say anything else either.
        let bare = Chained::chain(&["error sending request for url (http://host/)"]);
        assert_eq!(connect_reason(&*bare), "connect");
    }

    #[test]
    fn ping_body_classifies_identity() {
        let expected = Identity::new("keyServer", "development");

        // A matching identity is ok, with no reason.
        let ok = classify_ping_body(br#"{"service":"keyServer","env":"development"}"#, &expected);
        assert_eq!(ok.status, CheckStatus::Ok);
        assert_eq!(ok.reason, None);

        // Another service is wrong_service, checked before the env: this
        // body has both wrong.
        let wrong_service =
            classify_ping_body(br#"{"service":"authServer","env":"production"}"#, &expected);
        assert_eq!(wrong_service.status, CheckStatus::WrongTarget);
        assert_eq!(wrong_service.reason.as_deref(), Some("wrong_service"));
        assert_eq!(wrong_service.expected.as_ref(), Some(&expected));
        assert_eq!(
            wrong_service.observed,
            Some(Identity::new("authServer", "production"))
        );

        // The right service in the wrong env is wrong_env: dev pointed at
        // prod, or the reverse (CONTRACT.md §5.3).
        let wrong_env =
            classify_ping_body(br#"{"service":"keyServer","env":"production"}"#, &expected);
        assert_eq!(wrong_env.status, CheckStatus::WrongTarget);
        assert_eq!(wrong_env.reason.as_deref(), Some("wrong_env"));
        assert_eq!(
            wrong_env.observed,
            Some(Identity::new("keyServer", "production"))
        );

        // Not JSON, JSON without the fields, non-string fields, not an
        // object, empty: the SPA shell (CONTRACT.md §5.3).
        for body in [
            &b"<html>the spa shell</html>"[..],
            br#"{"service":"keyServer"}"#,
            br#"{"service":1,"env":"development"}"#,
            br#"[]"#,
            b"",
        ] {
            let bad = classify_ping_body(body, &expected);
            assert_eq!(bad.status, CheckStatus::Unexpected, "body {body:?}");
            assert_eq!(bad.reason.as_deref(), Some("bad_body"));
        }
    }

    #[test]
    fn jwks_key_counting() {
        // At least one key: ok.
        let ok = classify_jwks_body(br#"{"keys":[{"kty":"EC","crv":"P-256"}]}"#);
        assert_eq!(ok.status, CheckStatus::Ok);
        assert_eq!(ok.reason, None);

        // An empty keys array: no_keys.
        let empty = classify_jwks_body(br#"{"keys":[]}"#);
        assert_eq!(empty.status, CheckStatus::Unexpected);
        assert_eq!(empty.reason.as_deref(), Some("no_keys"));

        // Not JSON, no keys array, keys not an array: bad_body.
        for body in [
            &b"<html>not a jwks</html>"[..],
            br#"{"no_keys":[]}"#,
            br#"{"keys":"not-an-array"}"#,
            b"",
        ] {
            let bad = classify_jwks_body(body);
            assert_eq!(bad.status, CheckStatus::Unexpected, "body {body:?}");
            assert_eq!(bad.reason.as_deref(), Some("bad_body"));
        }
    }

    #[test]
    fn chain_id_hex_parsing() {
        assert_eq!(parse_chain_id("0x61"), Some(97));
        assert_eq!(parse_chain_id("0x1"), Some(1));
        assert_eq!(parse_chain_id("0x0"), Some(0));
        assert_eq!(parse_chain_id("0X61"), Some(97), "lenient prefix case");
        assert_eq!(parse_chain_id("0x"), None, "empty quantity");
        assert_eq!(parse_chain_id("zz"), None, "no 0x prefix");
        assert_eq!(parse_chain_id("0xzz"), None, "not hex");
        assert_eq!(parse_chain_id("0x-1"), None, "no sign");
        assert_eq!(parse_chain_id(""), None);
        assert_eq!(parse_chain_id("0xffffffffffffffffff"), None, "overflow");
    }

    #[test]
    fn chain_body_classifies_the_result() {
        // The configured chain: ok.
        let ok = classify_chain_body(br#"{"jsonrpc":"2.0","id":1,"result":"0x61"}"#, 97);
        assert_eq!(ok.status, CheckStatus::Ok);
        assert_eq!(ok.reason, None);

        // Another chain: wrong_target, wrong_chain (CONTRACT.md §6.2).
        let wrong = classify_chain_body(br#"{"jsonrpc":"2.0","id":1,"result":"0x38"}"#, 97);
        assert_eq!(wrong.status, CheckStatus::WrongTarget);
        assert_eq!(wrong.reason.as_deref(), Some("wrong_chain"));

        // No result string, a JSON-RPC error, not JSON: bad_body.
        for body in [
            &b"{}"[..],
            br#"{"result":56}"#,
            br#"{"result":"zz"}"#,
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#,
            b"<html>",
        ] {
            let bad = classify_chain_body(body, 97);
            assert_eq!(bad.status, CheckStatus::Unexpected, "body {body:?}");
            assert_eq!(bad.reason.as_deref(), Some("bad_body"));
        }
    }

    #[test]
    fn ping_url_joins_without_doubling_the_slash() {
        // A bare origin, with and without the trailing slash.
        for base in ["http://key-server:8092", "http://key-server:8092/"] {
            assert_eq!(
                ping_url(base).as_deref(),
                Some("http://key-server:8092/v1/ping"),
                "base {base}"
            );
        }
        // A base with a path prefix: the callee's BASE_PATH is part of the
        // address (CONTRACT.md §5.1).
        for base in ["http://gw.example/keys", "http://gw.example/keys/"] {
            assert_eq!(
                ping_url(base).as_deref(),
                Some("http://gw.example/keys/v1/ping"),
                "base {base}"
            );
        }
        // Userinfo, query and fragment never reach the request: a fleet
        // base URL is scheme://host[:port][/base_path], and a credential
        // hiding in one is not sent (CONTRACT.md §1.1).
        assert_eq!(
            ping_url("http://user:secret@h:1/keys?api_key=mxs_secret#frag").as_deref(),
            Some("http://h:1/keys/v1/ping")
        );
        // Anything that is not an absolute http(s) URL is misconfigured.
        for base in ["", "not a url", "key-server:8092", "ftp://h/"] {
            assert_eq!(ping_url(base), None, "base {base}");
        }
    }

    #[test]
    fn request_url_strips_credentials_and_keeps_the_query() {
        // Userinfo is a credential: a probe never sends it (CONTRACT.md
        // §1.1), even when the configured URL carries one.
        assert_eq!(
            request_url("http://user:secret@host.internal:8091/base").as_deref(),
            Some("http://host.internal:8091/base")
        );
        // The query is part of the address the request path uses; the
        // fragment is never sent over HTTP.
        assert_eq!(
            request_url("https://rpc.example/v1?key=k#frag").as_deref(),
            Some("https://rpc.example/v1?key=k")
        );
        // Not an absolute http(s) URL: misconfigured.
        for url in ["", "not a url", "db.internal:5432", "redis://h:6379"] {
            assert_eq!(request_url(url), None, "url {url}");
        }
    }
}
