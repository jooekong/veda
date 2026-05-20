//! Library facade over the veda-cli binary. The binary remains the
//! primary product; this lib exists so sibling crates (notably
//! `veda-fuse`) can reuse the on-disk config loader without
//! duplicating the schema. Keep the public surface small: only
//! re-export modules that are stable enough to share.
pub mod config;
