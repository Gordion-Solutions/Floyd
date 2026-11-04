//! A proc-macro crate with unit tests. Its test binary links the
//! toolchain's `libstd` dynamically, so it only starts with the
//! toolchain lib dir on the loader search path (`DYLD_LIBRARY_PATH`
//! / `LD_LIBRARY_PATH`), as `cargo test` sets it. Checked by
//! `cargo-floyd/tests/error_paths.rs`.

use proc_macro::TokenStream;

/// Passes its input through.
#[proc_macro]
pub fn identity(input: TokenStream) -> TokenStream {
    input
}

/// The decision Floyd analyses.
fn both(a: bool, b: bool) -> bool {
    a && b
}

#[cfg(test)]
mod tests {
    #[test]
    fn true_true() {
        assert!(super::both(true, true));
    }

    #[test]
    fn false_true() {
        assert!(!super::both(false, true));
    }

    #[test]
    fn true_false() {
        assert!(!super::both(true, false));
    }
}
