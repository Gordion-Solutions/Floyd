//! Floyd corpus pattern 008-inline-comparison.
//!
//! Canonical inline-comparison + `&&` shape. The `speed > 50`
//! produces a synthetic MIR bool temporary; the decomposer
//! recognises the comparison and synthesizes the condition name
//! `speed > 50`. See `pattern.toml`.

pub fn decide(speed: i32, brake: bool) -> bool {
    speed > 50 && brake
}
