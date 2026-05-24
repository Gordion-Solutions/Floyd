//! Floyd corpus pattern 006-try-with-and.
//!
//! The `?` operator wraps a boolean `&&` decision. The decomposer
//! looks through the `?` plumbing (`Try::branch` + `discriminant` +
//! `switchInt` over `ControlFlow`) and recovers the inner `x && b`
//! as the MC/DC decision. See `pattern.toml` for the expected
//! analysis.

pub fn decide(opt: Option<bool>, b: bool) -> Option<bool> {
    let x = opt?;
    Some(x && b)
}
