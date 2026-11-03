# CI image for the test job in deploy-dev.yml — rust toolchain plus the
# pieces the official image's `minimal` rustup profile leaves out.
#
# This image lives ONLY in the runner's own local registry (localhost:5000,
# a `registry:2` container bound to 127.0.0.1 on the runner VM) — never in
# Docker Hub or Artifact Registry. That keeps the test job free of external
# pulls, pull credentials, and quota; the trade-off is that a fresh runner
# machine must rebuild it before workflows pass. Rebuild + push:
#
#   docker build -t localhost:5000/maxgame-ci-rust:1.97-r4 -f ci-image.Dockerfile .
#   docker push localhost:5000/maxgame-ci-rust:1.97-r4
#
# Bump the tag in lockstep with the rust version in every repo's Dockerfile
# (currently 1.97) and update deploy-dev.yml's `container.image` to match.
# A change to this file gets a NEW revision tag (`1.97-rN`); never rebuild a tag
# a workflow already uses, or every repo on it changes under its next run.
# Revisions on both office runners (108, 111): `1.97` = 2026-08-15 original
# (most fleet repos), `-r2` + docker CLI/compose + python3, `-r3` + the 1.95
# toolchains (the plaza repos), `-r4` + cargo-nextest.
FROM rust:1.97-slim-bookworm
# `docker-ce-cli` + `docker-compose-plugin` (compose v2) and `python3`: the CLI
# only, for checks that ask `docker compose config` what compose would run
# (maxgame-plaza `scripts/check-compose-env.sh`). `config` needs no daemon and
# no socket. From docker's own apt repo, as maxgame-web3's
# `tools/ci-image/Dockerfile` does: bookworm's `docker-compose` package is v1.
# Baked in here, never apt-installed per run: the runners' network is the
# flaky part (lean W471).
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config git ca-certificates curl gnupg python3 \
    && install -m 0755 -d /etc/apt/keyrings \
    && curl -fsSL https://download.docker.com/linux/debian/gpg \
        | gpg --dearmor -o /etc/apt/keyrings/docker.gpg \
    && echo "deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/debian bookworm stable" \
        > /etc/apt/sources.list.d/docker.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends docker-ce-cli docker-compose-plugin \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy \
    && docker compose version \
    && python3 --version
# The toolchains the repos on this image PIN in `rust-toolchain.toml`, baked
# in so a job never syncs one at run time (lean W503: every plaza run
# downloaded 1.95). Surveyed 2026-10-21 over each repo's default branch:
# `1.95` (maxgame-payment-gateway, -plaza, -plaza-e2e, -signer) and `1.95.0`
# (maxgame-wallet, -web3). Those are two rustup toolchains, not one, because
# `1.95` tracks the latest 1.95.x. Each pin asks for rustfmt and clippy. A repo
# with no pin runs this image's default (1.97). When a repo's pin moves, add it
# here and rebuild.
RUN rustup toolchain install 1.95 1.95.0 --profile minimal -c rustfmt -c clippy \
    && rustup run 1.95 cargo --version \
    && rustup run 1.95 cargo clippy --version \
    && rustup run 1.95.0 cargo --version \
    && rustup run 1.95.0 cargo clippy --version \
    && cargo --version | grep -q '^cargo 1\.97\.'
# cargo-nextest, pinned: a repo's `scripts/ci.sh` runs its suite under it. Baked
# in so the first run on each runner does not compile it (~9 min there). Keep
# the version in lockstep with NEXTEST_VERSION in those scripts, which fall back
# to installing their own copy when the two differ. The registry is dropped:
# CI mounts the runner's own over /usr/local/cargo/registry.
# `.global-cache` goes too, and auto-clean is off: that database records when
# each crate was last used, but CI mounts only the registry, so every container
# would start from build-time timestamps and cargo (>= 1.88) would garbage-
# collect the registry every repo shares on the runner.
ENV CARGO_CACHE_AUTO_CLEAN_FREQUENCY=never
RUN cargo install --locked --version 0.9.150 cargo-nextest \
    && rm -rf /usr/local/cargo/registry /usr/local/cargo/.global-cache \
    && cargo nextest --version | grep -qF ' 0.9.150'
