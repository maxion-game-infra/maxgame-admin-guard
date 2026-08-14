# PLATFORM.md — wire contract for the Maxion admin platform

Convention over code (plan ADR D0): `maxion-admin-guard` is the only shared
*code* on this platform. Everything else that needs to match across services
matches because this document says so, because each repo ships its own
conformance test against it, and because `template/` (M6) starts new services
already compliant. Nothing here is enforced by a shared crate — a repo that
drifts from this document is a bug in that repo, caught by its own test, not
a version bump this document can force.

Scope: the eight admin-facing services behind `web-platform-back-office`.
Player-facing / non-admin surfaces (e.g. `maxgame-auth-server`'s player API)
are out of scope except where they double as an admin surface.

| Key | Service | Repo |
|---|---|---|
| `idp` | Admin identity provider | `maxgame-admin-auth-server` |
| `authServer` | Player IdP + admin proxy (**FROZEN** — deployed to prod) | `maxgame-auth-server` |
| `keyServer` | S2S service-key issuance + verification | `maxgame-key-server` |
| `launcher` | Launcher/games/downloads admin | `maxgame-launcher-backend` |
| `news` | News admin | `maxgame-news-backend` |
| `web` | Careers + user-reports admin | `maxgame-web-backend` |
| `utility` | R2 presign (partner + admin) | `maxgame-utility-server` |
| `api` | Legacy remainder (zone4, email relay, user stats) | `web-platform-backend` (NestJS, being retired) |

Every JSON example below is copied from a real source file, cited inline. A
`// TARGET (M2)` comment on an example means it does not exist yet — it is
what the repo named must produce once its M2 work lands, not a description
of current behaviour.

---

## 1. Error envelope (ADR D1)

### 1.1 The shape

```json
{ "statusCode": 403, "message": "Access denied. Required feature permission is missing.", "error": "Forbidden" }
```

Source: `maxgame-launcher-backend/src/inbound/error.rs` (`ErrorBody`) — this
is NestJS's own shape, copied on purpose because every existing client
(desktop launcher, back office, mu-landing, two CI tools) already parses it.
**`maxgame-news-backend/src/inbound/error.rs`** and
**`maxgame-web-backend/src/inbound/error.rs`** use the identical struct,
byte for byte (`#[serde(rename = "statusCode")]`, `message: String`,
`error: &'static str`) — 3 of the 4 Rust services already conform.

A 4th field is **additive and optional**:

```json
{ "statusCode": 422, "message": "unknown bucket 'typo-bucket'", "error": "Unprocessable Entity", "code": "UNPROCESSABLE_ENTITY" }
```

`code` is a stable, machine-readable string a client can safely `switch` on
(`message` may change wording; `code` must not). No repo emits `code` today.
When a repo adds it, reuse the `error_code` catalog utility already built
(§1.3) rather than inventing a second vocabulary.

### 1.2 Status → reason phrase

| Status | `error` | Used by |
|---|---|---|
| 400 | `Bad Request` | all |
| 401 | `Unauthorized` | all |
| 403 | `Forbidden` | all |
| 404 | `Not Found` | all |
| 409 | `Conflict` | launcher (`key-server` uses its own ad-hoc 409 today — see §1.4) |
| 422 | `Unprocessable Entity` | utility, news, key-server |
| 429 | `Too Many Requests` (+ `Retry-After` header, seconds) | launcher |
| 503 | `Service Unavailable` | all (admin-guard outage, upstream dependency down) |
| 500 | `Internal Server Error`, body always reads `"message": "Internal server error"` | all — internal detail goes to the log (`tracing::error!`), never the response |

A 503's `message` **is** allowed to say what's unavailable (it's actionable —
retry later); a 500's `message` never is. Source: launcher
`src/inbound/error.rs` tests `a_500_never_leaks_its_internal_detail` /
`a_503_does_say_what_is_unavailable`.

### 1.3 `code` catalog (recommended, from utility's existing `error_code`)

| `code` | Status |
|---|---|
| `BAD_REQUEST` | 400 |
| `AUTHENTICATION_REQUIRED` | 401 |
| `ACCESS_DENIED` | 403 |
| `RESOURCE_NOT_FOUND` | 404 |
| `UNPROCESSABLE_ENTITY` | 422 |
| `RATE_LIMIT_EXCEEDED` | 429 |
| `SERVICE_UNAVAILABLE` | 503 |
| `INTERNAL_ERROR` | 500 |
| `CONFLICT` | 409 |

Source: `maxgame-utility-server/src/inbound/error.rs` (`DomainError::error_code`) for the first eight. `CONFLICT` was added by `maxgame-key-server` (`src/error.rs`, `src/routes/admin_keys.rs` — its one 409 case, "key is already revoked") during M2, since the original eight had no 409 entry; sanctioned as a contract amendment rather than a repo-local invention, so the next repo with a 409 reuses it instead of picking its own string.

### 1.4 Non-conformance found and fixed during M2/M3

These were identified while writing this document (M1) or during M2 implementation, and closed as part of the convergence work rather than left open:

- **utility** answered `{"error": {"code": 422, "message": "…", "error_code": "UNPROCESSABLE_ENTITY"}}` (nested, nod to `web-cs-backend`'s shape) before M2 — `back-office`'s `messageFrom()` could not read a nested `error.message`, so every utility error rendered as "Something went wrong!". Fixed: flattened to §1.1's shape, `error_code`'s values carried over verbatim into the new `code` field.
- **key-server**'s admin-keys errors (`AdminKeysError`) were ad hoc and matched neither shape: `{"error": "unknown_scope", "scope": "..."}`, `{"error": "invalid_grace_hours", "message": "..."}`, `{"error": "already_revoked"}` — no `statusCode` field at all, and a `500` from `AdminKeysError::Db` returned a bare status with no body. Fixed: same §1.1 envelope as the rest of the fleet, via the shared `error_response()` builder in `src/error.rs`.
- **idp**'s non-OAuth admin routes (e.g. `/api/v1/me`, `/api/v1/admins`) answered `{"error": "<snake_case code>", "message": "<detail>"}` — no `statusCode`, and `error` carried a machine code rather than the HTTP reason phrase. Not caught while writing this document (a miss in M1, not something the M2 convergence commit introduced); found while writing idp's M5 conformance test, reported, and fixed the same day — `code` is now where the machine code lives, additive per §1.1.

### 1.5 Documented exceptions

- **idp OAuth endpoints** (`/api/v1/oauth/*`) keep RFC 6749's
  `{"error": "...", "error_description": "..."}` — this is a spec, not a
  house style, and every OAuth client in the wild expects it. idp's own
  *non*-OAuth REST routes (e.g. `/api/v1/admin/...`) should still use §1.1;
  converging them is optional cleanup, not required by this contract.
- **web-cs pair** (`web-cs-backend` / `web-cs-back-office`) keeps its own
  nested shape — different contract, different repo pair, out of scope here.
- **NestJS `api`** keeps whatever it already emits (it *is* the origin of
  §1.1's shape) — it's being retired, not migrated.

---

## 2. Pagination (ADR D2)

### 2.1 Admin: `page`/`take` query, `{items, meta}` response

```json
// GET /admin/news?page=2&take=3
{
  "items": [ /* NewsEntity[] */ ],
  "meta": {
    "page": 2,
    "take": 3,
    "itemCount": 13,
    "pageCount": 5,
    "hasNextPage": true,
    "hasPreviousPage": true
  }
}
```

Source: `maxgame-news-backend/src/modules/news_admin/dto.rs` (`ListMeta`,
camelCase on the wire). `pageCount = ceil(itemCount / take) || 1` — an empty
result still reports one page, not zero (`ListMeta::new`, same file). This is
the shape every new admin list endpoint should produce.

### 2.2 Public: cursor/`limit`

Used by public (non-admin-JWT) list endpoints. No single canonical shape is
pinned across repos yet — new public list endpoints should follow whichever
of `{items, nextCursor, hasMore}` (§2.4) or an equivalent cursor pattern the
owning repo already uses elsewhere, and record it here once written.

### 2.3 Non-conformance found (M2 work)

**key-server**'s admin list endpoints answer a **bare array** with
`limit`/`offset` query params, not `page`/`take` + `{items, meta}`:

```json
// GET /v1/admin/keys?limit=100&offset=0   — CURRENT
[ /* KeyRecord[] */ ]
```

Source: `maxgame-key-server/src/routes/admin_keys.rs:142-162` (`list_keys`,
`ListKeysQuery { consumer, limit, offset }`). Same issue on
`GET /v1/admin/audit-logs`. M2 target: `page`/`take` query params, response
`{items: KeyRecord[], meta: ListMeta}` per §2.1, plus a `COUNT` query to
populate `itemCount`/`pageCount` (small table, cost is acceptable — noted in
the plan). Neither this endpoint nor its SPA caller (`sa-service-keys`) is
deployed, so this is free to change.

### 2.4 Documented exceptions

- **`authServer` (FROZEN — deployed to prod):**

  ```json
  // GET /v1/admin/players?page=2&per_page=50
  { "items": [], "page": 2, "per_page": 50, "total": 0, "total_pages": 0 }
  ```

  Source: `maxgame-auth-server/src/interface/pagination.rs` (`PageQuery`)
  and `src/application/pagination.rs` (`Page<T>`). Field names are
  `page`/`per_page`/`total`/`total_pages`, not `page`/`take`/`itemCount`/
  `pageCount`. **Do not touch** — this endpoint is live in production.

- **`web` admin user-reports** keeps cursor/`limit`, not `page`/`take`:

  ```json
  // GET /admin/user-reports?limit=20&cursor=<uuid>
  { "items": [ /* UserReportResponse[] */ ], "nextCursor": "3f6c2a1e-…", "hasMore": true }
  ```

  Source: `maxgame-web-backend/src/modules/user_reports/dto.rs`
  (`RawListQuery { site, limit, cursor }`, `UserReportListResponse { items,
  next_cursor, has_more }` — the struct fields are snake_case but
  `#[serde(rename_all = "camelCase")]` makes the wire shape `nextCursor`/
  `hasMore`, consistent with every other camelCase response on this
  platform). The back office's user-reports page is a
  load-more UI (`sections/max-user-report/view/user-report-view.tsx`), so
  converging to a page-number UI would be a UI rework, not a wire-format
  fix — deferred to plan follow-up §7 item 7. `web`'s **careers** admin list
  (the other module in this repo) is unaffected and should follow §2.1.

---

## 3. Env / ports / health / CORS

### 3.1 Ports and health

| Service key | Port | Health (liveness) | Readiness | Repo |
|---|---|---|---|---|
| `idp` | 8091 | `/healthz` | `/readyz` | `maxgame-admin-auth-server` |
| `keyServer` | 8090 | `/healthz` | `/readyz` (**dev.sh gates boot on this one deliberately** — see below) | `maxgame-key-server` |
| `authServer` | 4000 | `/healthz` | `/readyz` | `maxgame-auth-server` |
| `launcher` | 8092 | `/healthz` | `/readyz` | `maxgame-launcher-backend` |
| `news` | 8093 | `/healthz` | `/readyz` (**waits on a background migration** — see below) | `maxgame-news-backend` |
| `web` | 8094 | `/healthz` | `/readyz` | `maxgame-web-backend` |
| `utility` | 8095 | `/healthz` | `/readyz` | `maxgame-utility-server` |
| `api` | 8080 | `/health` (legacy name, not `/healthz` — not converging, being retired) | — | `web-platform-backend` |
| SPA | 5173 | `/` | — | `web-platform-back-office` |

Source: `back-office-workspace/.scripts/dev.sh:34-44` (`SERVICES` array,
which is the definition of "does the local stack boot" — every port above
was read straight out of it, not inferred).

**Rule**: every service serves `/healthz` (liveness — "did the process come
up") and `/readyz` (readiness — "can serve traffic") **at the root**, always,
even once `BASE_PATH` (§5) is set — a k8s probe hits the pod directly, never
through the gateway prefix.

`dev.sh` gates `make up` on `/healthz` for every service *except*
`key-server`, which it gates on `/readyz` on purpose (comment at
`dev.sh:24-32`): `news` applies its migrations in the background and reports
unready until they land, so gating the whole stack's boot on `/readyz` would
stall unrelated services behind one service's schema work, and the 60s
`wait_healthy` timeout would then fail services that have nothing to do with
the slow migration. This is a documented, intentional divergence — not a bug
to fix in M2.

### 3.2 CORS

Standard env var: **`CORS_ALLOWED_ORIGINS`** (comma-separated, explicit
origins only — a literal `*` refuses to boot in every non-dev repo that
validates it; empty is refused outside development too).

| Repo | Current env var | Status |
|---|---|---|
| `idp` | `CORS_ALLOWED_ORIGINS` | ✅ already standard |
| `keyServer` | `ADMIN_CORS_ALLOWED_ORIGINS` | ❌ M2: rename to `CORS_ALLOWED_ORIGINS` — this is an admin-only service, not a dual-surface one, so it does **not** qualify for the `authServer` exception below |
| `launcher` | `CORS_ALLOWED_ORIGINS` | ✅ already standard |
| `news` | `ADMIN_CORS_ALLOWED_ORIGINS` | ❌ M2: rename to `CORS_ALLOWED_ORIGINS` |
| `web` | `CORS_ORIGIN` | ❌ M2: rename to `CORS_ALLOWED_ORIGINS` |
| `utility` | `CORS_ALLOWED_ORIGINS` | ✅ already standard |
| `authServer` | (its own admin-subtree origin config) | **exception**: a dual-surface service (player API + admin proxy) may additionally use `ADMIN_CORS_ALLOWED_ORIGINS` for the admin subtree — this is the one legitimate use of that name on the whole platform |

Source: `grep -n allow_headers\|CORS_ORIGIN\|ADMIN_CORS_ALLOWED_ORIGINS` across
`news/src/config.rs`, `web-backend/src/config.rs`, `key-server/src/config.rs`.

**`allow_headers`** — target: `authorization, content-type, accept,
x-request-id` (+ per-repo extras where a repo genuinely needs one, e.g.
utility's partner endpoint credential header).

| Repo | Current `allow_headers` | Has `x-request-id`? |
|---|---|---|
| `news` | `authorization, content-type, accept` | ❌ |
| `web` | `authorization, content-type, accept` | ❌ |
| `launcher` | `authorization, content-type, accept` | ❌ |
| `keyServer` | `authorization, content-type` (missing `accept` too) | ❌ |
| `utility` | `authorization, content-type, accept, x-api-key` | ❌ |

Source: `router.rs`/`app.rs` `.allow_headers([...])` calls in each repo
(`news/src/inbound/router.rs:114`, `web-backend/src/inbound/router.rs:61`,
`launcher/src/inbound/router.rs:76`, `key-server/src/app.rs:100`,
`utility/src/inbound/router.rs:88-94`). **No repo allows `x-request-id`
today** — this must land in M2 before the SPA can attach the header in M4
(D5), or every preflight fails closed.

### 3.3 IdP consumer env set

Every Rust service that verifies admin tokens (i.e. every one except `idp`
itself, which *is* the IdP) reads:

| Env var | Required? | Default |
|---|---|---|
| `ADMIN_IDP_BASE_URL` | yes | — |
| `ADMIN_JWT_ISSUER` | yes | — (byte-compared against the token's `iss`, not derived from the base URL) |
| `ADMIN_JWKS_URL` | no | `{ADMIN_IDP_BASE_URL}/.well-known/jwks.json` |
| `ADMIN_INTROSPECT_API_KEY` | yes once the repo introspects on mutations; optional otherwise | — |
| `ADMIN_INTROSPECT_PATH` | no | `/api/v1/oauth/introspect` |

Already correct, verbatim, in `utility` (`.env.example:20-30`, though
`ADMIN_INTROSPECT_API_KEY` is presently optional there — becomes required
once M3 closes utility's introspect gap), `launcher` (`.env.example:19-29`,
including the `ADMIN_INTROSPECT_PATH` note that "the legacy backend serves
`/admin/introspect`"), `news`, and `web` (both confirmed in `src/config.rs`
reading exactly this five-variable set, `ADMIN_INTROSPECT_API_KEY` required).

**key-server does not conform**: it reads `IDP_JWKS_URL`, `IDP_ISSUER`,
`IDP_BASE_URL` (`.env.example:2-8`) instead of the `ADMIN_IDP_*` family.
M2: rename `IDP_*` → `ADMIN_IDP_*` (`ADMIN_INTROSPECT_API_KEY` there is
already correctly named).

**Issuer-side exemption**: `idp` itself (`maxgame-admin-auth-server`) uses
plain `JWT_ISSUER` and `INTROSPECT_API_KEY` (no `ADMIN_` prefix) — source
`config.rs:174-176,198` and `.env.example:17,31`. This is deliberate: the
issuer is not a consumer of its own tokens in the same sense, and renaming
it would be a distinction without a difference. Do not "fix" this in M2.

**No audience claim, anywhere.** `idp` mints no `aud`, and no verifier should
require one. `maxgame-key-server/deploy/README.md:22` still documents an
`IDP_AUDIENCE` variable ("Expected `aud` claim on admin JWTs. Unset or blank
means the audience is not enforced.") — this is doc drift for a variable
that does nothing; M2 removes the doc line (and the equivalent line in
`openapi.rs` if present). `idp`'s own `.env.example` was checked and does
**not** currently list a stray `GOOGLE_ALLOWED_HD` — if the audit finding
that flagged one is still live, it is in a real (gitignored) `.env`, not the
template; sweep for it in M2 rather than assuming it is already gone.

**`idp`'s own default port is wrong**: `.env.example:7` and `config.rs:167`
both default `PORT` to `8090` — that's `keyServer`'s port (§3.1 says `idp` is
8091). M2: change the default to 8091 in both places.

### 3.4 `AppEnv`: three tiers, guardrails keyed off `!is_dev()`

```rust
pub enum AppEnv { Development, Staging, Production }
impl AppEnv {
    pub fn is_dev(&self) -> bool { matches!(self, AppEnv::Development) }
    pub fn is_production(&self) -> bool { matches!(self, AppEnv::Production) }
}
```

Source: `maxgame-utility-server/src/config.rs:40-94` — this is the reference
implementation. The load-bearing rule, stated in that file's own doc comment:
**every deployment guardrail must be keyed on `!app_env.is_dev()`, never on
`is_production()`** — a check written "production only" silently exempts
staging, which is exactly where a misconfiguration gets found first. `APP_ENV`
is *required*, no default, so an omitted variable fails loudly instead of
quietly inheriting the relaxed (dev) rules. Accepted spellings:
`production`/`prod`, `staging`/`uat`, `development`/`dev`/`local` — `"test"`
is deliberately **not** accepted (it reads as a deployed QA environment at
least as often as a laptop, and guessing wrong would hand an internet-facing
box the relaxed rules).

`idp` (`maxgame-admin-auth-server/src/config.rs:22-44`) currently has only
**two** tiers (`Development`, `Production`), defaulting unset `APP_ENV` to
`Development` — so **staging currently gets dev's relaxed rules there**. M2:
adopt utility's three-tier model, `APP_ENV` becomes required (no default).
`launcher`/`news`/`web` need the same audit — confirm each already matches
utility's model before M2 closes; `news` and `web` were not checked line by
line for this while writing this document, only for their CORS/IdP env names.

---

## 4. Admin authentication

The eight-rule contract lives at [`contract/README.md`](./README.md) in this
repo — offline EdDSA verification every request, live introspection on
mutations, the fail-closed 401-vs-503 split, the circuit breaker, etc. This
document does not repeat those rules; it only tracks one open gap against
them:

**`utility` does not introspect on mutations** (see plan ADR D4.1 for the
full reasoning on why this is being closed rather than kept as a documented
exception). Its current in-code ADR at
`maxgame-utility-server/src/infrastructure/admin_auth.rs:9-24` argues the
opposite — parity with the NestJS endpoint it replaces, and not wanting
uploads to depend on IdP availability — and **that comment must be rewritten
to match the M3 decision** once introspection lands, so the code and this
contract don't say opposite things. Until M3 ships, `utility` is a
documented, temporary exception to rule 4; after M3 it is not, and this
paragraph should be deleted.

---

## 5. Gateway-ready: `BASE_PATH` (ADR D6)

Deploy target: every service behind one ingress-nginx host,
`api.maxiondev.com`, path-routed per service. The **prefix lives in the
service**, not in an ingress rewrite — each service reads an optional
`BASE_PATH` env var (default empty = today's behaviour, mounted at `/`) and
nests its whole router under it (axum `Router::nest(base_path, app)`).

### 5.1 Path map

| Path | Service key | Repo |
|---|---|---|
| `/admin-auth` | `idp` | `maxgame-admin-auth-server` |
| `/auth` | `authServer` | `maxgame-auth-server` (additive at the ingress; the existing `account.*` host keeps working unchanged — see §5.4) |
| `/keys` | `keyServer` | `maxgame-key-server` |
| `/launcher` | `launcher` | `maxgame-launcher-backend` |
| `/news` | `news` | `maxgame-news-backend` |
| `/web` | `web` | `maxgame-web-backend` |
| `/utility` | `utility` | `maxgame-utility-server` |
| `/platform` | `api` | `web-platform-backend` (temporary — strip-prefix at the ingress instead of a code change, since this service is being retired) |
| (not on the gateway) | — | `maxgame-email-server` (stays on Cloud Run, `mailer.*`) |

### 5.2 `BASE_PATH` contract

- Env var: `BASE_PATH`, optional, default `""` (unset = mounted at root,
  today's behaviour — nothing changes for a repo that never sets it).
- The whole application router nests under it: `Router::nest(base_path, app)`.
- **`/healthz` and `/readyz` are served at the root always**, `BASE_PATH` or
  not — a k8s liveness/readiness probe hits the pod directly, never through
  the ingress prefix (§3.1's rule restated here because it's the reason
  `BASE_PATH` doesn't just wrap literally everything).
- OpenAPI/Swagger must reflect the prefix when one is set (the served paths
  in the document should match what a client actually calls).
- Per-repo M2 test: boot with `BASE_PATH=/x`, assert every existing route
  answers under `/x/...` and `/healthz`+`/readyz` still answer at the root.

### 5.3 Why prefix-in-service, not rewrite-at-ingress

A `rewrite-target` regex at the ingress (`/$2`) mangles `Location` headers,
relative redirects, and Swagger's own path — and ties the contract to an
nginx annotation instead of something a service can test locally without a
gateway in front of it. `BASE_PATH` makes each service correct on its own;
the NestJS `/platform` mapping uses the rewrite approach anyway, precisely
*because* it's temporary and nobody wants to touch code in a service that's
being deleted.

### 5.4 SPA impact: none

The back office's `axios.ts` already concatenates `baseURL + path` per
service instance — nothing in the SPA's code changes. Only the deployed
`public/config.json` changes, e.g. `"news": "https://api.maxiondev.com/news"`
instead of `"news": "http://localhost:8093"`. Proving this is exactly the
plan's verification step: run one service locally with `BASE_PATH=/news` set
and point a local `config.json` at `http://localhost:8093/news` — if the
news list page still works, deployed config is confirmed to be nothing more
than swapping a JSON file.

**`authServer` stays FROZEN**: adding `/auth` at the ingress is additive
(the existing `account.*` host is untouched), but `BASE_PATH` support inside
`maxgame-auth-server` itself is a follow-up, not part of this plan.

---

## 6. Service-to-service: key-server `/v1/verify` (ADR D7)

Every **new** S2S integration on this platform authenticates via a
`mxs_...` key issued by `maxgame-key-server` and verified through this one
endpoint. Existing legacy secrets (§6.3) are not being migrated by this
plan — they're catalogued here so the eventual migration has a map.

### 6.1 Request / response

```json
// POST {keyServerBaseUrl}/v1/verify
// header: x-verifier-service: <caller's own name>   (never send the key in a header, only the body)
{ "key": "mxs_...", "required_scopes": ["utility:partner-upload"] }
```

Always answers **200** — this is possession-based verification, not
authorization by HTTP status. The `active` field is what a caller branches
on:

```json
// 200 — key valid and holds every requested scope
{ "active": true, "key_id": "3f6c2a1e-…", "consumer": "maxgame-website", "scopes": ["utility:partner-upload"], "metadata": {}, "expires_at": null }
```
```json
// 200 — denied; reason is one of revoked | expired | not_found | missing_scope
{ "active": false, "reason": "missing_scope" }
```

Source: `maxgame-key-server/src/routes/verify.rs` (`VerifyResponse`,
`#[serde(untagged)]`) and `src/services/verify.rs` (`DenyReason::as_str()`
pins the four wire values — never rename them without a client sweep).
`not_found` covers both a malformed key (doesn't start with `mxs_`) and a
key that simply doesn't exist in the DB, and is **deliberately not audited**
— auditing a guess would let anyone flood `key_audit` by probing keys.

### 6.2 Fail-closed, on the caller's side

A verifier must treat anything that isn't a clean 200-with-parseable-body as
"cannot verify" (503), never as an implicit pass or a 401. Reference client:
`maxgame-utility-server/src/adapters/key_server.rs` — timeout, transport
error, non-2xx, and an unparseable body all map to the same
`DomainError::Unavailable("unable to verify the service key with
maxgame-key-server")`. Only a clean `{"active": false, ...}` is a denial.

### 6.3 Scope catalog

```
platform:introspect          platform:release-upload      platform:partner-upload
platform:presale-reconcile   platform:coupon-pipeline      email:send
email:admin                  cs:jobs                       authserver:games:read
utility:partner-upload
```

Source: `maxgame-key-server/src/domain/scopes.rs` (`SCOPE_CATALOG`) —
the live, enforced list (`is_known_scope`).

### 6.4 Legacy S2S secrets registry (not migrated by this plan)

| Secret | Header | Repo(s) it protects | Target key-server scope |
|---|---|---|---|
| `LAUNCHER_RELEASE_API_KEY` / `GAME_RELEASE_API_KEY` | `x-release-api-key` | launcher (the two `ci-register` routes) | `platform:release-upload` |
| `LAUNCHER_COUPONS_PIPELINE_SECRET` | `x-pipeline-secret` | launcher (mu-alpha-pipeline coupon routes) | `platform:coupon-pipeline` |
| `DOWNLOAD_APP_KEYS` (JSON map) | `X-Download-App-Key` | launcher (download-token minting) | not yet scoped — per-app, not per-service |
| `ADMIN_API_KEYS` (env) / DB-backed key | `X-Admin-Key` | `maxgame-auth-server` (dual-accept alongside admin JWT — `X-Admin-Key` wins if present; see `src/interface/middleware/admin.rs:1-21`) | `authserver:games:read` already exists in the catalog (§6.3) as the landing spot |
| (env-configured admin key) | `x-admin-key` | `maxgame-email-server` | not yet scoped |

None of these are touched by this plan (plan §7 follow-up item 4). They are
recorded so a future migration doesn't have to rediscover them.

---

## 7. Minimum conformance assertions per repo

Each repo below writes and owns its own test for these — no shared test
harness, per D0. This is the checklist a repo's conformance test (M5) must
cover; it should be concrete enough to write from directly.

**Every Rust repo (6): idp, keyServer, launcher, news, web, utility**

- [ ] Every error response is `{statusCode, message, error}` (§1.1); a 500's
      `message` is always exactly `"Internal server error"`; a 429 (where
      applicable) carries `Retry-After` in seconds.
- [ ] `GET /healthz` and `GET /readyz` both exist at the root and return 2xx
      when the service is healthy, independent of `BASE_PATH`.
- [ ] Booting with `BASE_PATH=/x` set: every existing route answers under
      `/x/...`, and `/healthz`+`/readyz` still answer at the root (unset
      `BASE_PATH` must be behaviourally identical to today).
- [ ] CORS: `allow_headers` includes `authorization, content-type, accept,
      x-request-id` (plus any repo-specific extra, e.g. utility's
      `x-api-key`); a literal `*` in `CORS_ALLOWED_ORIGINS` refuses to boot;
      an empty allowlist refuses to boot outside `Development`.
- [ ] `APP_ENV` is required (no silent default to `Development`); `staging`
      and `uat` both parse to `AppEnv::Staging`, not `Development`; every
      deployment guardrail (Swagger off, HTTPS JWKS, non-empty CORS
      allowlist, etc.) fires identically for `staging` and `production` —
      i.e. is keyed off `!is_dev()`, verified by a test that runs the same
      guardrail assertions against both tiers.
- [ ] Admin-auth: a request with no `Authorization` header is 401; a request
      whose token fails offline verification (bad `kid`, expired, wrong
      `iss`) is 401; an unreachable/erroring JWKS endpoint is 503, not 401
      (contract rule 5) — see `contract/cases.json` in this repo for the
      full case set.

**idp specifically**

- [ ] `PORT` defaults to 8091, not 8090.
- [ ] Every JWT/JWKS shape idp actually mints, run through
      `maxion_admin_guard::AdminTokenVerifier`, verifies successfully (plan
      D4.2 — the issuer-conformance test); mutating one field of the minted
      token must make that same test fail (proves the test isn't vacuous).
- [ ] No `aud` claim is ever minted.
- [ ] idp's own non-OAuth admin routes (e.g. `/api/v1/me`) answer §1.1's
      envelope, not a two-field `{error, message}` shape — only idp's OAuth
      endpoints are exempt (§1.5).

**keyServer specifically**

- [ ] `GET /v1/admin/keys` and `GET /v1/admin/audit-logs` accept `page`/
      `take`, not `limit`/`offset`, and answer `{items, meta}` (§2.1), not a
      bare array.
- [ ] A 429 from the `/v1/verify` rate limiter carries `Retry-After`.
- [ ] `AdminKeysError`'s three variants (`UnknownScope`, `InvalidGraceHours`,
      `AlreadyRevoked`) and the bare-500 `Db` variant all answer the §1.1
      envelope, not the current ad hoc `{"error": "..."}` shapes.
- [ ] `APP_ENV=staging`/`uat` parses to a distinct `Staging` tier per §3.4 —
      key-server's `AppEnv` was two-tier (`Development`/`Production`) and
      rejected `staging` outright; this is a real gap, not a documented
      exception (the original plan's M2 table omitted this repo from the
      AppEnv fix by oversight).
- [ ] Env is `ADMIN_IDP_BASE_URL`/`ADMIN_JWT_ISSUER`/`ADMIN_JWKS_URL`, not
      `IDP_*`; CORS env is `CORS_ALLOWED_ORIGINS`, not
      `ADMIN_CORS_ALLOWED_ORIGINS`.
- [ ] `POST /v1/verify` request/response match §6.1 exactly (this one is
      already conformant — the test just needs to exist and pin it).

**news specifically**

- [ ] CORS env is `CORS_ALLOWED_ORIGINS`, not `ADMIN_CORS_ALLOWED_ORIGINS`.
- [ ] Every admin route enumerated in news's own OpenAPI spec is covered by
      the wrong-site-token sweep (plan D4.3): adding a new admin route
      without a matching sweep entry must fail the test (prove this by
      temporarily adding a fake route with no sweep entry and confirming red,
      then reverting).

**web specifically**

- [ ] CORS env is `CORS_ALLOWED_ORIGINS`, not `CORS_ORIGIN`.
- [ ] Careers admin list uses `page`/`take` + `{items, meta}` (§2.1);
      user-reports admin list intentionally keeps cursor/`limit` (§2.4
      exception) — a test should assert this divergence is *intentional*
      (i.e. exists and matches the documented shape) rather than accidental.

**utility specifically**

- [ ] Error envelope is flat `{statusCode, message, error}` (§1.1), not the
      current nested `{"error": {...}}`.
- [ ] A mutation (partner presign) with a token whose live-introspection
      verdict is `active: false` is 401; an unreachable/erroring IdP on that
      same call is 503 (plan D4.1 — this only applies once introspection is
      added in M3; today utility does not introspect at all).
- [ ] The in-code ADR comment in `src/infrastructure/admin_auth.rs` matches
      whatever the actual current behaviour is (no code/comment
      contradiction) — re-check this specifically after M3 lands.

**launcher specifically**

- [ ] `AppEnv` has three tiers and the guardrail test described above.
- [ ] `docker build` succeeds (currently broken — the Dockerfile does not
      `COPY` the path-dependency `maxion-admin-guard/rust`; see `news`'s
      Dockerfile for the working pattern to copy).

**SPA (`web-platform-back-office`, M4 — not a Rust repo, listed for
completeness)**

- [ ] `npm run typecheck && npm run lint && npm run test` all green.
- [ ] `FEATURE_KEYS` is a single union type; an invalid feature key literal
      fails `tsc`.
- [ ] `format-error.ts`: a server response with a `message` field always
      shows that message; the status-code table is a fallback only.
- [ ] `X-Request-Id` is attached on the six instances whose CORS allows it
      (idp, keyServer, launcher, news, web, utility) and **not** on
      `authServer` or `api`.
- [ ] e2e preflight: every `FEATURE_KEYS` entry is a subset of
      `GET /api/v1/sites`'s live catalog (subset, not equality — the catalog
      legitimately has keys the SPA has no route for).
