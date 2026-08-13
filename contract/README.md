# admin-auth contract

The single definition of how a service decides whether an admin request is
allowed. Two libraries implement it — a Rust crate and an npm package — and both
run `cases.json` as a test suite, so a behavioural difference between them fails
CI rather than surfacing as "works on launcher, 403s on auth-server".

The contract was not invented here. It is the behaviour already implemented and
tested three times over in `web-platform-backend/src/libs/admin-idp/`,
`maxgame-launcher-backend/src/infrastructure/admin_auth.rs` and
`maxgame-key-server/src/infrastructure/idp_jwt.rs`; this directory extracts it so
the fourth implementation does not have to be written from scratch.

## The eight rules

1. **Credential** — only `Authorization: Bearer <admin JWT>`. No shared secret,
   no API key, no query parameter carries admin identity.
2. **Offline verification, every request** — EdDSA over the IdP's
   `/.well-known/jwks.json`, `kid` required in the header, `iss` compared
   byte-for-byte against configuration, `exp` in the future, `type == "admin"`.
   `aud` is deliberately **not** checked (the IdP mints none today); a verifier
   must not start requiring it until the audience rollout says so.
3. **Authorization** — `role == "super_admin"` satisfies every check.
   Otherwise the required feature key must appear in `siteAccess[site]`.
4. **Live introspection on mutations** — `POST {idp}/api/v1/oauth/introspect`
   with header `x-api-key` for `POST|PUT|PATCH|DELETE`. The verdict's `role` and
   `siteAccess` **replace** the token's before the authorization decision.
   `GET|HEAD|OPTIONS` never introspect.
5. **Fail closed, with the status distinguishing the cause** — a verdict of
   `active:false` is 401; a failure to obtain a verdict at all (timeout,
   network, non-2xx, open breaker, malformed body) is 503. These must not
   collapse into one status: the first means "this session ended", the second
   means "we cannot tell", and only the second should page anyone. This covers
   the key set as well as introspection: a JWKS endpoint that is unreachable,
   answers non-2xx, or returns something unparseable is a dependency outage
   (503), while a key set that arrives fine and simply lacks the token's `kid`
   is a bad credential (401).
6. **Empty access** — a non-super-admin whose `siteAccess` has no entries gets
   403 before any feature check. A site key present with an empty array counts
   as having the site, and then fails the feature check.
7. **Breaker** — 5 consecutive transport failures open it for 30s, then one
   half-open trial. Any 2xx records success, so a stream of legitimately
   revoked tokens must never open it.
8. **No escape hatch** — no environment variable, debug flag or dev mode may
   bypass any of the above. A verifier that cannot reach its configuration
   refuses requests rather than allowing them.

## Shape normalisation

`siteAccess` arrives in two shapes and both must yield the same grants:

```jsonc
{ "zone4-back-office": ["news-management"] }              // current
{ "zone4-back-office": { "news-management": "edit" } }    // legacy: keys only
```

A value that is neither array nor object contributes no entry at all. Output is
deduplicated and sorted, so two tokens carrying the same grants in a different
order authorize identically.

## Running the suite

Each library reads `cases.json`, feeds every case through its own verifier with
introspection stubbed to the case's `introspect` field, and asserts `expect`.
A case's `id` is stable — reference it in bug reports.

`fixtures/` holds the Ed25519 keypair the cases are signed with. It is a test
key generated for this directory and has never signed anything real.
