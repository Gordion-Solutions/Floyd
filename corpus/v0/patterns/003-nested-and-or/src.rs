//! Floyd corpus pattern 003-nested-and-or.
//!
//! First nested pattern: `(a && b) || c`. Exercises chained
//! short-circuit across both operators. See `pattern.toml` for the
//! expected MC/DC analysis.

pub fn decide(a: bool, b: bool, c: bool) -> bool {
    (a && b) || c
}
