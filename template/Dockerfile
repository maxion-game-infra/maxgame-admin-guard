# Multi-stage build for maxgame-template-server.
#
# Requires BuildKit (default since Docker 23) for the cache mounts, which keep
# the dependency tree out of every rebuild without the dummy-main trick that
# silently goes stale.
#
# ── THE BUILD CONTEXT IS THE PARENT DIRECTORY ─────────────────────────
#
#   cd template
#   docker build -f Dockerfile -t maxgame-template-server ..
#
# `maxion-admin-guard` is a **path dependency** (`../rust` in Cargo.toml,
# from inside `maxion-admin-guard/`) because no service repo has a git
# remote yet. Cargo resolves that path before it compiles anything, so a
# build whose context is this directory alone fails at `cargo build` with
# "failed to load manifest for dependency" no matter what the Dockerfile
# does — the file is simply not in the context. Hence the parent.
#
# ⚠️ WHEN YOU COPY THIS TEMPLATE OUT to become a real service repo
# (`maxgame-foo-server/`, a sibling of `maxion-admin-guard/`), the COPY
# paths below and Cargo.toml's path-dep both need their leading
# `template/` replaced with `maxion-admin-guard/` — see the warning at the
# top of `Cargo.toml`. The build context command above also changes to
# `docker build -f Dockerfile -t maxgame-foo-server ../..` (one more level
# up) since the new repo is no longer nested inside `maxion-admin-guard/`.
#
# ── BUILDKIT IS REQUIRED, AND NOT ONLY FOR THE CACHE MOUNTS ───────────
#
# `<dockerfile>.dockerignore` is a BuildKit feature. The legacy builder
# reads only `<context>/.dockerignore`, and the context here is
# `maxion-admin-guard/`, which has none — a legacy build would try to
# upload the whole crate (including every OTHER repo's build artifacts, if
# run from a shared parent) before failing on the `RUN --mount` below.
# BuildKit is the default from Docker 23 and matches the pattern
# `maxgame-news-backend`'s Dockerfile already uses (the one repo in the
# fleet that had this working from the start).
#
# The image carries no configuration and no keys — every COPY below names
# exactly what it takes, so no `.env` can be swept in. Supply DATABASE_URL
# and the rest through the environment. Migrations are compiled into the
# binary by `sqlx::migrate!`, so they need no separate copy at runtime.

# ── builder ───────────────────────────────────────────────────────────
FROM rust:1.97-slim-bookworm AS builder

WORKDIR /build

# rustls everywhere means no openssl headers; pkg-config is still wanted by a
# couple of build scripts in the tree.
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

# The sibling crate first: it changes far less often than this service, so it
# earns its own layer.
COPY rust ./rust

COPY template/Cargo.toml template/Cargo.lock template/build.rs ./template/
COPY template/src ./template/src
COPY template/migrations ./template/migrations

WORKDIR /build/template

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/template/target \
    cargo build --release --locked --bin maxgame-template-server \
    && cp target/release/maxgame-template-server /usr/local/bin/

# ── runtime ───────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# ca-certificates is required: the server fetches the IdP's JWKS and calls its
# introspection endpoint over HTTPS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 template-server \
    && useradd --system --uid 10001 --gid template-server --no-create-home \
       --shell /usr/sbin/nologin template-server

WORKDIR /app
COPY --from=builder /usr/local/bin/maxgame-template-server /usr/local/bin/

# Nothing in /app needs writing at runtime, and the process never needs root.
USER 10001:10001

ENV HOST=0.0.0.0 \
    PORT=8097 \
    RUST_LOG=info,template_server=info

EXPOSE 8097

# No HEALTHCHECK on purpose. Kubernetes ignores it and uses the probes in the
# manifest, and adding one here would mean shipping curl just to call our own
# port. Point the probes at the two endpoints the server already serves:
#
#   livenessProbe  -> GET /healthz   (never touches the database, so a
#                                     database blip cannot restart a healthy
#                                     process)
#   readinessProbe -> GET /readyz    (checks the database, so traffic drains
#                                     instead of erroring)

# SIGTERM is what Kubernetes and Compose send; the server drains in-flight
# requests on it. Run the binary as PID 1 directly so it receives the signal
# rather than a shell swallowing it.
ENTRYPOINT ["/usr/local/bin/maxgame-template-server"]
