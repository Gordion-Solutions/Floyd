//! Floyd corpus pattern 001-simple-and.
//!
//! Canonical two-condition AND with short-circuit evaluation. See
//! `pattern.toml` for the expected MC/DC analysis.

pub fn decide(a: bool, b: bool) -> bool {
    a && b
}
