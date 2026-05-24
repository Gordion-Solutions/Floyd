//! Floyd corpus pattern 002-simple-or.
//!
//! Canonical two-condition OR with short-circuit evaluation. See
//! `pattern.toml` for the expected MC/DC analysis.

pub fn decide(a: bool, b: bool) -> bool {
    a || b
}
