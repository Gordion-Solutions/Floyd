//! Class 1: observation collapse and ambiguity.
//!
//! `llvm-cov` branch counters are summed over every evaluation in a
//! test. A test that evaluates a decision several times with
//! different values must never be collapsed into one fabricated
//! vector: Floyd derives its vectors only when the counters
//! determine them (here: the decision is a tree, whose counters are
//! its edge flows, or exactly one condition took both values), and
//! otherwise records the test as ambiguous (no condition vector).
//! An ambiguous test leaves a condition unknown only if it took the
//! outcomes a missing independence-pair row needs.

/// `a && b`, evaluated twice by `collapse_and_loop` and once by
/// `collapse_and_once_tf`.
///
/// `collapse_and_loop` evaluates (T,T) -> T then (F,-) -> F, so its
/// counters are a = [1 T, 1 F], b = [1 T, 0 F]. `a && b` is a tree,
/// so the counters are its edge flows and determine both vectors.
///
/// Expected (`--only=collapse_and`): verdict `analysed`, three
/// vector observations, none ambiguous: `collapse_and_loop`'s
/// (`a=T, b=T -> T` and `a=F -> F`, each `evaluations: 1` and
/// `resolved_by: "tree_counters"`) and
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
/// so the test is ambiguous: no vector is credited. `a`'s and `b`'s
/// pairs both need (F,F,-) -> F, and no test ever made `b` false.
/// Each of the two decompositions completes a (different) masking
/// pair for `c`: (T,-,T) / (F,T,F) or (T,-,F) / (F,T,T).
///
/// Expected (`--only=dag_or_and`): one ambiguous observation
/// (`evaluations: 2`, empty `inputs`, `result: null`, the raw
/// `counts`, both `decompositions`); `c` `ExercisedUnidentified`
/// (two `pair_candidates`, `pair_identifiable: false`); `a` and `b`
/// `UNEXERCISED`, with `analysis.ruled_out.b` naming
/// `dag_or_and_loop`'s zero `b = F` counter; `summary.unknown == 0`,
/// percentage 33, exit code 2.
pub fn dag_or_and(a: bool, b: bool, c: bool) -> Option<u8> {
    if (a || b) && c {
        return None;
    }
    Some(1)
}

/// `(a || b) && c`, the shape of `dag_or_and`, evaluated three
/// times by `dag_one_two_sided_loop`: (T,-,T) -> T twice and
/// (F,T,T) -> T once, so a = [2 T, 1 F], b = [1 T, 0 F],
/// c = [3 T, 0 F]. Not a tree, but only `a` took both values, so
/// each evaluation's walk is fixed by `a`'s value and the counters
/// determine both vectors.
///
/// Expected (`--only=dag_one_two_sided`): two vector observations,
/// none ambiguous: `a=T, c=T -> T` and `a=F, b=T, c=T -> T`, each
/// `evaluations: 3` and `resolved_by: "single_two_sided"`. No test
/// made the decision false, so no condition is exercised.
pub fn dag_one_two_sided(a: bool, b: bool, c: bool) -> Option<u8> {
    if (a || b) && c {
        return None;
    }
    Some(1)
}

/// `(a || b) && c`, the shape of `dag_or_and`, evaluated three
/// times by `dag_zero_inflow_loop`: (T,-,T) -> T, (T,-,F) -> F and
/// (F,F,-) -> F, so a = [2 T, 1 F], b = [0 T, 1 F], c = [1 T, 1 F].
/// Not a tree and three conditions two-sided, so neither the tree
/// rule nor the single-two-sided rule applies. But `b` was never
/// true, so no evaluation took a walk through `b = T`: both of `c`'s
/// evaluations came from `a = T`, and the counters determine all
/// three vectors (flow decomposition). `dag_zero_inflow_ftt` adds
/// (F,T,T) -> T, which `b`'s pair needs.
///
/// Expected (`--only=dag_zero_inflow`): four vector observations,
/// none ambiguous; the loop's three each `evaluations: 1` and
/// `resolved_by: "flow_decomposition"`; every condition exercised,
/// `summary.percentage == 100`, exit 0.
pub fn dag_zero_inflow(a: bool, b: bool, c: bool) -> Option<u8> {
    if (a || b) && c {
        return None;
    }
    Some(1)
}

/// `(a || b) && c` evaluated three times by `dag_partial_flow_loop`:
/// (T,-,T) -> T, (F,T,F) -> F and (F,F,-) -> F, so a = [1 T, 2 F],
/// b = [1 T, 1 F], c = [1 T, 1 F]. (T,-,F) + (F,T,T) + (F,F,-)
/// leaves the same counters, so the counters determine only
/// (F,F,-) (the one walk that tests `b = F`, taken once in both
/// decompositions).
///
/// Expected (`--only=dag_partial_flow`): two observations of the
/// loop test, both `resolved_by: "flow_decomposition"`: the vector
/// `a=F, b=F -> F` (`evaluations: 1`) and an ambiguous remainder
/// (`evaluations: 2`, `attributed_evaluations: 1`, counts
/// a = [1, 1], b = [1, 0], c = [1, 1], two `decompositions`); `a`
/// and `b` `UNKNOWN` (one decomposition completes no pair for them),
/// `c` `ExercisedUnidentified`; `summary.percentage == null`, exit
/// code 2.
pub fn dag_partial_flow(a: bool, b: bool, c: bool) -> Option<u8> {
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
    fn dag_one_two_sided_loop() {
        let mut hits = 0;
        for (a, b, c) in [(true, false, true), (true, true, true), (false, true, true)] {
            if dag_one_two_sided(a, b, c).is_none() {
                hits += 1;
            }
        }
        assert_eq!(hits, 3);
    }

    #[test]
    fn dag_zero_inflow_loop() {
        let mut hits = 0;
        for (a, b, c) in [
            (true, false, true),
            (true, true, false),
            (false, false, true),
        ] {
            if dag_zero_inflow(a, b, c).is_none() {
                hits += 1;
            }
        }
        assert_eq!(hits, 1);
    }

    #[test]
    fn dag_zero_inflow_ftt() {
        assert!(dag_zero_inflow(false, true, true).is_none());
    }

    #[test]
    fn dag_partial_flow_loop() {
        let mut hits = 0;
        for (a, b, c) in [
            (true, false, true),
            (false, true, false),
            (false, false, true),
        ] {
            if dag_partial_flow(a, b, c).is_none() {
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
