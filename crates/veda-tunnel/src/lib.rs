//! veda-tunnel — an independent process that bridges veda's retrieval to
//! external IMs over long connections. A standard `wk_` consumer of the veda
//! data plane; veda-server is untouched.
//!
//! See docs/plans/veda-tunnel-plan.md for the design.

pub mod admin;
pub mod config;
pub mod registry;
pub mod store;
pub mod veda;
pub mod wecom;
