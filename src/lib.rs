//! vhrn runs coding agents ("harnesses") in a container jailed to the current
//! project, with default-deny network egress. This crate is the Rust port of the
//! Go CLI (`cmd/vhrn` + `internal/vhrn`); it is built alongside the Go binary
//! until the port completes. Logic lives here (testable); `src/main.rs` is a thin
//! shim. Comments explain why, not what, and stay terse.
#![forbid(unsafe_code)]
// Port in progress: leaf modules land bottom-up and are wired into dispatch at the
// cutover phase; until then some functions are exercised only by unit tests. Remove
// this allow once the Go CLI is deleted and everything is reachable.
#![allow(dead_code)]

mod cli;
mod config;
mod harness;
mod image;
mod net;

pub use cli::run;
