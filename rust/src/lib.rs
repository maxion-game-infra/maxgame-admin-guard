//! `maxion-admin-guard` — the Rust half of the admin-auth contract in
//! `../contract/README.md`.
//!
//! One thing every service that trusts an admin JWT needs to do identically:
//! verify it offline against the IdP's JWKS, ask the IdP live on a mutation,
//! trip a breaker rather than hammer a down IdP, and decide site/feature
//! access the same way for every token shape the platform has ever minted.
//! This crate is that logic, extracted out of `maxgame-launcher-backend`
//! (`src/infrastructure/admin_auth.rs` and neighbours) so
//! `maxgame-key-server` and `maxgame-auth-server` do not have to write it a
//! third and fourth time.
//!
//! Two ways to use it:
//! - **Assemble the pieces yourself** ([`token::AdminTokenVerifier`],
//!   [`introspect_client::AdminIntrospectClient`], [`access::AdminIdentity`])
//!   if your service already has its own middleware structure. This is what
//!   `maxgame-launcher-backend` does.
//! - **Use [`guard::AdminGuard`]** for the whole contract in one call, plus
//!   [`axum_ext::require_admin`] and [`axum_ext::VerifiedAdmin`] to wire it
//!   into an axum router with no code of your own.
//!
//! This crate never reads an environment variable — [`guard::GuardConfig`]
//! is built from explicit values the host service already owns. See
//! contract rule 8: a verifier missing its configuration refuses requests,
//! it does not fall back to reading the environment as an escape hatch.

pub mod access;
pub mod axum_ext;
pub mod circuit_breaker;
pub mod clock;
pub mod error;
pub mod guard;
pub mod introspect;
pub mod introspect_client;
pub mod jwks;
pub mod token;

pub use access::{normalize_site_access, AdminIdentity, SiteAccess, ROLE_SUPER_ADMIN};
pub use axum_ext::{bearer_token, require_admin, VerifiedAdmin};
pub use circuit_breaker::{CircuitBreaker, RequestPermit};
pub use clock::{Clock, FakeClock, SystemClock};
pub use error::{GuardError, GuardResult};
pub use guard::{AdminGuard, AuthorizedAdmin, GuardConfig, Requirement};
pub use introspect::{AdminIntrospection, AdminIntrospector, IntrospectionBody};
pub use introspect_client::AdminIntrospectClient;
pub use jwks::{AdminJwksClient, AdminJwksClientConfig, JwksKeyProvider};
pub use token::{AdminTokenVerifier, VerifiedAdminToken};
