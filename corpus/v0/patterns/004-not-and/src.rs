//! Floyd corpus pattern 004-not-and.
//!
//! Negated AND: `!a && b`. rustc optimises the unary `!` into the
//! branching itself — the emitted MIR has the same shape as `a && b`
//! with the switchInt arms swapped. See `pattern.toml` for the
//! expected MC/DC analysis.

pub fn decide(a: bool, b: bool) -> bool {
    !a && b
}
