//! vhrn runs coding agents ("harnesses") in a container jailed to the current
//! project, with default-deny network egress. Logic lives here (testable);
//! `src/main.rs` is a thin shim. Comments explain why, not what, and stay terse.
#![forbid(unsafe_code)]

mod cli;
mod config;
mod env;
mod harness;
mod image;
mod logging;
mod net;
mod persist;
mod run;
mod shell;

#[cfg(test)]
mod testutil;

pub use cli::run;
pub use logging::init_logging;
