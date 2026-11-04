//! Class 1: observation collapse and ambiguity.
//!
//! `llvm-cov` branch counters are summed over every evaluation in a
//! test. A test that evaluates a decision several times with
//! different values must never be collapsed into one fabricated
//! vector: Floyd derives its vectors only when the counters
//! determine them (here: exactly one condition took both values, or
//! the decision is a tree, whose counters are its edge flows), and
//! otherwise records the test as ambiguous (no condition vector).
//! An ambiguous test leaves a condition unknown only if it took the
//! outcomes a missing independence-pair row needs.

/// `a && b`, evaluated twice by `collapse_and_loop` and once by
/// `collapse_and_once_tf`.
///
/// `collapse_and_loop` evaluates (T,T) -> T then (F,-) -> F, so its
/// counters are a = [1 T, 1 F], b = [1 T, 0 F]: only `a` took both
/// values, so the counters determine both vectors.
///
/// Expected (`--only=collapse_and`): verdict `analysed`, three
/// vector observations, none ambiguous: `collapse_and_loop`'s
/// (`a=T, b=T -> T` and `a=F -> F`, each `evaluations: 2`) and
/// `collapse_and_once_tf`'s (`a=T, b=F -> F`); both conditions
/// exercised, `summary.percentage == 100`, exit 0.
pub fn collapse_and(a: bool, b: bool) -> bool {
    a && b
}

/// `a && b`, evaluated three times by `tree_and_loop`: (T,T) -> T,
/// (T,F) -> F and (F,-) -> F, so a = [2 T, 1 F], b = [1 T, 1 F].
/// Both conditions took both values, but `a && b` is a tree: b's
/// total (2) is a's true count, and the three leaves carry one
/// evaluation each.
///
/// Expected (`--only=tree_and`): verdict `analysed`, three vector
/// observations from the one test, none ambiguous, each
/// `evaluations: 1` and `resolved_by: "tree_counters"`; both
/// conditions exercised, `summary.percentage == 100`, exit 0.
pub fn tree_and(a: bool, b: bool) -> bool {
    a && b
}

/// `a && (b || c)`, evaluated four times by `and_or_tree_loop`:
/// (T,T,-) -> T, (T,F,T) -> T, (T,F,F) -> F, (F,-,-) -> F, so
/// a = [3 T, 1 F], b = [1 T, 2 F], c = [1 T, 1 F]. Every condition
/// is reached along one path only (a tree), so the counters give
/// every vector.
///
/// Expected (`--only=and_or_tree`): four vector observations, each
/// `evaluations: 1` and `resolved_by: "tree_counters"`; all three
/// conditions exercised, `summary.percentage == 100`, exit 0.
pub fn and_or_tree(a: bool, b: bool, c: bool) -> bool {
    a && (b || c)
}

/// `(a || b) && c` (as the `if` of an early return: the bare
/// `bool` form of this shape is one the decomposer declines),
/// evaluated twice by `dag_or_and_loop`: (T,-,T) -> T and
/// (F,T,F) -> F, so a = [1 T, 1 F], b = [1 T, 0 F], c = [1 T, 1 F].
/// Not a tree: `c` is reached both from `a = T` and from
/// `a = F, b = T`, and (T,-,F) + (F,T,T) leaves the same counters,
/// so the test is ambiguous. `a`'s and `b`'s pairs both need
/// (F,F,-) -> F, and no test ever made `b` false.
///
/// Expected (`--only=dag_or_and`): one ambiguous observation
/// (`evaluations: 2`, empty `inputs`, `result: null`, the raw
/// `counts`); `c` `UNKNOWN`; `a` and `b` `UNEXERCISED`, with
/// `analysis.ruled_out.b` naming `dag_or_and_loop`'s zero `b = F`
/// counter; `summary.unknown == 1`, `summary.percentage == null`,
/// text output says "no percentage", exit code 2.
pub fn dag_or_and(a: bool, b: bool, c: bool) -> Option<u8> {
    if (a || b) && c {
        return None;
    }
    Some(1)
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
    fn tree_and_loop() {
        let mut hits = 0;
        for (a, b) in [(true, true), (true, false), (false, true)] {
            if tree_and(a, b) {
                hits += 1;
            }
        }
        assert_eq!(hits, 1);
    }

    #[test]
    fn and_or_tree_loop() {
        let mut hits = 0;
        for (a, b, c) in [
            (true, true, true),
            (true, false, true),
            (true, false, false),
            (false, true, true),
        ] {
            if and_or_tree(a, b, c) {
                hits += 1;
            }
        }
        assert_eq!(hits, 2);
    }

    #[test]
    fn dag_or_and_loop() {
        let mut hits = 0;
        for (a, b, c) in [(true, false, true), (false, true, false)] {
            if dag_or_and(a, b, c).is_none() {
                hits += 1;
            }
        }
        assert_eq!(hits, 1);
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
