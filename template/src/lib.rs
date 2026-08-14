//! Skeleton backend for the Maxion admin platform — copy this crate wholesale
//! to start a new service, then delete `domain::ExampleItem`,
//! `adapters::example_repo`, and `inbound::example` once real routes replace
//! them. Everything else (config, error envelope, health, `BASE_PATH`, CORS,
//! admin-guard wiring) is meant to survive unchanged.
//!
//! See `README.md` for what to rename and `maxgame-admin-guard/contract/PLATFORM.md`
//! for why each piece is shaped the way it is.

pub mod adapters;
pub mod config;
pub mod domain;
pub mod inbound;
pub mod infrastructure;
