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

## Qualified grants (`feature@qualifier`)

A grant string may carry a qualifier after an `@`:

```jsonc
{
  "maxion-game-back-office": [
    "launcher-game-releases@mu-maxage",   // scoped to one game tenant
    "launcher-game-releases@snake",       // …and another
    "launcher-analytics"                  // unqualified
  ]
}
```

**This changes nothing in this contract.** `siteAccess` is still
`Record<site, string[]>`, normalisation is still sort-dedupe over opaque
strings, and rule 3 still matches by plain string equality. Neither
implementation splits on `@`, prefix-matches, or otherwise gives the separator
meaning — the six `qualified-*` cases in `cases.json` exist to keep it that
way, and they pass against the pre-existing code unchanged.

Two consequences follow directly from equality-matching, and both are
deliberate:

1. **An unqualified requirement is NOT satisfied by a qualified grant.**
   `has_feature(site, "launcher-coupons")` is `false` for an admin holding only
   `launcher-coupons@mu-maxage`. A service that has not been taught the
   qualifier vocabulary therefore refuses rather than over-grants — a partial
   rollout across the fleet fails **closed**.
2. **A qualified requirement is NOT satisfied by an unqualified grant.** A
   service must never widen a bare grant by guessing qualifiers; if it wants
   "bare means all", that is a rule the *service* implements on top of the
   grants this contract hands it, not something this contract does for it.

Super admins bypass both, per rule 3, and never need qualified grants minted.

**What owns the qualifier vocabulary.** This contract does not: it neither
validates nor interprets the text after `@`. The IdP
(`maxgame-admin-auth-server`) decides which feature keys may be qualified and
what a qualifier may look like, and the enforcing service decides what it
means. Today exactly one vocabulary exists — game tenants on the four
tenant-scoped launcher keys (`launcher-games`, `launcher-maintenance`,
`launcher-coupons`, `launcher-game-releases`), enforced by
`maxgame-launcher-backend`. A second vocabulary would need no change here
either, which is the point.

## Known gap: a JWKS with duplicate `kid`s

Not covered by a case yet, and the two implementations disagree — recorded here
so it is not rediscovered from scratch.

If the IdP ever publishes two keys sharing a `kid`, Rust's key map does
`insert(kid, key)` in document order, so the last one silently wins and which
key verifies a token depends on document ordering. JS raises jose's
`JWKSMultipleMatchingKeys` and currently answers 401.

Neither is obviously right, but 401 is the least defensible of the three
options: the token is not implicated, the key set is. By the same reasoning as
rule 5 this is a "cannot tell" and should be 503 in both. Closing it means a
case here, duplicate detection in Rust, and a reclassification in JS — worth
doing, not urgent, since it takes an IdP misconfiguration to reach.

## Running the suite

Each library reads `cases.json`, feeds every case through its own verifier with
introspection stubbed to the case's `introspect` field, and asserts `expect`.
A case's `id` is stable — reference it in bug reports.

`fixtures/` holds the Ed25519 keypair the cases are signed with. It is a test
key generated for this directory and has never signed anything real.
