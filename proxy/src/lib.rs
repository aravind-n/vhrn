//! Values and decisions for the network boundary.
#![forbid(unsafe_code)]

mod broker;
pub mod config;
pub mod diagnostics;
mod idle_pool;
pub mod policy;
pub mod public;
pub mod relay;
pub mod service;
pub mod target;

/// Returns this package's Cargo name.
#[must_use]
pub fn package_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::package_name;

    #[test]
    fn reports_its_package_name() {
        assert_eq!(package_name(), "vhrn-proxy");
    }
}
