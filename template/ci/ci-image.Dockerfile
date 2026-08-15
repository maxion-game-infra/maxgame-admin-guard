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
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config git ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy
