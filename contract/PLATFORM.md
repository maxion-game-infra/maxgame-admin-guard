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
| `authServer` | Player IdP + admin proxy (rewrite/migration target — **FROZEN lifted 2026-08-15**; not connected to the running production system. FROZEN was never a statement about code quality, only "we can't safely change this," and had been used to justify several exemptions below that no longer hold on that basis — see §2, §3.5, §5.4, and `maxgame-auth-server/tests/platform_conformance.rs`) | `maxgame-auth-server` |
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
| 429 | `Too Many Requests` (+ `Retry-After` header, seconds) | launcher, keyServer, utility, web (see §3.6 for idp's OAuth-exception case and authServer's out-of-scope precedent) |
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

**Ruling on vocabulary — a repo may keep its own `code` values.** idp's `code` field carries its pre-existing `DomainError` codes verbatim (`unauthorized`, `not_found`, `forbidden`, …, lowercase snake_case) rather than remapping onto this table's SCREAMING_CASE strings (`src/inbound/error.rs`, fixed in `f45b634` alongside the rest of §1.1). This is allowed: the requirement is that `code` be *stable and machine-readable within a service*, not that every service share one vocabulary — a client already scopes its branching by which service answered, so two services never need to compare `code` values against each other directly. §1.3's table remains the *recommended* starting vocabulary for a service with no existing one of its own (i.e. new services via the M6 template), not a mandate to remap an established one.

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
- **`mailer` (`maxgame-mail-server`) splits by surface.** Its **admin**
  routes (`/v1/admin/*`) conform to §1.1 in full. Its **team** routes
  (`POST /v1/emails:send`, `GET /v1/jobs/{id}`) keep the nested
  `{"error": {"code", "message"}}` envelope inherited from
  `maxgame-email-server-legacy`, because `web-cs-backend`
  (`src/adapters/notification/relay_email_service.rs`) and `mu-alpha-pipeline`
  (`src/lib/email-api.js`) parse that shape against the live relay today, and
  the port's premise is that team API keys and their callers survive cutover
  untouched. The split is deliberate and was decided with the evidence in
  front of it: the admin surface is already breaking-changed by the move to
  `maxion-admin-guard`, so it has no legacy client to protect, and leaving it
  nested would have reproduced the exact §1.4 utility bug — the back office's
  `messageFrom()` (`web-platform-back-office/src/lib/axios.ts:163`) reads only
  a top-level `message`, and the NestJS proxy rethrows the mailer's body
  verbatim, so every Mail Service admin error rendered as "Something went
  wrong!". `code` is identical across both surfaces, and so is `message` with
  exactly one exception: a 500 reads this document's fleet-wide
  `"Internal server error"` on the admin surface and Node's
  `"Internal Server Error"` on the team surface. Per the §1.3 vocabulary
  ruling, `code` keeps the service's own inherited values
  (`domain_not_found`, `sender_inactive`, `key_revoked`, …) rather than
  remapping onto the SCREAMING_CASE table. Both invariants are pinned by
  tests (`the_two_surfaces_agree_on_code_and_message` and
  `the_500_message_is_the_only_wording_that_differs_between_surfaces`).

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

- **`mailer` (`maxgame-mail-server`)** keeps the Node relay's query and
  envelope on every admin list, rather than §2.1's `page`/`take` +
  `{items, meta}`:

  ```json
  // GET /v1/admin/domains?page=2&pageSize=20
  { "page": 2, "pageSize": 20, "total": 13, "totalPages": 1, "items": [] }
  ```

  Source: ported from `maxgame-email-server-legacy/src/lib/pagination.js`. The five
  `sa-maxgame-email-*` back-office sections and the NestJS proxy DTOs already
  consume this shape, so converging would be a SPA rework rather than a wire
  fix — the same reasoning that exempts `web`'s user-reports below. It also
  keeps the Node service's `parseInt` semantics deliberately: `?page=1.7` is
  page 1, `?page=abc` falls back to the default rather than erroring, and a
  `pageSize` above 200 clamps silently instead of 400ing. Only a parsed value
  below 1 is a 400. Those are bug-for-bug parity with the live relay, pinned
  by tests.

- **`authServer` (no longer FROZEN as of 2026-08-15 — see the fleet table's
  note; the reasoning below is evaluated on its own merits, not inherited
  from that status):**

  ```json
  // GET /v1/admin/players?page=2&per_page=50
  { "items": [], "page": 2, "per_page": 50, "total": 0, "total_pages": 0 }
  ```

  Source: `maxgame-auth-server/src/interface/pagination.rs` (`PageQuery`)
  and `src/application/pagination.rs` (`Page<T>`). Field names are
  `page`/`per_page`/`total`/`total_pages`, not `page`/`take`/`itemCount`/
  `pageCount`. **The merit that survives FROZEN's lift:**
  `web-platform-back-office` already calls this endpoint directly
  (`authServerEndpointsGroup`, `CLAUDE.md`'s API map) and parses this exact
  shape today — converging it is a back-office change with a real live
  caller to update, not a side effect of a backend test. **What's an open
  question, not resolved here:** the old "do not touch — live in
  production" line conflated two claims that used to travel together and no
  longer necessarily do — "the SPA depends on this shape" (still true,
  independent of FROZEN) and "this exact service is what's currently
  serving production traffic" (uncertain now that FROZEN, the label that
  assertion leaned on, has been lifted). Whether the second claim still
  holds is for the user to determine; it doesn't change the first.
  Separately: `GET /v1/admin/api-keys` (`interface::routes::admin_keys::list`)
  answers the **identical** shape through the same `Page<T>` type — not
  named in this exception's text even though, mechanically, there's no way
  for one endpoint to conform to §2.1 without the other doing so too. See
  `maxgame-auth-server/tests/platform_conformance.rs`'s
  `admin_api_keys_list_shares_the_identical_undocumented_pagination_shape`.
  Whether to widen this exception's text to cover it, or converge it
  separately, is for the user to decide — not assumed here.

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
| `mailer` | 8096 | `/healthz` | `/readyz` | `maxgame-mail-server` (also serves the legacy `/health` at root, because the Node relay's clients and its runbooks use that path — additive, not a replacement) |
| `api` | 8080 | `/healthz` (`/health` kept as a legacy alias) | `/readyz` | `web-platform-backend` (being retired) |
| SPA | 5173 | `/` | — | `web-platform-back-office` |

Source: `back-office-workspace/.scripts/dev.sh:34-44` (`SERVICES` array,
which is the definition of "does the local stack boot" — every port above
was read straight out of it, not inferred).

**Rule**: every service serves `/healthz` (liveness — "did the process come
up") and `/readyz` (readiness — "can serve traffic") **at the root**, always,
even once `BASE_PATH` (§5) is set — a k8s probe hits the pod directly, never
through the gateway prefix.

**Body shape.** Until now this section pinned only the *paths* — never the
*bodies* — and that omission is exactly how four services drifted without
anything failing: one answered `/healthz` in `text/plain` where every other
repo answered JSON, and two more used non-standard readiness keys. The
bodies are now part of the contract:

```json
GET /healthz → 200 {"status": "ok"}
```

`/healthz` **never touches a dependency** — that's the whole point of
separating it from `/readyz` (the "Rule" above): a liveness probe that pings
a database turns that database's blip into every pod's restart.

```json
GET /readyz → 200 {"status": "ready"}
GET /readyz → 503 {"status": "unavailable", "dependency": "<name>"}
```

A service **may add fields to either response** (additive only) but **must
not rename or drop `status`**, and on the 503 branch must not drop
`dependency`. Three sanctioned additions exist today, kept deliberately
rather than converged away:

- **`utility`** answers `{"status": "ready", "buckets": N}` on `/readyz`
  and has **no 503 branch at all** — it has no runtime dependency to fail
  against (an empty bucket registry refuses to boot, so a started replica
  always has *a* registry), so `buckets` is the entire readiness signal: the
  count is how a rolling deploy shows a replica still running a stale
  `R2_BUCKETS`. Source: `maxgame-utility-server/src/inbound/health.rs`.
- **`authServer`** keeps `postgres_write`/`postgres_read`/`redis` on
  `/readyz` alongside the standard `status`/`dependency` keys, deliberately,
  for backward compatibility with a production deployment that already
  parses those three fields. Source:
  `maxgame-auth-server/src/interface/routes/mod.rs`.
- **`mailer`** also serves the legacy `/health` at root — the Node relay's
  own document shape (`{"status", "service", "time", "config": {...}}`),
  unrelated in shape to `/healthz`/`/readyz` — additive, kept because
  existing runbooks and uptime checks call that path. Source:
  `maxgame-mail-server/src/inbound/health.rs`.

`api` (NestJS) carries a fourth, platform-wide alias rather than a
service-specific addition: its pre-existing `/health` → `{"ok": true}` stays
as a legacy alias alongside the new standard `/healthz`/`/readyz` pair, for
the same reason as `mailer`'s — an existing caller reads it, and converging
the *body* shape doesn't require migrating that caller off the old path.
Source: `web-platform-backend/src/app.controller.ts`.

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
| `mailer` | `CORS_ALLOWED_ORIGINS` | ✅ standard — shipped as `CORS_ORIGIN` (copied from `web`'s spelling), caught by its own `tests/platform_conformance.rs` before deployment and renamed in `4d6842f`. Its conformance test asserts the standard name boots **and** that the pre-rename name no longer does, so the service cannot quietly accept both |
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

**Every IdP URL the service resolves — `jwks_url` and `introspect_url` —
must be https outside development, validated at the resolved value, not
the raw input env var.** `ADMIN_JWKS_URL` being https was already a rule
(§3.4's guardrail list); `introspect_url` (built from `ADMIN_IDP_BASE_URL`
+ `ADMIN_INTROSPECT_PATH`) was not separately checked, which left a gap: an
https `ADMIN_JWKS_URL` override did not stop a plaintext `ADMIN_IDP_BASE_URL`
from being a token-forgery path on the introspection call. The fix checks
the two *resolved* struct fields (`admin_idp.jwks_url`,
`admin_idp.introspect_url`) directly rather than checking
`ADMIN_IDP_BASE_URL` as a proxy for them — checking the resolved value is
what stays correct even if a future change to how a URL is built forgets to
preserve the scheme, instead of relying on every call site to remember the
invariant.

**That last point is not hypothetical — it is already true of six of the
seven Rust repos.** `keyServer`, `launcher`, `news`, `web`, `utility`, and
`mailer` each have their own `absolute_url(base, path)` helper: when
`ADMIN_INTROSPECT_PATH` itself starts with `http://` or `https://`, that
value is used *as-is* instead of being joined onto `ADMIN_IDP_BASE_URL` —
`maxgame-utility-server/src/config.rs:561` pins
`ADMIN_INTROSPECT_PATH=https://other-host.example/introspect` as an
accepted, deliberate config shape, not an edge case. So in those six
repos, `introspect_url`'s scheme does **not** only come from
`ADMIN_IDP_BASE_URL`; it can come from the override instead. The
resolved-value check is what makes that safe: it does not matter whether
`introspect_url` got its scheme from the base URL or from an absolute
override, checking the final resolved string catches a plaintext result
either way. **This is exactly why the check must stay on the resolved URL,
and must never be "simplified" back to checking `ADMIN_IDP_BASE_URL`
alone** — that would silently stop covering the passthrough path. A
consequence worth being explicit about: pointing introspection at a
*different* https host is possible in these six repos via an env var.
That requires env write access on the deployment (not, by itself, a
vulnerability an outside caller can trigger), but this document should not
claim it is impossible.

**The template is the deliberate exception.** It has no
`ADMIN_INTROSPECT_PATH` absolute-URL passthrough at all — `join_url` always
joins the path onto the base, never substitutes for it (see its doc
comment) — so a new service starts stricter than the six existing repos
rather than inheriting their more permissive shape. If a future change to
the template *does* add a passthrough, the resolved-value check keeps it
safe the same way it already does for the six repos that have one; the
check does not depend on the passthrough's absence to be correct.

A related bug in the same code path: `ADMIN_INTROSPECT_PATH` used to
concatenate straight onto the base URL (`{base}{path}`), so a value missing
its leading slash (`admin/introspect` instead of `/admin/introspect`)
silently produced a broken URL with no path separator at all — not an
error, just a URL that resolves nowhere real. Fixed by joining the two
pieces through a helper that tolerates the missing slash.

Reference implementation: `maxgame-admin-guard/template/src/config.rs`'s
`validate()` (the resolved-value checks) and `join_url` (the leading-slash
tolerance, and its doc comment for the no-passthrough guarantee), plus the
tests `an_explicit_https_jwks_url_does_not_excuse_a_plaintext_base_url`,
`admin_introspect_path_is_never_an_absolute_url_override`,
`join_url_tolerates_a_path_missing_its_leading_slash`, and
`admin_introspect_path_without_a_leading_slash_still_resolves_correctly`.
Every repo with this env set should carry the same checks.

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

`idp` (`maxgame-admin-auth-server/src/config.rs`) has since adopted the
three-tier model above (fixed `0f04ddd`) — `APP_ENV` is now required, no
default, and `staging`/`uat` parse to `AppEnv::Staging` exactly as they do
in `utility`. `launcher`, `news`, `web`, and `utility` already matched the
three-tier model; `key-server` did not and was fixed separately (see its
own conformance test).

**Ops note — idp no longer accepts `APP_ENV=test`.** Before the fix above,
idp's two-tier parser mapped an unset or unrecognized `APP_ENV` (including
literally `test`) to `Development`, so a deployment that set `APP_ENV=test`
got the relaxed dev guardrails. Post-fix, idp follows the same parser every
other repo uses: `test` is rejected outright (refuses to boot) rather than
silently mapped to anything. If any deployment manifest, CI job, or
`.env.example` still sets `APP_ENV=test` for idp, it will now fail to boot
— change it to `development`/`dev`/`local` (for a real dev box) or
`staging`/`uat` (for a QA deployment that should get deployed-tier
guardrails), per the reasoning above: `test` is deliberately not a
recognized spelling anywhere on the platform, because it reads as a
deployed QA environment at least as often as a laptop.

### 3.5 Swagger / OpenAPI mount

Every service that ships Swagger mounts the UI at **`/docs`** and serves the
spec at **`/docs/openapi.json`** — hardcoded, no env override. `SWAGGER_PATH`
existed as a per-repo env var before this and is now retired platform-wide;
there is nothing left to configure.

Because the Swagger router is merged into each service's app **before** the
`BASE_PATH` nest (§5.2), the path a client calls once deployed is
`<BASE_PATH>/docs`, not a bare `/docs` — the hardcoded value is what the
service mounts at its own root, the same way `/healthz`/`/readyz` are
rooted (§3.1) but, unlike them, Swagger **does** move under `BASE_PATH`
when one is set.

| Repo | Mounted at | Spec at | Gated by | Default |
|---|---|---|---|---|
| `idp` | `/docs` | `/docs/openapi.json` | `SWAGGER_ENABLED` | `false` |
| `launcher` | `/docs` | `/docs/openapi.json` | `SWAGGER_ENABLED` | `false` |
| `news` | `/docs` | `/docs/openapi.json` | `SWAGGER_ENABLED` | `false` |
| `utility` | `/docs` | `/docs/openapi.json` | `SWAGGER_ENABLED` | `false`, and refuses to boot with it `true` outside `Development` (§3.4) |
| `keyServer` | `/docs` | `/docs/openapi.json` | `SWAGGER_ENABLED` | `false`, same production refusal as `utility` |
| `authServer` | `/docs` | `/docs/openapi.json` | `OPENAPI_ENABLED` | **`true`** — deviation, see below |
| `web` | — (no Swagger) | — | — | — |
| `mailer` | — (no Swagger) | — | — | — |
| `api` (NestJS) | `/docs` (coincidentally the same path, via `@nestjs/swagger`'s own convention — not converged to this rule) | `/docs-json` (Nest's default, not `/docs/openapi.json`) | `APP_ENV !== 'production'` (its own gate, not `SWAGGER_ENABLED`) | enabled whenever `APP_ENV` isn't exactly `production` |

Source: `SwaggerUi::new("/docs").url("/docs/openapi.json", ...)` in each
Rust repo's router (`idp` `src/inbound/router.rs:90`, `launcher`
`src/inbound/router.rs:30`, `news` `src/inbound/router.rs:32`, `utility`
`src/inbound/router.rs:29`, `keyServer` `src/app.rs:66`, `authServer`
`src/interface/routes/mod.rs:146`); `web-platform-backend/src/main.ts:39-45`
and `src/libs/swagger/swagger.ts` for `api`.

**Known gap — `authServer` has not converged.** It still gates its mount
with `OPENAPI_ENABLED` (`src/infrastructure/config/mod.rs`, default `true`),
not the fleet's `SWAGGER_ENABLED` (default `false`), and — unlike
`utility`/`keyServer` — has **no guardrail refusing to boot with it enabled
outside `Development`**. This is a real, open non-conformance. The reason it
was left alone — "`authServer` is a live production deployment, out of
scope for the round of work that converged the other six repos" — no longer
holds: FROZEN was lifted 2026-08-15, and this repo is a rewrite/migration
target, not a service whose deployment status this document can point to as
a reason not to touch it. It remains unconverged only because nobody has
done the work yet, not because of any surviving exemption — tracked here so
the next pass on this repo doesn't have to rediscover it, and is now free to
just fix it.

**Services with no Swagger at all**, so a reader doesn't mistake the blank
row above for a gap: `web` (`maxgame-web-backend`), `mailer`
(`maxgame-mail-server`), and the `template` (M6) scaffold. `api`'s Swagger
is real but is NestJS's own framework convention, predates this rule, and
is not being converged — the service is being retired (§1.5, §2.4).

### 3.6 Rate limiting

Not every endpoint needs a limiter — the fleet's rule of thumb is **look at
the endpoint, not the repo**: if hammering it gets an attacker an account,
free inventory, or a guessable secret, it needs one; if it only gets them
public, cacheable data, it doesn't. `news` and `web`'s careers admin/public
GETs have none, deliberately, on that basis. `mailer` has none for a
different reason — a documented deviation, not an oversight:

> `RATE_LIMIT_PER_MIN` is gone (deviation D2). Request throttling moved to
> ingress-nginx.

Source: `maxgame-mail-server/src/config.rs:14-15`.

Where a limiter does exist today:

| Service | Endpoint(s) | Key | Env (default) | State |
|---|---|---|---|---|
| `idp` | `POST /api/v1/oauth/google/start` | IP | `RATE_LIMIT_START_PER_MINUTE=20` | in-memory, per replica |
| `idp` | `POST /api/v1/oauth/token` | IP | `RATE_LIMIT_TOKEN_PER_HOUR=60` | in-memory, per replica |
| `keyServer` | `POST /v1/verify` | presented `mxs_` key (SHA-256 via `domain::hash_key`; IP fallback for bodies with no parseable `mxs_` candidate; 10k-bucket cap, fail-closed on *new* buckets at capacity) | `RATE_LIMIT_VERIFY_PER_MIN=120` (per key since `f3e2f7c` — was per IP, which under k8s SNAT would have collapsed every S2S caller onto a handful of node-IP buckets and, because callers treat non-200 as 503 fail-closed, turned the limit into a fleet-wide mutation outage) | in-memory, per replica |
| `utility` | `POST /v1/partner/presign-upload` | IP | `PARTNER_PRESIGN_PER_HOUR=15` (documented exception, §1.5-style — see rule 2) | in-memory, per replica |
| `web` | `POST /admin/user-reports` (public submission) | IP (salted hash) | `USER_REPORTS_PER_HOUR=10` (documented exception, same as above) | **Postgres** (`rate_limit_counters`) — survives restarts, correct across replicas |
| `launcher` | `POST /maxgame-launcher-coupons/redeem` | `player_id` | `RATE_LIMIT_COUPON_REDEEM_PER_MIN=10` | in-memory, per replica |
| `authServer` *(out of scope — informative precedent only, see line 12)* | `POST /v1/auth/login`, `POST /v1/auth/{provider}/login-url`, `POST /v1/auth/refresh`, `POST /v1/launch/redemptions` | IP | `RATE_LIMIT_LOGIN_PER_MIN=10`, `RATE_LIMIT_LOGIN_URL_PER_MIN=30`, `RATE_LIMIT_REFRESH_PER_MIN=60`, `RATE_LIMIT_REDEMPTIONS_PER_MIN=10`, kill-switch `RATE_LIMIT_ENABLED=true` | **Redis** (fixed-window `INCR`+`EXPIRE`) — survives restarts, correct across replicas |

Sources: `maxgame-admin-auth-server/src/config.rs:265-267`,
`maxgame-key-server/src/config.rs:174`,
`maxgame-utility-server/src/config.rs:242`,
`maxgame-web-backend/src/config.rs:213-217` +
`src/modules/user_reports/rate_limit.rs` +
`migrations/20260814000003_rate_limit_counters.sql`,
`maxgame-launcher-backend/src/modules/coupons/rate_limit.rs` +
`.env.example:51`, `maxgame-auth-server/src/interface/middleware/rate_limit.rs`
+ `.env.example:38-42` + `src/infrastructure/adapters/rate_limit.rs`
(`RedisRateLimiter`). `authServer`'s `POST /v1/auth/introspect` is
deliberately **not** limited — game servers call it at a high, steady rate
and it carries no secret to brute-force, so limiting it risks breaking games
against a threat that doesn't apply to it
(`maxgame-auth-server/src/interface/middleware/rate_limit.rs`'s module doc).

Normative rules, extracted from the above rather than invented:

1. **Every 429 carries `Retry-After` in seconds** — the remaining window,
   rounded up, minimum 1. This was already §7's checklist line for "a 429
   (where applicable)"; it is now a rule, not just a checklist item, because
   `utility` shipped a 429 with no header at all before being caught (fixed
   `167000e`, test `a_caller_over_the_hourly_limit_is_429` in
   `tests/partner_presign.rs`). §1.2's 429 row records which services this
   applies to today: `launcher`, `keyServer`, `utility`, `web` answer it
   through the shared flat envelope (§1.1); `idp`'s two OAuth limits answer
   it through the RFC 6749 exception envelope instead (§1.5) — same header,
   different body shape, because the OAuth routes were already exempt from
   §1.1 before rate limiting existed. `authServer` is out of contract scope
   (line 12) and is not counted in either.
2. **Env naming: `RATE_LIMIT_<WHAT>_PER_MIN` / `_PER_HOUR`.** Two pre-existing
   names are documented exceptions rather than forced renames, in the same
   spirit as §1.5/§2.4: `PARTNER_PRESIGN_PER_HOUR` (utility) and
   `USER_REPORTS_PER_HOUR` (web) predate this convention and are load-bearing
   in deployed `.env` files. `idp`'s two names are a **third**, previously
   unrecorded exception in the same family: `RATE_LIMIT_START_PER_MINUTE` and
   `RATE_LIMIT_TOKEN_PER_HOUR` spell the minute unit out in full
   (`_PER_MINUTE`, not `_PER_MIN`) where every other service abbreviates it.
   Noted here rather than renamed, for the same reason as the other two.
3. **The trust-proxy flag is named `TRUST_PROXY_HEADERS`, default `false`.**
   It gates whether a proxy header (`X-Forwarded-For`, `X-Real-IP`, or
   `CF-Connecting-IP`, depending on the service — rule 4) is read at all for
   both rate-limit keying and audit-log IP capture; with it off, only the TCP
   peer address is trusted. `idp` shipped this as `TRUST_FORWARDED_HEADERS`
   until M2/M3 convergence; the M2 rename (`1261e72`) brought it in line with
   `keyServer` and `utility`, which already used `TRUST_PROXY_HEADERS`.
   `grep -rn TRUST_FORWARDED_HEADERS` across the fleet today returns nothing
   outside git history.
4. **Client-IP resolution order: `CF-Connecting-IP` → left-most
   `X-Forwarded-For` hop → TCP peer, gated entirely behind
   `TRUST_PROXY_HEADERS`.**
   `maxgame-web-backend/src/modules/user_reports/rate_limit.rs:42-53` (test
   `cloudflares_header_outranks_the_forwarded_chain`) is the reference
   implementation — Cloudflare overwrites its own header at the edge so a
   client cannot forge it, which is why it outranks the client-suppliable
   XFF. This order is now converged fleet-wide: `idp` and `authServer`
   originally resolved XFF → `X-Real-IP` → peer with no `CF-Connecting-IP`
   step (a gap the first revision of this section documented), and were
   brought in line the same day (`maxgame-admin-auth-server` `4f9890e`,
   `maxgame-auth-server` `e0807f3` — both keep `X-Real-IP` as a third
   fallback after XFF, a harmless historical extra the reference
   implementation simply doesn't have). The load-bearing property remains:
   **a service must never trust a proxy header unconditionally** — every
   implementation gates the headers behind `TRUST_PROXY_HEADERS` and falls
   back to the TCP peer with it off, proven per repo by a
   headers-are-ignored-when-untrusted test.
5. **State honesty.** In-memory windows (`idp`, `keyServer`, `utility`,
   `launcher`) are per replica — the effective limit is the configured value
   × replica count — and are lost on restart. This is accepted by design for
   anti-abuse limits, not an oversight (see each service's own module doc,
   e.g. `maxgame-launcher-backend/src/modules/coupons/rate_limit.rs`'s
   "accepted on purpose, because this is anti-abuse rather than an account
   quota"). `web` (Postgres) and `authServer` (Redis) are the fleet's two
   examples of a limiter that survives restarts and is correct across
   replicas — reach for one of those two patterns, not a new one, when a
   future limiter needs to scale out. Separately: **prefer keying by
   identity over IP wherever an authenticated identity already exists.**
   `launcher`'s coupon-redeem limiter keys by `player_id`, not IP, precisely
   because the route sits behind player auth and an IP-keyed limiter would
   both be weaker (NAT/shared-IP false sharing) and unnecessary (the real
   identity is already known). `keyServer`'s `/v1/verify` limiter is the
   second example: it keys by the presented `mxs_` key (hashed), because the
   unit of S2S identity is the key, not the socket — and under k8s SNAT the
   socket address actively lies (every pod egresses as one of a handful of
   node IPs).
6. **A rate-limit value that fails to parse refuses to boot** — never a
   silent fallback to the default. This matches the fleet's general config
   convention (§3.4) rather than inventing a rate-limit-specific rule; e.g.
   `maxgame-key-server/src/config.rs:708-713` and
   `maxgame-launcher-backend/src/config.rs:1195-1209` both test a
   non-numeric value for their respective rate-limit env vars and assert the
   boot fails naming that variable.

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
| `/mailer` | `mailer` | `maxgame-mail-server` (the Rust port of `maxgame-email-server-legacy`; port 8096, `BASE_PATH=/mailer`, no ingress rewrite. Replaces the old "stays on Cloud Run" row — the Node service on `mailer.*` is retired at cutover. See its exceptions in §1.5 and §2.4) |

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

**`authServer`'s `BASE_PATH` support is still a follow-up, not part of this
plan** — unaffected by FROZEN being lifted (2026-08-15). Adding `/auth` at
the ingress remains additive (the existing `account.*` host is untouched)
regardless of that status; implementing `BASE_PATH` inside
`maxgame-auth-server` itself was never blocked by FROZEN in the first
place, only not yet scheduled.

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

### 6.2b The S2S HTTP client must refuse redirects

Every `reqwest::Client` used for an S2S call — key-server verify, admin
introspection, JWKS fetch, any provider call carrying a secret — must be built
with `.redirect(reqwest::redirect::Policy::none())`.

This is not hygiene, it is a credential-exfiltration boundary. reqwest's
default is `Policy::limited(10)`, and on a 307/308 it re-sends the **method,
body, and custom headers** to the new target. It strips only `Authorization`
and `Cookie`, and only cross-host. That leaves exposed:

- the admin access token, which `AdminIntrospectClient` sends in the JSON
  **body**, and the introspect secret, which it sends as the custom header
  `x-api-key` (`rust/src/introspect_client.rs`);
- any service key sent in a request body, e.g. the plaintext `mxs_` key in
  `/v1/verify`'s body.

It also routes around §3.3's https guardrails, which validate the *configured*
URL and cannot see where a redirect leads — the same class of gap as the
`absolute_url` passthrough. Anyone able to make the configured host answer a
redirect (a compromised pod, DNS spoofing, a misconfigured ingress or CDN)
collects live admin sessions and service keys without holding any credential.

**The JWKS fetch is not a lower-risk case of this, even though it carries no
credential of its own.** It is tempting to reason that a client with no
secret in the request needs less protection than one sending a token or an
`x-api-key` — that reasoning is backwards. A redirected JWKS fetch lets an
attacker serve their **own** key set, which every verifier that trusts it
will then accept as the set of valid signing keys — i.e. mint admin tokens
the service treats as genuine. That is a total auth bypass requiring **no
credential at all**, strictly more severe than leaking one admin's token or
one service key, both of which are scoped and revocable. The protection
already covers this call transitively within each service — a repo
typically builds **one** `reqwest::Client` with the policy set once (e.g.
`maxgame-utility-server/src/inbound/state.rs`'s `AppState::new`) and clones
it into the JWKS client, the introspect client, and the key-server client
alike, so no call site opts in separately. That is a per-repo implementation
convenience, not a fleet-wide shared HTTP client (D0 — there is no such
thing on this platform); the point is not to rely on it but to make sure
the JWKS case is never recorded as "lower risk" and quietly dropped from
§6.2b's scope — the rule already names the JWKS fetch explicitly, and this
paragraph exists so that stays true.

No S2S call in this platform legitimately redirects. Found fleet-wide by
security review on 2026-08-14 (no repo set a policy) and fixed across all
seven Rust services; each carries a test that points its client at a mock
answering 307/308 and asserts the redirect target receives nothing.

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
| ~~(env-configured admin key)~~ | ~~`x-admin-key`~~ | ~~`maxgame-email-server-legacy`~~ | **RETIRED** — `maxgame-mail-server` replaced it with `maxion-admin-guard` (super_admin only) on the admin surface. Its team surface dual-accepts the legacy per-tenant `mxk_live_…` bearer key (a tenant credential, not an S2S service key, so it does not belong in this table) **and** a `mxs_` key-server key — see §6.5, not this table: mxs is the standard for this surface, not a legacy secret being tracked for a future migration |

None of these are touched by this plan (plan §7 follow-up item 4). They are
recorded so a future migration doesn't have to rediscover them.

### 6.5 Services that accept `mxs_` (ADR D1)

Unlike §6.4's registry — legacy secrets catalogued for a *future* migration
onto key-server — this table is the live roster: a service already dual-accepts
`mxs_` keys, verified through `POST /v1/verify` (§6.1), as one of its accepted
credential forms today, alongside whatever legacy form it also keeps.

| Service | Route(s) | Scope required | Identity mapping |
|---|---|---|---|
| `mailer` (`maxgame-mail-server`) | `POST /v1/emails:send`, `GET /v1/jobs/{id}` (both behind `require_team`) | `email:send` | `key_id` → stored as `mxs:{key_id}` in `jobs.key_id` and the audit trail (those columns are plain text with no FK, so the prefix is the discriminator that keeps the two id namespaces — this service's own `api_keys.id`, key-server's `key_id` — from colliding) · `team_name` → verify's `consumer` · `allowed_senders` → verify's `metadata.allowed_senders` (array of sender-id strings), **explicit only**: no `metadata`, no `allowed_senders` field, or an empty array all mean **no senders**, never "every sender". `/v1/verify` has no sender concept of its own — `metadata` is free-form — so this mapping is the only place enforcing the "empty means none, not all" rule an `email:send` key would otherwise bypass entirely, turning one key into the ability to impersonate every sender the service knows about. Source: `maxgame-mail-server/src/adapters/key_server.rs` (`VerifiedServiceKey::allowed_senders`) and `src/infrastructure/team_auth.rs` (`verify_via_key_server`) |

Error taxonomy, same as §6.2 with one addition worth naming explicitly: a
verdict of `active: false` (any of the four `reason` values in §6.1) is a
401; **anything else that is not a clean 200-with-parseable-body — including
a 429 from `/v1/verify`'s own rate limiter (120/min per IP) — is a 503**,
never an implicit pass and never a fallback to the legacy credential form.
`mailer`'s reference tests: `an mxs key with no metadata.allowed_senders is
refused`, `an mxs key listing a different sender is refused`, `a rate-limited
verify is service-unavailable, not a denial`.

---

## 7. Minimum conformance assertions per repo

Each repo below writes and owns its own test for these — no shared test
harness, per D0. This is the checklist a repo's conformance test (M5) must
cover; it should be concrete enough to write from directly.

**Every Rust repo (8): idp, keyServer, launcher, news, web, utility, mailer,
authServer** — `authServer` was excluded from this count until 2026-08-15
(it carried the FROZEN label discussed in the fleet table and §2/§3.5/§5.4);
several items below apply to it only partially, per its own documented and
open exceptions — see **authServer specifically** at the end of this
section rather than assuming every bullet below applies unmodified.

- [ ] Every error response is `{statusCode, message, error}` (§1.1); a 500's
      `message` is always exactly `"Internal server error"`; a 429 (where
      applicable) carries `Retry-After` in seconds.
- [ ] `GET /healthz` and `GET /readyz` both exist at the root and return 2xx
      when the service is healthy, independent of `BASE_PATH`.
- [ ] `GET /healthz` answers exactly `{"status": "ok"}` and never queries a
      dependency; `GET /readyz` answers `{"status": "ready"}` on 200 and
      `{"status": "unavailable", "dependency": "<name>"}` on 503, with any
      repo-specific extra fields present but `status`/`dependency` neither
      renamed nor dropped (§3.1's body shape) — e.g. `utility`'s `buckets`
      field and no-503-branch, `authServer`'s three extra dependency flags.
- [ ] Booting with `BASE_PATH=/x` set: every existing route answers under
      `/x/...`, and `/healthz`+`/readyz` still answer at the root (unset
      `BASE_PATH` must be behaviourally identical to today).
- [ ] If this repo ships Swagger: it is mounted at `/docs` with the spec at
      `/docs/openapi.json`, hardcoded — no `SWAGGER_PATH` or equivalent env
      var exists to override either path (§3.5). Booting with `BASE_PATH=/x`
      set, the mount answers at `/x/docs` (Swagger nests under `BASE_PATH`
      like every other route — only the two health probes are exempt).
- [ ] CORS: `allow_headers` includes `authorization, content-type, accept,
      x-request-id` (plus any repo-specific extra, e.g. utility's
      `x-api-key`); a literal `*` in `CORS_ALLOWED_ORIGINS` refuses to boot;
      an empty allowlist refuses to boot outside `Development`.
- [ ] `APP_ENV` is required (no silent default to `Development`); `staging`
      and `uat` both parse to `AppEnv::Staging`, not `Development`; `test`
      is rejected outright, never silently mapped to any tier; every
      deployment guardrail (Swagger off, every resolved IdP URL over https,
      non-empty CORS allowlist, etc.) fires identically for `staging` and
      `production` — i.e. is keyed off `!is_dev()`, verified by a test that
      runs the same guardrail assertions against both tiers.
- [ ] Outside `Development`, every IdP URL the service resolves — `jwks_url`
      and `introspect_url`, not the raw `ADMIN_IDP_BASE_URL`/
      `ADMIN_JWKS_URL`/`ADMIN_INTROSPECT_PATH` input env vars — must be
      `https://`, checked **independently** at the resolved value: an https
      `ADMIN_JWKS_URL` override must not excuse a plaintext
      `ADMIN_IDP_BASE_URL`. **Check the resolved string, never
      `ADMIN_IDP_BASE_URL` alone as a proxy for it** — in the template,
      `introspect_url` always inherits the base URL's scheme (no
      `ADMIN_INTROSPECT_PATH` absolute-URL passthrough exists there — see
      `join_url`'s doc comment), but keyServer/launcher/news/web/utility/mailer
      each have their own `absolute_url(base, path)` passthrough, where an
      `ADMIN_INTROSPECT_PATH` starting with `http://`/`https://` is used
      as-is instead of being joined onto the base (`maxgame-utility-server
      /src/config.rs:561` documents this as an accepted config shape, not an
      edge case). A base_url-only check would silently stop covering that
      passthrough path in those six repos; a resolved-value check catches
      a plaintext result regardless of which input produced it. See §3.3,
      `template/src/config.rs`'s `validate()` for the reference resolved-value
      checks, and its
      `an_explicit_https_jwks_url_does_not_excuse_a_plaintext_base_url` and
      `admin_introspect_path_is_never_an_absolute_url_override` tests (the
      latter proving the template's own, stricter no-passthrough design,
      not a platform-wide guarantee).
- [ ] Every `reqwest::Client` carrying a credential is built with
      `.redirect(reqwest::redirect::Policy::none())` (§6.2b), with a test
      pointing it at a mock that answers 307/308 and asserting the redirect
      target receives nothing and the call fails closed. The https checks
      above validate the *configured* URL and cannot see past a redirect, so
      without this the credential in the request **body** (admin token,
      `mxs_` key) and the `x-api-key` **header** are forwarded to whatever
      the configured host names next — reqwest strips only `Authorization`
      and `Cookie`, and only cross-host.
- [ ] `ADMIN_INTROSPECT_PATH` tolerates a value missing its leading slash
      (`admin/introspect` as well as the correct `/admin/introspect`)
      rather than silently concatenating into a broken URL with no path
      separator at all. See `template/src/config.rs`'s `join_url` and its
      `join_url_tolerates_a_path_missing_its_leading_slash` /
      `admin_introspect_path_without_a_leading_slash_still_resolves_correctly`
      tests.
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
- [ ] `POST /api/v1/oauth/google/start` and `POST /api/v1/oauth/token` each
      429 once their respective window (`RATE_LIMIT_START_PER_MINUTE`,
      `RATE_LIMIT_TOKEN_PER_HOUR`) is exhausted, carrying `Retry-After`, in
      the RFC 6749 exception envelope (§1.5), not §1.1's (§3.6 rule 1).
- [ ] `grep -rn TRUST_FORWARDED_HEADERS` returns nothing outside git history
      — renamed to `TRUST_PROXY_HEADERS` (§3.6 rule 3, `1261e72`).

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
- [ ] The public user-reports submission 429s past `USER_REPORTS_PER_HOUR`
      (default 10), keyed by a salted IP hash (`RATE_LIMIT_IP_SALT`, required
      at boot) resolved `CF-Connecting-IP` → left-most XFF → peer, with
      `Retry-After` set and the counter surviving a restart — it lives in
      Postgres (`rate_limit_counters`), not memory (§3.6 rules 1, 4, 5).

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
- [ ] A 429 past `PARTNER_PRESIGN_PER_HOUR` carries `Retry-After` (§3.6 rule
      1 — this was missing until `167000e`; test
      `a_caller_over_the_hourly_limit_is_429` in `tests/partner_presign.rs`
      pins it so it cannot regress silently again).

**launcher specifically**

- [ ] `AppEnv` has three tiers and the guardrail test described above.
- [ ] `docker build` succeeds (currently broken — the Dockerfile does not
      `COPY` the path-dependency `maxgame-admin-guard/rust`; see `news`'s
      Dockerfile for the working pattern to copy).
- [ ] `POST /maxgame-launcher-coupons/redeem` 429s past
      `RATE_LIMIT_COUPON_REDEEM_PER_MIN` (default 10), keyed by `player_id`
      rather than IP (§3.6 rule 5), and the check runs before the code lookup
      so a valid and an invalid code are throttled identically — limiting
      only failed attempts would leak which codes are valid through 429
      timing (`src/modules/coupons/rate_limit.rs`, `c8bb9c8`).

**mailer specifically**

- [ ] `APP_ENV` is required (no silent default); `staging`/`uat` parse to a
      distinct `Staging` tier and `test` is rejected outright, same as the
      rest of the fleet (`src/config.rs`, fixed `f2eeeaf` — this repo shipped
      after the rest of the fleet already had the three-tier model, so it
      never carried the two-tier gap idp/key-server once did).
- [ ] Outside `Development`, the resolved `introspect_url` is checked for
      `https://` independently of `ADMIN_JWKS_URL` (`f2eeeaf`) — the
      `absolute_url(base, path)` passthrough is *kept*, per the §3.3/§7
      "option A" decision now shared by six of the seven Rust repos, not
      removed; a negative test (`http://evil` refused in staging/production,
      accepted in development) mirrors utility's and key-server's.
- [ ] `BASE_PATH` nests the whole router and both probes stay at the root
      regardless of whether it's set (`tests/fallback.rs`'s
      `an_unset_base_path_serves_everything_at_the_root` and
      `base_path_nests_the_api_but_never_the_probes`).
- [ ] CORS: `CORS_ALLOWED_ORIGINS` (already standard — see §3.2's note on the
      `4d6842f` rename, pinned by its own conformance test so the pre-rename
      name can't quietly come back), `allow_headers` includes `x-request-id`
      (`src/inbound/router.rs`).
- [ ] Team surface (`POST /v1/emails:send`, `GET /v1/jobs/{id}`) dual-accepts
      `mxs_` (verified live via key-server, scope `email:send`) alongside the
      unchanged legacy `mxk_` per-tenant lookup (`28fa7a4`, plan ADR D1).
      `allowed_senders` comes from verify's `metadata.allowed_senders` and
      nothing else — no metadata, or an empty array, grants zero senders,
      never every sender; every non-clean-200 from key-server (including its
      own `/v1/verify` 429 rate limiter) is a 503, never an implicit pass;
      only a clean `active:false` is a 401. `key_id` is stored as
      `mxs:{key_id}` everywhere it lands in this service's own text columns
      (`jobs.key_id`, the audit trail), to keep the two id namespaces from
      colliding.
- [ ] The two sanctioned exceptions stay pinned by this repo's own test, not
      merely assumed: the team surface keeps the nested `{"error": {"code",
      "message"}}` envelope (§1.5) while the admin surface is flat per §1.1;
      admin list pagination keeps Node's `page`/`pageSize` →
      `{page,pageSize,total,totalPages,items}` (§2.4), not `page`/`take` +
      `{items,meta}`. `tests/platform_conformance.rs`'s own doc comment spells
      out why a "well-meaning fleet-wide sweep" is exactly what these guard
      against — read that file before "fixing" either shape.

**authServer specifically** — first assessed against this section
2026-08-15, the day FROZEN was lifted; `tests/platform_conformance.rs`
(new, same day) is the source for every item below.

- [ ] Health at root: `/healthz` answers exactly `{"status":"ok"}`; `/readyz`
      answers exactly `{status, postgres_write, postgres_read, redis}` on
      200/503, per §3.1's sanctioned three-extra-field row for this repo
      (`postgres_write`/`postgres_read` gate the status code, `redis` is
      reported only, not gating, as of the 2026-08-15 P0-4 readiness fix) —
      `healthz_answers_exactly_the_contract_body`,
      `readyz_reports_the_documented_extra_fields_without_dropping_status`.
- [ ] The player API's `{error, message}` envelope is the sanctioned §1.1
      exception (lines 10-13) —
      `player_facing_errors_keep_the_repos_own_shape_which_platform_md_scopes_out`.
      **`/v1/admin/*` answers the identical shape** through the same
      `impl IntoResponse for AuthError`, which is **not** covered by that
      exception — the admin surface is exactly what its "except where they
      double as an admin surface" carve-out excludes, and it's the surface
      `web-platform-back-office`'s `authServerEndpointsGroup` calls directly.
      Pinned, not fixed, by
      `admin_route_errors_use_the_same_two_field_shape_as_the_player_api`.
      Open decision for the user — see §1 crosswalk above.
- [ ] `GET /v1/admin/players` keeps §2's documented `page`/`per_page` →
      `{items, page, per_page, total, total_pages}` shape —
      `admin_players_list_keeps_the_documented_non_standard_pagination_shape`.
      `GET /v1/admin/api-keys` shares the **identical** shape through the
      same `Page<T>` type, not named in §2's exception text —
      `admin_api_keys_list_shares_the_identical_undocumented_pagination_shape`.
      Open decision for the user — see §2 above.
- [ ] Every credential-bearing `reqwest::Client` refuses redirects (§6.2b) —
      fixed fleet-wide 2026-08-14/15, including here: both of this repo's
      clients (`services::admin_http_client`;
      `infrastructure::adapters::http::build_http_client`, used by the
      Google/Maxion providers) set `.redirect(Policy::none())`, proven by
      `tests/admin_dual_auth_flow.rs`'s
      `a_jwks_redirect_is_not_followed_and_the_request_fails_closed`.
- [ ] Every 429 on the four rate-limited public routes
      (login/login-url/refresh/redemptions) carries `Retry-After` through the
      real router and middleware, not just the error type in isolation (§3.6
      rule 1) — `tests/login_rate_limit_flow.rs`.
- [ ] **Real, open gaps found while writing this repo's conformance test —
      none fixed, none previously assessed here because the repo was outside
      §1's scope while FROZEN:**
  - [ ] No three-tier `AppEnv` (§3.4). `app_env` is a raw `String`
        (`infrastructure::config::Config`); `Config::validate()`'s only
        deployment guardrail branch is `is_production()`, so `staging`/`uat`
        currently get the same relaxed rules as `development` — the exact
        pattern §3.4 warns against. Pinned by
        `staging_gets_the_same_relaxed_guardrails_as_development_today`.
  - [ ] No resolved-URL https guardrail (§3.3). A plaintext
        `ADMIN_IDP_BASE_URL` passes `Config::validate()` even with
        `app_env=production` — unlike six of the seven other Rust repos.
        Pinned by
        `a_plaintext_admin_idp_url_passes_validate_in_production_because_no_https_guardrail_exists`.
  - [ ] No CORS wildcard/empty refusal (§3.2). `CORS_ALLOWED_ORIGINS=*`
        boots with no refusal anywhere in the chain. Pinned by
        `a_wildcard_cors_origin_passes_validate_with_no_refusal_anywhere`.
- [ ] Swagger/`OPENAPI_ENABLED` gap — see §3.5's "Known gap" note. Real,
      still open, and no longer excused by a live-production-deployment
      reason now that FROZEN is lifted.

**SPA (`web-platform-back-office`, M4 — not a Rust repo, listed for
completeness)**

- [ ] `npm run typecheck && npm run lint && npm run test` all green.
- [ ] `FEATURE_KEYS` is a single union type; an invalid feature key literal
      fails `tsc`.
- [ ] `format-error.ts`: a server response with a `message` field always
      shows that message; the status-code table is a fallback only.
- [ ] `X-Request-Id` is attached on the seven instances whose CORS allows it
      (idp, keyServer, launcher, news, web, utility, mailer) and **not** on
      `authServer` or `api`.
- [ ] e2e preflight: every `FEATURE_KEYS` entry is a subset of
      `GET /api/v1/sites`'s live catalog (subset, not equality — the catalog
      legitimately has keys the SPA has no route for).
