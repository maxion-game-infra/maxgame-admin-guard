//! Runs every case in `../contract/cases.json` through the real
//! [`maxion_admin_guard::AdminGuard`] — the same eight rules
//! `../contract/README.md` describes, exercised end to end: offline JWKS
//! fetch and EdDSA verification against a real (mocked) HTTP JWKS endpoint,
//! and live introspection against a real (mocked) HTTP introspection
//! endpoint, stubbed per case exactly as `cases.json` specifies.
//!
//! The Ed25519 keys the cases are signed with live in
//! `../contract/fixtures/` (`jwks.json`, `signing-key.private.jwk.json`,
//! `wrong-key.private.jwk.json`) — test-only keys generated for this
//! contract, never used to sign anything real. Both this suite and the JS
//! package's suite sign against the same fixture, so a signature bug shows
//! up identically in either implementation. `jwks.json` is served to the
//! guard verbatim rather than re-derived, so a checked-in mismatch between
//! the private and public halves would fail this suite rather than go
//! unnoticed.
//!
//! `sequence` cases (the three breaker cases) drive the guard's
//! [`AdminIntrospectClient`] with a [`FakeClock`] behind its circuit
//! breaker, so `advanceMs` is a clock nudge rather than a real sleep.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use maxion_admin_guard::{
    AdminGuard, AdminIntrospectClient, AdminJwksClient, AdminJwksClientConfig, AdminTokenVerifier,
    FakeClock, GuardConfig, Requirement,
};
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CASES_PATH: &str = "../contract/cases.json";
const JWKS_FIXTURE: &str = "../contract/fixtures/jwks.json";
const SIGNING_KEY_FIXTURE: &str = "../contract/fixtures/signing-key.private.jwk.json";
const WRONG_KEY_FIXTURE: &str = "../contract/fixtures/wrong-key.private.jwk.json";

/// Kept short so the 12-odd simulated-timeout calls in the breaker sequences
/// do not turn this suite into a slow one. The mock server's delay for a
/// "timeout" case is comfortably longer than this.
const TEST_HTTP_TIMEOUT: Duration = Duration::from_millis(100);
const MOCK_TIMEOUT_DELAY: Duration = Duration::from_millis(600);
const DEFAULT_INTROSPECT_API_KEY: &str = "contract-test-key";
const DEFAULT_SUB: &str = "6c7b84de-9e3d-40ef-97eb-6bd47ae170a5";

fn load_json(path: &str) -> Value {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let text = std::fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", full.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", full.display()))
}

/// Load an Ed25519 signing key from a private JWK (RFC 8037: `d` is the
/// base64url, unpadded, 32-byte seed).
fn load_seed(fixture_path: &str) -> SigningKey {
    let fixture = load_json(fixture_path);
    let d = fixture["d"].as_str().expect("JWK `d` field");
    let bytes = URL_SAFE_NO_PAD
        .decode(d)
        .expect("JWK `d` must be valid base64url");
    let seed: [u8; 32] = bytes.as_slice().try_into().expect("seed must be 32 bytes");
    SigningKey::from_bytes(&seed)
}

/// A counter shared across a whole test run, so the final report states how
/// many individual expectations were actually checked, not just how many
/// top-level cases exist.
struct Tally {
    cases: usize,
    assertions: usize,
    failures: Vec<String>,
}

impl Tally {
    fn new() -> Self {
        Self {
            cases: 0,
            assertions: 0,
            failures: Vec::new(),
        }
    }

    fn check(&mut self, context: &str, condition: bool, detail: impl std::fmt::Display) {
        self.assertions += 1;
        if !condition {
            self.failures.push(format!("{context}: {detail}"));
        }
    }
}

/// Everything a case (or one step of a sequence case) needs to run once.
struct Fixture {
    good_key: SigningKey,
    wrong_key: SigningKey,
    kid: String,
    jwks_body: Value,
    top_config: Value,
}

impl Fixture {
    fn load() -> Self {
        let signing_fixture = load_json(SIGNING_KEY_FIXTURE);
        Self {
            good_key: load_seed(SIGNING_KEY_FIXTURE),
            wrong_key: load_seed(WRONG_KEY_FIXTURE),
            kid: signing_fixture["kid"].as_str().unwrap().to_string(),
            jwks_body: load_json(JWKS_FIXTURE),
            top_config: load_json(CASES_PATH)["config"].clone(),
        }
    }

    /// The checked-in `jwks.json`, served verbatim rather than re-derived
    /// from `good_key` — so a mismatch between the private and public
    /// fixture halves fails this suite instead of hiding behind a
    /// self-consistent re-derivation.
    fn jwks_body(&self) -> Value {
        self.jwks_body.clone()
    }

    /// Mint a JWT per a case's `token` spec. `null`/absent fields fall back
    /// to a well-formed default so a case only has to name what it breaks.
    fn mint(&self, spec: &Value) -> String {
        let sub = spec
            .get("sub")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_SUB);
        let role = spec.get("role").and_then(Value::as_str).unwrap_or("");
        let token_type = spec.get("type").and_then(Value::as_str).unwrap_or("admin");
        let site_access = spec.get("siteAccess").cloned().unwrap_or_else(|| json!({}));
        let iss = spec
            .get("iss")
            .and_then(Value::as_str)
            .unwrap_or_else(|| self.top_config["issuer"].as_str().unwrap())
            .to_string();
        let expired = spec
            .get("expired")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let exp = if expired { now - 60 } else { now + 900 };

        let mut payload = json!({
            "sub": sub,
            "iss": iss,
            "exp": exp,
            "role": role,
            "type": token_type,
            "siteAccess": site_access,
        });
        if let Some(aud) = spec.get("aud").and_then(Value::as_str) {
            payload["aud"] = json!(aud);
        }

        let mut header = json!({ "alg": "EdDSA", "typ": "JWT" });
        let omit_kid = spec
            .get("omitKid")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !omit_kid {
            let kid = spec.get("kid").and_then(Value::as_str).unwrap_or(&self.kid);
            header["kid"] = json!(kid);
        }

        let sign_with_wrong_key = spec.get("signWith").and_then(Value::as_str) == Some("wrong-key");
        let key = if sign_with_wrong_key {
            &self.wrong_key
        } else {
            &self.good_key
        };

        let encode = |v: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());
        let signing_input = format!("{}.{}", encode(&header), encode(&payload));
        let signature = key.sign(signing_input.as_bytes());
        format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        )
    }
}

/// A guard wired to two mock servers: JWKS (mounted once, stable for the
/// whole case) and introspection (its mocks reset and remounted per step,
/// so a sequence case can change the IdP's answer mid-sequence without
/// disturbing JWKS).
struct Rig {
    guard: AdminGuard,
    jwks_server: MockServer,
    introspect_server: MockServer,
    clock: Arc<FakeClock>,
}

impl Rig {
    async fn build(fixture: &Fixture, case: &Value) -> Self {
        let jwks_server = MockServer::start().await;
        let jwks_path = fixture.top_config["jwksPath"].as_str().unwrap();
        mount_jwks(&jwks_server, jwks_path, fixture.jwks_body(), case.get("jwks")).await;

        let introspect_server = MockServer::start().await;

        let config = build_config(fixture, case, &jwks_server, &introspect_server);

        let http = reqwest::Client::new();
        let mut jwks_config = AdminJwksClientConfig::new(config.jwks_url.clone());
        // Short enough that the "timeout" transport in a case's `jwks` spec
        // actually times out against MOCK_TIMEOUT_DELAY, rather than just
        // being a slow-but-successful fetch.
        jwks_config.http_timeout = TEST_HTTP_TIMEOUT;
        let jwks_provider = Arc::new(AdminJwksClient::new(jwks_config, http.clone()));
        let verifier = AdminTokenVerifier::new(jwks_provider, config.issuer.clone());

        let clock = Arc::new(FakeClock::new());
        let introspector = Arc::new(
            AdminIntrospectClient::new(
                http,
                config.introspect_url.clone(),
                config.introspect_api_key.clone(),
            )
            .with_header_name(config.introspect_header.clone())
            .with_timeout(TEST_HTTP_TIMEOUT)
            .with_breaker_clock(
                config.breaker_failure_threshold,
                config.breaker_open,
                clock.clone(),
            ),
        );

        let guard = AdminGuard::with_ports(config, verifier, introspector);

        Self {
            guard,
            jwks_server,
            introspect_server,
            clock,
        }
    }

    /// Replace whatever is mounted on the introspect server with `spec`,
    /// without touching the JWKS server.
    async fn stub_introspect(&self, fixture: &Fixture, spec: &Value) {
        self.introspect_server.reset().await;
        let introspect_path = fixture.top_config["introspectPath"].as_str().unwrap();

        if let Some(transport) = spec.get("transport").and_then(Value::as_str) {
            let template = match transport {
                "timeout" => ResponseTemplate::new(200).set_delay(MOCK_TIMEOUT_DELAY),
                "http" => {
                    let status = spec["status"].as_u64().expect("status for http transport") as u16;
                    ResponseTemplate::new(status)
                }
                "malformed" => ResponseTemplate::new(200).set_body_string("not-json"),
                other => panic!("unknown introspect transport '{other}'"),
            };
            Mock::given(method("POST"))
                .and(path(introspect_path))
                .respond_with(template)
                .mount(&self.introspect_server)
                .await;
        } else {
            Mock::given(method("POST"))
                .and(path(introspect_path))
                .respond_with(ResponseTemplate::new(200).set_body_json(spec.clone()))
                .mount(&self.introspect_server)
                .await;
        }
    }

    async fn introspect_call_count(&self) -> usize {
        self.introspect_server
            .received_requests()
            .await
            .unwrap()
            .len()
    }

    async fn jwks_call_count(&self) -> usize {
        self.jwks_server.received_requests().await.unwrap().len()
    }
}

/// Mount the JWKS endpoint per a case's optional `jwks` spec:
/// `{coldCache: true, transport: "timeout"|"http"|"malformed", status?: n}`.
/// Absent, or present with no `transport` (the kid-absent case, which needs
/// a *successful* fetch that just lacks the requested kid), it serves the
/// fixture key set verbatim as before. `coldCache` is asserted rather than
/// acted on: every case already gets a brand-new [`AdminJwksClient`] in
/// [`Rig::build`], so the cache is cold by construction and this harness has
/// no way to pre-warm it.
async fn mount_jwks(server: &MockServer, jwks_path: &str, fixture_body: Value, spec: Option<&Value>) {
    if let Some(spec) = spec {
        assert_eq!(
            spec.get("coldCache").and_then(Value::as_bool),
            Some(true),
            "jwks harness only supports coldCache: true (every case's cache starts empty already)"
        );
    }

    let template = match spec.and_then(|s| s.get("transport")).and_then(Value::as_str) {
        None => ResponseTemplate::new(200).set_body_json(fixture_body),
        Some("timeout") => ResponseTemplate::new(200).set_delay(MOCK_TIMEOUT_DELAY),
        Some("http") => {
            let status = spec.unwrap()["status"]
                .as_u64()
                .expect("status for http transport") as u16;
            ResponseTemplate::new(status)
        }
        Some("malformed") => ResponseTemplate::new(200).set_body_string("not-json"),
        Some(other) => panic!("unknown jwks transport '{other}'"),
    };

    Mock::given(method("GET"))
        .and(path(jwks_path))
        .respond_with(template)
        .mount(server)
        .await;
}

fn build_config(
    fixture: &Fixture,
    case: &Value,
    jwks_server: &MockServer,
    introspect_server: &MockServer,
) -> GuardConfig {
    let top = &fixture.top_config;
    let issuer = top["issuer"].as_str().unwrap().to_string();
    let introspect_header = top["introspectHeader"].as_str().unwrap().to_string();
    let breaker_threshold = top["breaker"]["failureThreshold"].as_u64().unwrap() as u32;
    let breaker_open_ms = top["breaker"]["openMs"].as_u64().unwrap();
    let mutating_methods: Vec<axum::http::Method> = top["mutatingMethods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| axum::http::Method::from_bytes(m.as_str().unwrap().as_bytes()).unwrap())
        .collect();

    let default_jwks_url = format!("{}{}", jwks_server.uri(), top["jwksPath"].as_str().unwrap());
    let default_introspect_url = format!(
        "{}{}",
        introspect_server.uri(),
        top["introspectPath"].as_str().unwrap()
    );

    let (jwks_url, introspect_url, introspect_api_key) = match case.get("config") {
        Some(over) => {
            let idp_base = over.get("idpBaseUrl").and_then(Value::as_str).unwrap_or("");
            let (jwks_url, introspect_url) = if idp_base.is_empty() {
                (String::new(), String::new())
            } else {
                let base = idp_base.trim_end_matches('/');
                (
                    format!("{base}{}", top["jwksPath"].as_str().unwrap()),
                    format!("{base}{}", top["introspectPath"].as_str().unwrap()),
                )
            };
            let api_key = over
                .get("introspectApiKey")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_INTROSPECT_API_KEY)
                .to_string();
            (jwks_url, introspect_url, api_key)
        }
        None => (
            default_jwks_url,
            default_introspect_url,
            DEFAULT_INTROSPECT_API_KEY.to_string(),
        ),
    };

    GuardConfig::new(issuer, jwks_url, introspect_url, introspect_api_key)
        .with_introspect_header(introspect_header)
        .with_mutating_methods(mutating_methods)
        .with_breaker(breaker_threshold, Duration::from_millis(breaker_open_ms))
}

fn requirement_from(case: &Value) -> Option<Requirement> {
    let require = case.get("require")?;
    if require.is_null() {
        return None;
    }
    Some(Requirement::new(
        require["site"].as_str().unwrap().to_string(),
        require["feature"].as_str().unwrap().to_string(),
    ))
}

fn method_from(case: &Value) -> axum::http::Method {
    axum::http::Method::from_bytes(case["method"].as_str().unwrap().as_bytes()).unwrap()
}

/// Resolve the bearer token for a step/case: `rawToken` wins if present
/// (the malformed-token case), else `token: null` means no credential at
/// all, else mint one from the spec.
fn bearer_for(fixture: &Fixture, case: &Value) -> Option<String> {
    if let Some(raw) = case.get("rawToken").and_then(Value::as_str) {
        return Some(raw.to_string());
    }
    match case.get("token") {
        None | Some(Value::Null) => None,
        Some(spec) => Some(fixture.mint(spec)),
    }
}

/// Check every field an `expect` object names against what actually
/// happened. Shared between plain cases and sequence steps.
async fn check_expectations(
    tally: &mut Tally,
    context: &str,
    expect: &Value,
    outcome: &Result<maxion_admin_guard::AuthorizedAdmin, maxion_admin_guard::GuardError>,
    rig: &Rig,
) {
    let actual_status = match outcome {
        Ok(_) => 200u16,
        Err(e) => e.status().as_u16(),
    };

    if let Some(expected) = expect.get("status").and_then(Value::as_u64) {
        tally.check(
            context,
            actual_status as u64 == expected,
            format!("expected status {expected}, got {actual_status}"),
        );
    }
    if let Some(expected_list) = expect.get("statusIn").and_then(Value::as_array) {
        let allowed: Vec<u64> = expected_list.iter().map(|v| v.as_u64().unwrap()).collect();
        tally.check(
            context,
            allowed.contains(&(actual_status as u64)),
            format!("expected status in {allowed:?}, got {actual_status}"),
        );
    }
    if let Some(forbidden) = expect.get("notStatus").and_then(Value::as_u64) {
        tally.check(
            context,
            actual_status as u64 != forbidden,
            format!("status must not be {forbidden}, got {actual_status}"),
        );
    }

    if let Some(expected) = expect.get("introspectCalled").and_then(Value::as_bool) {
        let actual = rig.introspect_call_count().await > 0;
        tally.check(
            context,
            actual == expected,
            format!("expected introspectCalled={expected}, got {actual}"),
        );
    }
    if let Some(expected) = expect.get("jwksFetched").and_then(Value::as_bool) {
        let actual = rig.jwks_call_count().await > 0;
        tally.check(
            context,
            actual == expected,
            format!("expected jwksFetched={expected}, got {actual}"),
        );
    }

    if let Some(needle) = expect.get("reasonContains").and_then(Value::as_str) {
        let message = match outcome {
            Ok(_) => String::new(),
            Err(e) => e.to_string(),
        };
        tally.check(
            context,
            message.contains(needle),
            format!("expected message to contain '{needle}', got '{message}'"),
        );
    }

    if let Some(expected) = expect.get("adminIdSet").and_then(Value::as_bool) {
        let actual = matches!(outcome, Ok(a) if !a.0.admin_id.trim().is_empty());
        tally.check(
            context,
            actual == expected,
            format!("expected adminIdSet={expected}"),
        );
    }

    if let Some(context_expect) = expect.get("context") {
        match outcome {
            Ok(authorized) => {
                if let Some(admin_id) = context_expect.get("adminId").and_then(Value::as_str) {
                    tally.check(
                        context,
                        authorized.0.admin_id == admin_id,
                        format!(
                            "expected context.adminId={admin_id}, got {}",
                            authorized.0.admin_id
                        ),
                    );
                }
                if let Some(role) = context_expect.get("adminRole").and_then(Value::as_str) {
                    tally.check(
                        context,
                        authorized.0.role == role,
                        format!(
                            "expected context.adminRole={role}, got {}",
                            authorized.0.role
                        ),
                    );
                }
                if let Some(present) = context_expect
                    .get("adminSiteAccessPresent")
                    .and_then(Value::as_bool)
                {
                    // `site_access` is a plain (never-optional) map in this
                    // crate's model, so it is always "present" — an empty
                    // map is still a map, never a missing field. This check
                    // exists so a future change that made it optional would
                    // have to consciously break this assertion.
                    tally.check(
                        context,
                        present,
                        "adminSiteAccessPresent is expected true (site_access is never optional in this crate)",
                    );
                }
            }
            Err(e) => tally.check(
                context,
                false,
                format!("expected a context (success) but got error: {e}"),
            ),
        }
    }
}

async fn run_plain_case(tally: &mut Tally, fixture: &Fixture, case: &Value) {
    let id = case["id"].as_str().unwrap();
    let rig = Rig::build(fixture, case).await;

    if let Some(introspect_spec) = case.get("introspect") {
        rig.stub_introspect(fixture, introspect_spec).await;
    }

    let method = method_from(case);
    let bearer = bearer_for(fixture, case);
    let require = requirement_from(case);

    let outcome = rig
        .guard
        .authorize(&method, bearer.as_deref(), require.as_ref())
        .await;

    check_expectations(tally, id, &case["expect"], &outcome, &rig).await;
}

async fn run_sequence_case(tally: &mut Tally, fixture: &Fixture, case: &Value) {
    let id = case["id"].as_str().unwrap();
    let rig = Rig::build(fixture, case).await;
    let require = requirement_from(case);
    let bearer = bearer_for(fixture, case);

    for (i, step) in case["sequence"].as_array().unwrap().iter().enumerate() {
        if let Some(advance_ms) = step.get("advanceMs").and_then(Value::as_u64) {
            rig.clock.advance(Duration::from_millis(advance_ms));
            continue;
        }

        let repeat = step.get("repeat").and_then(Value::as_u64).unwrap_or(1);
        let method =
            axum::http::Method::from_bytes(step["method"].as_str().unwrap().as_bytes()).unwrap();

        for rep in 0..repeat {
            if let Some(introspect_spec) = step.get("introspect") {
                rig.stub_introspect(fixture, introspect_spec).await;
            }

            let outcome = rig
                .guard
                .authorize(&method, bearer.as_deref(), require.as_ref())
                .await;

            let context = format!("{id} step {i} rep {rep}");
            check_expectations(tally, &context, &step["expect"], &outcome, &rig).await;
        }
    }
}

#[tokio::test]
async fn every_contract_case_passes() {
    let cases_doc = load_json(CASES_PATH);
    let fixture = Fixture::load();
    let cases = cases_doc["cases"].as_array().expect("cases array");

    let mut tally = Tally::new();
    let mut seen_ids: HashMap<&str, ()> = HashMap::new();

    for case in cases {
        let id = case["id"].as_str().expect("case id");
        assert!(
            seen_ids.insert(id, ()).is_none(),
            "duplicate case id '{id}' in cases.json"
        );

        tally.cases += 1;
        if case.get("sequence").is_some() {
            run_sequence_case(&mut tally, &fixture, case).await;
        } else {
            run_plain_case(&mut tally, &fixture, case).await;
        }
    }

    println!(
        "contract suite: {} cases, {} individual expectations checked, {} failed",
        tally.cases,
        tally.assertions,
        tally.failures.len()
    );

    assert!(
        tally.failures.is_empty(),
        "\n{} of {} expectation checks failed across {} cases:\n{}",
        tally.failures.len(),
        tally.assertions,
        tally.cases,
        tally.failures.join("\n")
    );
}
