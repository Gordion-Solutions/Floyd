//! Floyd corpus pattern 005-if-let-some.
//!
//! Canonical `if let` with a binding: the pattern match contributes a
//! synthetic boolean condition (`opt is Some`); the matched arm uses
//! the bound value. See `pattern.toml` for the expected MC/DC analysis.

pub fn decide(opt: Option<bool>) -> bool {
    if let Some(x) = opt {
        x
    } else {
        false
    }
}
