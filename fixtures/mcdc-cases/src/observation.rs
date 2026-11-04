//! Class 1: observation collapse and ambiguity.
//!
//! `llvm-cov` branch counters are summed over every evaluation in a
//! test. A test that evaluates a two-condition decision twice with
//! different values must be recorded as ambiguous (no condition
//! vector), never collapsed into one fabricated vector.

/// `a && b`, evaluated twice by `collapse_and_loop` and once by
/// `collapse_and_once_tf`.
///
/// Expected (`--only=collapse_and`): verdict `analysed`, two
/// observations, one ambiguous (`collapse_and_loop`, `evaluations: 2`,
/// empty `inputs`, `result: null`) and one vector (`a=T, b=F -> F`);
/// both conditions `UNKNOWN`, so `summary.unknown == 2`,
/// `summary.percentage == null`, text output says "no percentage",
/// exit code 2.
pub fn collapse_and(a: bool, b: bool) -> bool {
    a && b
}

/// `a && b` with three tests that each evaluate it exactly once:
/// (T,T) -> T, (F,-) -> F, (T,F) -> F.
///
/// Expected (`--only=single_and`): three non-ambiguous observations,
/// both conditions exercised, `summary.percentage == 100`, exit 0.
pub fn single_and(a: bool, b: bool) -> bool {
    a && b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_and_loop() {
        let mut hits = 0;
        for (a, b) in [(true, true), (false, true)] {
            if collapse_and(a, b) {
                hits += 1;
            }
        }
        assert_eq!(hits, 1);
    }

    #[test]
    fn collapse_and_once_tf() {
        assert!(!collapse_and(true, false));
    }

    #[test]
    fn single_and_tt() {
        assert!(single_and(true, true));
    }

    #[test]
    fn single_and_f() {
        assert!(!single_and(false, true));
    }

    #[test]
    fn single_and_tf() {
        assert!(!single_and(true, false));
    }
}
