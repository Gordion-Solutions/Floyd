//! Class 4: assert terminators.
//!
//! rustc's checked-arithmetic and bounds checks are `assert`
//! terminators in MIR. They must parse as `Assert` (not `Other`), so a
//! decision before or after one still decomposes.

/// `a && b` followed by a checked add (`Assert` terminator) before
/// the result is returned.
///
/// Expected: candidate over conditions `a`, `b` (no `lift`),
/// `invoked: true`, 3 observations.
pub fn assert_after_and(a: bool, b: bool, n: u32) -> (bool, u32) {
    let d = a && b;
    (d, n + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_after_and_tt() {
        assert_eq!(assert_after_and(true, true, 1), (true, 2));
    }

    #[test]
    fn assert_after_and_f() {
        assert_eq!(assert_after_and(false, true, 1), (false, 2));
    }

    #[test]
    fn assert_after_and_tf() {
        assert_eq!(assert_after_and(true, false, 1), (false, 2));
    }
}
