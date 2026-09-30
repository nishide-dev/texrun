//! Core library for texrun.
//!
//! This crate will host the compile domain model and the typesetting engine
//! interface. It is intentionally minimal for now.

/// The version of the texrun core library.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_matches_package_version() {
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        assert!(!VERSION.is_empty());
    }
}
