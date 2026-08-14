# maxgame-admin-guard

> The Rust crate inside `rust/` is still named `maxion-admin-guard` — consumers depend on it by that name; only the repository/directory was renamed.

One definition of admin request authorization, and two implementations of it
that are held to that definition by a shared test suite.

```
contract/   cases.json + the eight rules. The spec. Read this first.
rust/       axum middleware + extractor. Used by launcher-backend,
            key-server, auth-server.
js/         NestJS guard + framework-free middleware. Used by
            web-platform-backend, and by email-server when it leaves GCP.
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

## Consuming it

Rust, until this is published: a path or git dependency on `rust/`.
JS, until this is published: a path dependency on `js/`, then GitHub Packages
under the `maxion-game` org.

A service adopting the library should end up with *less* code than before, not
more. If adopting it means writing new auth logic in the service, the logic
belongs here instead.
