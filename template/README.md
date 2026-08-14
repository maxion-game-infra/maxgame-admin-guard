# maxgame-template-server

Skeleton backend for the Maxion admin platform — copy this **whole
directory** out to start a new service, already conformant with
[`../contract/PLATFORM.md`](../contract/PLATFORM.md) before you write a
single domain route.

## What's here

| Piece | File(s) | Keep or replace? |
| --- | --- | --- |
| Config loader, `AppEnv` 3-tier, deployment guardrails | `src/config.rs` | **Keep** — copy unchanged except `PORT`'s default (pick the next free one, see `PLATFORM.md` §3.1) |
| Error envelope `{statusCode, message, error}` | `src/domain/error.rs`, `src/inbound/error.rs` | **Keep** |
| Health (`/healthz`, `/readyz`, always at root) | `src/inbound/health.rs` | **Keep** |
| `BASE_PATH` + CORS (incl. `x-request-id`) | `src/inbound/router.rs` | **Keep** |
| Admin-guard wiring | `src/inbound/mod.rs` (`AppState::new`), `src/inbound/router.rs` (`require_admin`) | **Keep** |
| Bind/serve/graceful-shutdown | `src/infrastructure/server.rs` | **Keep** |
| Postgres connect/migrate | `src/adapters/postgres.rs` | **Keep** |
| **Everything about `ExampleItem`** | `src/domain/mod.rs`, `src/adapters/example_repo.rs`, `src/inbound/example.rs`, `migrations/20260101000001_example_items.sql` | **Replace** with your real domain |

`src/inbound/example.rs` demonstrates the two route shapes every admin
service needs:

- `GET /example` — **public**, no token. Shows the minimal public-route
  pattern (`PLATFORM.md` §2.2 has no single canonical shape here yet).
- `GET /admin/example`, `POST /admin/example` — **admin, feature-gated**.
  The `GET` is offline-verified only; the `POST` additionally pays for live
  introspection (`PLATFORM.md` §1.1's admin-auth contract, rule 4) — watch
  `tests/example.rs` to see the 401-vs-503 split this buys you for free.
  `GET /admin/example`'s list shape (`page`/`take` query, `{items, meta}`
  response) is `PLATFORM.md` §2.1 verbatim; copy it for any new admin list
  endpoint.

## Copying this out to become a real service

1. `cp -r template ../maxgame-<name>-server && cd ../maxgame-<name>-server`
   — it is now a **sibling** of `maxgame-admin-guard/`, not a child of it.
2. **Fix the path dependency**: `Cargo.toml`'s `maxion-admin-guard = { path
   = "../rust" }` becomes `{ path = "../maxgame-admin-guard/rust" }` — see
   the warning comment right above that line. Same fix in `Dockerfile`'s
   `COPY rust ./rust` (→ `COPY maxgame-admin-guard/rust ./maxgame-admin-guard/rust`)
   and its build command (context becomes `../..`, one more level up).
3. Rename the crate: `Cargo.toml`'s `[package] name`, `[lib] name`,
   `[[bin]] name`, and every `template_server`/`maxgame-template-server`
   string in `src/main.rs`, `Dockerfile`, `tests/common/mod.rs`'s imports.
4. Delete `domain::ExampleItem`, `adapters::example_repo`,
   `inbound::example`, and the example migration; write your real ones in
   their place. Update `router.rs` to mount your routes instead.
5. Pick a real port (not 8097 — that's the template's placeholder) and
   update `.env.example`, `Dockerfile`'s `ENV PORT=`/`EXPOSE`, and
   `back-office-workspace/.scripts/dev.sh`'s `SERVICES` array.
6. Pick a real `EXAMPLE_SITE`/`EXAMPLE_FEATURE` pair (or however many your
   routes need) from the IdP's live catalog (`GET /api/v1/sites`) — see the
   workspace `CLAUDE.md`'s "Invariant สำคัญ" section on why that catalog,
   not a hardcoded constant file, is the source of truth.
7. `cargo test` — the whole suite, including `tests/platform_conformance.rs`,
   should still pass unmodified at this point. If something breaks, you
   likely touched a "Keep" file above in a way the contract doesn't allow;
   check `PLATFORM.md` before adjusting the test.

## Running it

```bash
cp .env.example .env    # then fill in real values
docker compose -f ../../.infra/docker-compose.yml up -d postgres  # or point
                         # DATABASE_URL at any Postgres you have
cargo run                # server on :8097 (or whatever PORT you set)
cargo test                # needs its own disposable Postgres — never point
                           # DATABASE_URL at a database another test run is
                           # using concurrently
docker build -f Dockerfile -t maxgame-template-server ..  # context is the
                                                            # parent directory
```

## Contract

Cross-service conventions — error envelope, pagination, ports/health/CORS,
`BASE_PATH`, S2S verification — are documented once for the whole platform
at [`../contract/PLATFORM.md`](../contract/PLATFORM.md). The eight
admin-auth rules every verifier implements live at
[`../contract/README.md`](../contract/README.md). This crate's own
conformance test against the platform contract is
`tests/platform_conformance.rs` — every item in `PLATFORM.md` §7's "Every
Rust repo" checklist is proven somewhere in this test suite; the file's own
doc comment says where.
