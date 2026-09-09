//! Runtime wiring will live here.
#![forbid(unsafe_code)]

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
