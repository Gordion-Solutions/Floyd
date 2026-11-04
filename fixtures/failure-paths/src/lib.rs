//! Fixture for `cargo floyd test` error paths and options (see
//! `cargo-floyd/tests/error_paths.rs`).
//!
//! - Without `--features builds` the crate does not compile, so the
//!   instrumented build fails with the `compile_error!` below.
//! - With `--features builds` it analyses `gate` (feature passthrough).

#[cfg(not(feature = "builds"))]
compile_error!("failure-paths fixture: build with --features builds");

/// The decision Floyd analyses.
pub fn gate(a: bool, b: bool) -> bool {
    a && b
}

#[cfg(test)]
mod tests {
    #[test]
    fn both() {
        assert!(super::gate(true, true));
    }

    #[test]
    fn first_false() {
        assert!(!super::gate(false, true));
    }

    #[test]
    fn second_false() {
        assert!(!super::gate(true, false));
    }
}
