//! Per-hop readiness reporting for `/readyz`, implementing
//! `maxgame-readiness-contract/CONTRACT.md`.
//!
//! Two kinds of check exist (`CONTRACT.md` §2):
//!
//! * **gating** — the service's own datastores. They run inline on every
//!   `/readyz` request, and a failure answers 503 `unavailable` with a
//!   `dependency`.
//! * **reporting** — every upstream HTTP hop, object storage, mail provider
//!   and chain RPC. They run in the background against the same URL the
//!   request path uses, are cached, and never fail readiness. A request to
//!   `/readyz` never makes an upstream call (`CONTRACT.md` §6.1).
//!
//! Probes are **connection and configuration only**: a probe sends no
//! credential and never reports whether one is valid (`CONTRACT.md` §1.1).
//! The static credential checks at boot (`CONTRACT.md` §7) live in the
//! service, not here.
//!
//! Cargo features: `http` (default) adds the HTTP hop probes, `postgres`
//! adds [`PgProbe`] and [`health_pool`] on sqlx 0.8, and `redis` adds
//! [`RedisProbe`] on redis 1.x.
//!
//! ```
//! use std::time::Duration;
//! use maxgame_readiness::{AppEnv, CheckStatus, FnProbe, ProbeOutcome, Readiness};
//!
//! # #[tokio::main(flavor = "current_thread")] async fn run() {
//! let r2 = if false {
//!     Some(FnProbe::new(|| async { ProbeOutcome::ok() }))
//! } else {
//!     None // R2_* unset in this deployment
//! };
//!
//! let readiness = Readiness::builder("launcher", AppEnv::Development)
//!     .version("1.2.3")
//!     // own datastore: gating
//!     .gate("postgres", FnProbe::new(|| async { ProbeOutcome::ok() }))
//!     // upstream hop: reporting only. A 404 on the probe path means the
//!     // BASE_PATH is wrong, which is `unexpected`, not `unreachable`
//!     // (CONTRACT.md §3.5).
//!     .report(
//!         "key_server",
//!         FnProbe::new(|| async {
//!             ProbeOutcome::fail(CheckStatus::Unexpected, "not_found")
//!         })
//!         .target("http://key-server.platform.svc:8092/keys"),
//!     )
//!     // optional feature that is switched off in this deployment
//!     .optional("r2", r2)
//!     .interval(Duration::from_secs(30))
//!     .build();
//!
//! // The real request path closes the gap an interval leaves
//! // (CONTRACT.md §6.4): a request that fails with a 5xx, a timeout or a
//! // transport error re-checks that hop ahead of its schedule. A 4xx is an
//! // answer about that request and never triggers.
//! if maxgame_readiness::should_trigger(Some(503)) {
//!     readiness.hop_failed("key_server");
//! }
//!
//! // background runner: one cycle at once, then every interval, jittered
//! let runner = readiness.spawn();
//!
//! // mounted at the pod root, never under BASE_PATH
//! let app = readiness.router::<()>();
//! # let _ = (runner, app);
//! # }
//! ```
//!
//! Probes are added by feature-gated companion crates for each hop kind
//! (`CONTRACT.md` §6.2). This crate carries no HTTP or database client.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod env;
#[cfg(feature = "http")]
pub mod http;
pub mod model;
pub mod ping;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod probe;
mod readiness;
#[cfg(feature = "redis")]
pub mod redis_probe;

pub use env::{AppEnv, ParseAppEnvError};
#[cfg(feature = "http")]
pub use http::{
    classify_error_status, classify_transport_error, AuthEndpointProbe, ChainIdProbe, JwksProbe,
    PingProbe, ReachProbe,
};
pub use model::{CheckStatus, Identity, ProbeOutcome};
pub use ping::ping_router;
#[cfg(feature = "postgres")]
pub use postgres::{classify_sqlx_error, health_pool, PgProbe};
pub use probe::{sanitize_target, FnProbe, Probe};
pub use readiness::{
    is_probe_request, should_trigger, Readiness, ReadinessBuilder, DEFAULT_INTERVAL_SECONDS,
    DEFAULT_TRIGGER_COOLDOWN_SECONDS, INTERVAL_ENV_VAR, MAX_INTERVAL_SECONDS,
    MAX_TRIGGER_COOLDOWN_SECONDS, MIN_INTERVAL_SECONDS, MIN_TRIGGER_COOLDOWN_SECONDS, PROBE_HEADER,
    TRIGGER_COOLDOWN_ENV_VAR,
};
#[cfg(feature = "redis")]
pub use redis_probe::{classify_redis_error, RedisProbe};
