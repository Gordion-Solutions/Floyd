//! Floyd corpus pattern 007-match-int-literal.
//!
//! Canonical `match` with an integer literal arm and a wildcard. The
//! decomposer recognises the non-bool `switchInt` and emits a
//! single-condition decision named `n == 0`. See `pattern.toml`.

pub fn decide(n: i32) -> bool {
    match n {
        0 => false,
        _ => true,
    }
}
