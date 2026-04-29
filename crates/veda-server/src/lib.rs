//! veda-server library surface.
//!
//! Modules are exposed so integration tests (and future companion binaries
//! like veda-migrate, veda-worker) can wire them up without going through
//! the HTTP layer.

pub mod auth;
pub mod config;
pub mod error;
pub mod routes;
pub mod state;
pub mod worker;
