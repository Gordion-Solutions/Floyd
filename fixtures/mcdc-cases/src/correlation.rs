//! Class 2: name-based correlation.
//!
//! MIR names impl methods `<impl at FILE:L:C: L:C>::name`, while the
//! coverage symbol is the demangled path; generic functions have one
//! MIR body and one coverage record per instantiation; closures are
//! `fn::{closure#0}`. Each must be joined to coverage by source span
//! and show `invoked: true` with observations.

/// A receiver for the inherent and trait method cases.
pub struct Gate;

impl Gate {
    /// Inherent method: `armed || forced`.
    ///
    /// Expected: candidate `invoked: true`, 3 observations, no
    /// `not_measurable`.
    pub fn impl_or(&self, armed: bool, forced: bool) -> bool {
        armed || forced
    }
}

/// Trait for the trait-impl method case.
pub trait Check {
    /// Combine two flags.
    fn trait_and(&self, left: bool, right: bool) -> bool;
}

impl Check for Gate {
    /// Trait impl method: `left && right`.
    ///
    /// Expected: candidate `invoked: true`, 3 observations.
    fn trait_and(&self, left: bool, right: bool) -> bool {
        left && right
    }
}

/// Generic function: `p || q`, instantiated for `u8` and `u16` by
/// different tests. The two instantiations' coverage records must be
/// summed into one candidate.
///
/// Expected (`--only=generic_or`): `invoked: true`, 3 observations,
/// both conditions exercised (the pairs need vectors from both
/// instantiations), percentage 100, exit 0.
pub fn generic_or<T: Copy>(_tag: T, p: bool, q: bool) -> bool {
    p || q
}

/// Closure capturing an outer bool: `x && enable`.
///
/// Expected: the `{closure#0}` body is a candidate, `invoked: true`,
/// 3 observations.
pub fn closure_and(x: bool, enable: bool) -> bool {
    let f = |v: bool| v && enable;
    f(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impl_or_ff() {
        assert!(!Gate.impl_or(false, false));
    }

    #[test]
    fn impl_or_t() {
        assert!(Gate.impl_or(true, false));
    }

    #[test]
    fn impl_or_ft() {
        assert!(Gate.impl_or(false, true));
    }

    #[test]
    fn trait_and_tt() {
        assert!(Gate.trait_and(true, true));
    }

    #[test]
    fn trait_and_f() {
        assert!(!Gate.trait_and(false, true));
    }

    #[test]
    fn trait_and_tf() {
        assert!(!Gate.trait_and(true, false));
    }

    #[test]
    fn generic_or_u8_ff() {
        assert!(!generic_or(1u8, false, false));
    }

    #[test]
    fn generic_or_u16_t() {
        assert!(generic_or(1u16, true, false));
    }

    #[test]
    fn generic_or_u8_ft() {
        assert!(generic_or(1u8, false, true));
    }

    #[test]
    fn closure_and_tt() {
        assert!(closure_and(true, true));
    }

    #[test]
    fn closure_and_f() {
        assert!(!closure_and(false, true));
    }

    #[test]
    fn closure_and_tf() {
        assert!(!closure_and(true, false));
    }
}
