# CI image for the test job in deploy-dev.yml — rust toolchain plus the
# pieces the official image's `minimal` rustup profile leaves out.
#
# This image lives ONLY in the runner's own local registry (localhost:5000,
# a `registry:2` container bound to 127.0.0.1 on the runner VM) — never in
# Docker Hub or Artifact Registry. That keeps the test job free of external
# pulls, pull credentials, and quota; the trade-off is that a fresh runner
# machine must rebuild it before workflows pass. Rebuild + push:
#
#   docker build -t localhost:5000/maxgame-ci-rust:1.97 -f ci-image.Dockerfile .
#   docker push localhost:5000/maxgame-ci-rust:1.97
#
# Bump the tag in lockstep with the rust version in every repo's Dockerfile
# (currently 1.97) and update deploy-dev.yml's `container.image` to match.
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
