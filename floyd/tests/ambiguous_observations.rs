//! Engine-level regression test for per-test observations derived
//! from aggregate coverage counters (FINDINGS 2026-11-01).
//!
//! `decide(a, b) = a && b` with two tests:
//!
//! - `loop_tt_then_f` evaluates the decision twice, (T,T) -> T then
//!   (F,-) -> F, so its counters are a = [1 T, 1 F], b = [1 T, 0 F].
//! - `once_tf` evaluates it once, (T,F) -> F.
//!
//! The old correlation collapsed the loop test to the single vector
//! (T,T) and reported `a` UNEXERCISED (50% MC/DC). The counters of a
//! twice-evaluated two-condition decision do not identify which
//! vectors occurred, so the loop test must be recorded as ambiguous,
//! excluded from pair finding, and `a` reported as unknown
//! (ambiguous) rather than unexercised.

use floyd::correlate;
use floyd::decision;
use floyd::masking::{self, ConditionStatus, Variant};
use floyd::{mir, profile};

/// Span-annotated MIR for `fn decide(a, b) -> bool { a && b }`.
const AND_MIR: &str = r#"
fn decide(_1: bool, _2: bool) -> bool {
    debug a => _1;                       // in scope 0 at src/lib.rs:1:15: 1:16
    debug b => _2;                       // in scope 0 at src/lib.rs:1:24: 1:25
    let mut _0: bool;                    // return place in scope 0 at src/lib.rs:1:36: 1:40

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at src/lib.rs:2:5: 2:6
    }

    bb1: {
        _0 = copy _2;                    // scope 0 at src/lib.rs:2:10: 2:11
        goto -> bb3;                     // scope 0 at src/lib.rs:2:5: 2:11
    }

    bb2: {
        _0 = const false;                // scope 0 at src/lib.rs:2:5: 2:11
        goto -> bb3;                     // scope 0 at src/lib.rs:2:5: 2:11
    }

    bb3: {
        return;                          // scope 0 at src/lib.rs:3:2: 3:2
    }
}
"#;

/// One test's `llvm-cov export` for `decide`, with the function's
/// entry count and the (true, false) counters of `a` and `b`.
fn coverage(count: u64, a: (u64, u64), b: (u64, u64)) -> profile::CoverageReport {
    let json = format!(
        r#"{{
          "data": [{{
            "files": [],
            "functions": [{{
              "name": "_RNvCsb6BSQ92hH2b_19floyd_runtime_scout6decide",
              "count": {count},
              "filenames": ["/abs/src/lib.rs"],
              "regions": [[1, 1, 1, 40, {count}, 0, 0, 0]],
              "branches": [
                [2, 5, 2, 6, {at}, {af}, 0, 0, 4],
                [2, 10, 2, 11, {bt}, {bf}, 0, 0, 4]
              ],
              "mcdc_records": []
            }}],
            "totals": {{}}
          }}],
          "type": "llvm.coverage.json.export",
          "version": "2.0.1"
        }}"#,
        at = a.0,
        af = a.1,
        bt = b.0,
        bf = b.1,
    );
    profile::parse(&json).expect("coverage parses")
}

#[test]
fn loop_test_is_ambiguous_and_a_is_unknown_not_unexercised() {
    let mir = mir::parse_text(AND_MIR).expect("MIR parses");
    let tree = decision::decompose(&mir);
    assert_eq!(tree.decisions.len(), 1);

    let tests = [
        ("loop_tt_then_f", coverage(2, (1, 1), (1, 0))),
        ("once_tf", coverage(1, (1, 0), (0, 1))),
    ];
    // The cargo-mode Pass 3 path.
    let prepared = correlate::prepare_mir(&mir);
    let observations: Vec<_> = tests
        .iter()
        .flat_map(|(name, cov)| {
            let pcov = correlate::prepare_coverage_lookup(cov);
            correlate::observations_from_prepared_coverage(
                Some(name.to_string()),
                &prepared,
                0,
                &pcov,
                &tree,
            )
        })
        .collect();

    assert_eq!(observations.len(), 2);
    let looped = &observations[0];
    assert_eq!(looped.test_name.as_deref(), Some("loop_tt_then_f"));
    assert!(looped.ambiguous);
    assert!(looped.inputs.is_empty());
    assert_eq!(looped.result, None);
    assert_eq!(looped.evaluations, Some(2));
    assert_eq!(looped.counts["a"].true_count, 1);
    assert_eq!(looped.counts["a"].false_count, 1);
    assert_eq!(looped.counts["b"].true_count, 1);
    assert_eq!(looped.counts["b"].false_count, 0);

    let once = &observations[1];
    assert!(!once.ambiguous);
    assert_eq!(once.evaluations, Some(1));
    assert_eq!(once.inputs.get("a"), Some(&true));
    assert_eq!(once.inputs.get("b"), Some(&false));
    assert_eq!(once.result, Some(false));

    for variant in [Variant::Masking, Variant::UniqueCause] {
        let analysis = masking::analyze_with_runtime(&tree, &observations, variant);
        assert_eq!(analysis.ambiguous_observations, 1);
        for c in ["a", "b"] {
            match &analysis.condition_status[c] {
                ConditionStatus::Ambiguous { tests } => {
                    assert_eq!(tests, &vec!["loop_tt_then_f".to_string()]);
                }
                other => panic!("{variant:?}: {c} should be unknown (ambiguous), got {other:?}"),
            }
        }
    }
}

#[test]
fn single_evaluation_tests_still_demonstrate_full_mcdc() {
    // Splitting the loop test into one-evaluation tests restores the
    // evidence: (T,T), (F,-), (T,F) -> 100% masking MC/DC.
    let mir = mir::parse_text(AND_MIR).expect("MIR parses");
    let tree = decision::decompose(&mir);
    let prepared = correlate::prepare_mir(&mir);
    let tests = [
        ("tt", coverage(1, (1, 0), (1, 0))),
        ("f", coverage(1, (0, 1), (0, 0))),
        ("tf", coverage(1, (1, 0), (0, 1))),
    ];
    let observations: Vec<_> = tests
        .iter()
        .flat_map(|(name, cov)| {
            let pcov = correlate::prepare_coverage_lookup(cov);
            correlate::observations_from_prepared_coverage(
                Some(name.to_string()),
                &prepared,
                0,
                &pcov,
                &tree,
            )
        })
        .collect();
    assert_eq!(observations.len(), 3);
    assert!(observations.iter().all(|o| !o.ambiguous));
    let analysis = masking::analyze_with_runtime(&tree, &observations, Variant::Masking);
    assert_eq!(analysis.ambiguous_observations, 0);
    for c in ["a", "b"] {
        assert!(
            matches!(analysis.condition_status[c], ConditionStatus::Exercised(_)),
            "{c}: {:?}",
            analysis.condition_status[c]
        );
    }
}

#[test]
fn loop_inside_a_function_called_once_is_still_multi_evaluation() {
    // The decision runs twice inside ONE call (a loop in the body):
    // the function entry count is 1, but the root condition `a` was
    // tested twice. The evaluation count must come from the root,
    // so the test is ambiguous rather than a single (T,T) vector.
    let mir = mir::parse_text(AND_MIR).expect("MIR parses");
    let tree = decision::decompose(&mir);
    let prepared = correlate::prepare_mir(&mir);
    let cov = coverage(1, (1, 1), (1, 0));
    let pcov = correlate::prepare_coverage_lookup(&cov);
    let observations = correlate::observations_from_prepared_coverage(
        Some("loop_in_one_call".to_string()),
        &prepared,
        0,
        &pcov,
        &tree,
    );
    assert_eq!(observations.len(), 1);
    assert!(observations[0].ambiguous);
    assert_eq!(observations[0].evaluations, Some(2));
    assert!(observations[0].inputs.is_empty());
}
