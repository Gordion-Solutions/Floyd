//! Class 1: observation collapse and ambiguity.
//!
//! `llvm-cov` branch counters are summed over every evaluation in a
//! test. A test that evaluates a decision several times with
//! different values must never be collapsed into one fabricated
//! vector: Floyd derives its vectors only when the counters
//! determine them (here: exactly one condition took both values),
//! and otherwise records the test as ambiguous (no condition vector).
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

/// `a && b`, evaluated three times by `ambiguous_and_loop`, (T,T),
/// (T,F) and (F,-), and once by `ambiguous_and_once_tf`. In the loop
/// both conditions took both values, so its counters (a = [2 T,
/// 1 F], b = [1 T, 1 F]) do not determine its vectors.
///
/// Expected (`--only=ambiguous_and`): verdict `analysed`, two
/// observations, one ambiguous (`ambiguous_and_loop`,
/// `evaluations: 3`, empty `inputs`, `result: null`) and one vector
/// (`a=T, b=F -> F`); both conditions `UNKNOWN`, so
/// `summary.unknown == 2`, `summary.percentage == null`, text output
/// says "no percentage", exit code 2.
pub fn ambiguous_and(a: bool, b: bool) -> bool {
    a && b
}

/// `a && (b || c)`, evaluated three times by
/// `ruled_out_and_or_loop`: (T,T,-) -> T, (T,F,F) -> F and
/// (F,-,-) -> F, so a = [2 T, 1 F], b = [1 T, 1 F], c = [0 T, 1 F].
/// Ambiguous (two two-sided conditions), but `c`'s only pair needs
/// (T,F,T) -> T and no test ever made `c` true.
///
/// Expected (`--only=ruled_out_and_or`): one ambiguous observation;
/// `a` and `b` `UNKNOWN`, `c` `UNEXERCISED` with
/// `analysis.ruled_out.c` naming `ruled_out_and_or_loop`'s zero
/// `c = T` counter; exit code 2. (`a && b && c` would read more
/// simply, but rustc's instrumented MIR for a three-operand chain is
/// a shape the decomposer declines.)
pub fn ruled_out_and_or(a: bool, b: bool, c: bool) -> bool {
    a && (b || c)
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
    fn ambiguous_and_loop() {
        let mut hits = 0;
        for (a, b) in [(true, true), (true, false), (false, true)] {
            if ambiguous_and(a, b) {
                hits += 1;
            }
        }
        assert_eq!(hits, 1);
    }

    #[test]
    fn ambiguous_and_once_tf() {
        assert!(!ambiguous_and(true, false));
    }

    #[test]
    fn ruled_out_and_or_loop() {
        let mut hits = 0;
        for (a, b, c) in [
            (true, true, true),
            (true, false, false),
            (false, true, true),
        ] {
            if ruled_out_and_or(a, b, c) {
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
