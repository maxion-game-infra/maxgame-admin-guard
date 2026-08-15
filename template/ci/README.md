# `deploy-dev.yml` — copy-per-repo CI/CD template

Canonical source: [`deploy-dev.yml`](./deploy-dev.yml) in this directory. Every
Rust backend that deploys to the office k3s dev cluster gets its own **copy**
of this file at `.github/workflows/deploy-dev.yml` — same philosophy as the
platform contract + conformance-test template: one canonical source here,
copies in each repo, no shared include mechanism. If the workflow needs to
change, fix it here first and re-copy into every repo that carries it; don't
patch one repo's copy out of sync with the rest.

## How to copy this into a repo

1. Copy the file:
   ```bash
   cp maxgame-admin-guard/template/ci/deploy-dev.yml \
      <repo>/.github/workflows/deploy-dev.yml
   ```
2. Fill in the two placeholders in the `env:` block at the top —
   `APP` and `REPO_DIR` — using the table below.
3. Nothing else in the file should need to change. If your repo's
   `scripts/ci.sh`, `Dockerfile`, or `Dockerfile.dockerignore` don't follow
   the conventions this template assumes (sibling `maxgame-admin-guard/rust`
   checkout, `TEST_DATABASE_URL`-only test config, build context = workspace
   parent), fix those to match the fleet convention rather than diverging the
   workflow — see `maxgame-admin-guard/contract/PLATFORM.md`.
4. One-time repo wiring before the workflow can actually run end-to-end
   (not part of this copy step — see plan T2):
   - **WIF binding**: bind this repo to the existing `github-pool` /
     `github-provider` Workload Identity Federation setup with one
     `gcloud iam service-accounts add-iam-policy-binding` command (pool and
     provider are already active fleet-wide; only the per-repo binding is
     new — see `maxion-k3s-dev` skill, `references/gitops.md`).
   - **`GITOPS_SSH_KEY`**: already exists as an **org secret** on
     `maxion-game-infra` (a write-only deploy key scoped to
     `maxgame-dev-gitopt`) — every repo's copy of this workflow references it
     as-is, nothing to provision per repo.
5. Tag `dev-vX.Y.Z` to trigger.

## Per-repo values

| `APP` | `REPO_DIR` | Artifact Registry image | Notes |
| --- | --- | --- | --- |
| `admin-auth` | `maxgame-admin-auth-server` | `.../maxgame-platform-dev/admin-auth` | pilot repo — first to get this workflow |
| `auth-server` | `maxgame-auth-server` | `.../maxgame-platform-dev/auth-server` | |
| `key-server` | `maxgame-key-server` | `.../maxgame-platform-dev/key-server` | |
| `launcher` | `maxgame-launcher-backend` | `.../maxgame-platform-dev/launcher` | |
| `news` | `maxgame-news-backend` | `.../maxgame-platform-dev/news` | |
| `web` | `maxgame-web-backend` | `.../maxgame-platform-dev/web` | |
| `utility` | `maxgame-utility-server` | `.../maxgame-platform-dev/utility` | no DB — see note below |
| `mailer` | `maxgame-mail-server` | `.../maxgame-platform-dev/mailer` | |

Full image path is
`asia-southeast1-docker.pkg.dev/maxion-game-platform/maxgame-platform-dev/<APP>`
— the `APP` value doubles as the image name, so the two columns above always
match by construction.

### `utility` has no database

`maxgame-utility-server` has no Postgres dependency at all — it's a pure R2
presign service. Its `scripts/ci.sh` needs no `TEST_DATABASE_URL` and never
touches the `services: postgres` container this template starts. Copy the
template unmodified anyway: the unused Postgres service container is harmless
(it just starts and sits idle for that job), and keeping every repo's
workflow byte-for-byte identical is worth more than trimming one repo's
version of a step that costs nothing to leave in.

## What this workflow assumes about your repo

- `scripts/ci.sh` exists, is executable from the repo root, runs fmt+clippy+test,
  and takes its test database exclusively from `TEST_DATABASE_URL` — never
  `DATABASE_URL`/`MIGRATION_DATABASE_URL`, never a `.env` file.
- `Dockerfile` builds with **context = the workspace parent directory** (the
  directory containing both `<REPO_DIR>/` and `maxgame-admin-guard/`), because
  `maxgame-admin-guard/rust` is a Cargo path-dependency. A
  `Dockerfile.dockerignore` beside it trims that wide context back down to
  just what the build needs.
- `workloads/<APP>/deployment.yaml` exists in `maxgame-dev-gitopt` with an
  `image: asia-southeast1-docker.pkg.dev/.../<APP>:<tag>` line for the deploy
  job to rewrite.

## prod (`v*`)

No prod deploy exists yet. The bottom of `deploy-dev.yml` carries a
commented-out stub showing the same three-job shape pointed at a `v*` tag
trigger — do not uncomment or wire it up until a prod Artifact Registry repo
and gitops target actually exist.
