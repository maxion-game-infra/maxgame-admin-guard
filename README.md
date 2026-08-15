# maxgame-admin-guard

One definition of admin request authorization, and two implementations of it
that are held to that definition by a shared test suite.

```
contract/   cases.json + the eight rules. The spec. Read this first.
rust/       axum middleware + extractor. The crate is named
            `maxion-admin-guard` (only the repository was renamed).
js/         NestJS guard + framework-free middleware.
template/   a scaffold for a new Rust service that starts out conformant.
```

Both implementations run `contract/cases.json` as their test suite. They live in
one repository on purpose: the risk worth engineering against is not that either
one is wrong on its own, but that they drift apart — and then the same admin
token authorizes a request on one service and is refused on another. Co-located,
a divergence fails a test; split across repos, it ships.

Neither implementation was written from scratch. The behaviour was already
implemented and tested three times over — in `web-platform-backend`'s
`src/libs/admin-idp/`, `maxgame-launcher-backend`'s `admin_auth.rs`, and
`maxgame-key-server`'s `idp_jwt.rs`. This repository extracts the most complete
of those rather than reinventing it.

## Who depends on this

`rust/` is a dependency of all eight backend services on the platform:
`maxgame-admin-auth-server` (the IdP — dev-dependency only, for its issuer
conformance test), `maxgame-auth-server`, `maxgame-key-server`,
`maxgame-launcher-backend`, `maxgame-mail-server`, `maxgame-news-backend`,
`maxgame-utility-server`, `maxgame-web-backend`.

`js/` is consumed by `web-platform-backend`, now only by its regression tests —
the NestJS admin surface those guards protected has been retired, so the JS
implementation exists to keep the contract honest in a second language rather
than to serve live traffic.

## Consuming it

Rust services take a **path dependency** on `rust/`, with this repository
checked out as a sibling directory:

```
<parent>/
  maxgame-admin-guard/      <- this repository
  maxgame-news-backend/     <- Cargo.toml: { path = "../maxgame-admin-guard/rust" }
```

That is deliberate. The guard is the platform's only shared code, so a change
here affects eight services at once — a path dependency means a consumer's test
suite runs against your edit immediately, with no publish/version-bump round
trip in between. The Dockerfiles build from the parent directory for the same
reason, and CI checks out both repositories side by side.

A service adopting the library should end up with *less* code than before, not
more. If adopting it means writing new auth logic in the service, the logic
belongs here instead.

## Why this repository is public

It is published so that CI in the consuming repositories can check it out
without managing credentials, and so the contract can be linked to. It is not
a general-purpose library and carries no support or stability promise for use
outside this platform: `contract/` describes one specific identity provider's
behaviour, and the interfaces change whenever that platform needs them to.

Nothing secret lives here. `contract/fixtures/` holds an Ed25519 keypair, but it
is a test key generated for that directory and has never signed anything real.

## License

Apache-2.0 — see [LICENSE](LICENSE).
