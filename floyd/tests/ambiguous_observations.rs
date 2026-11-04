//! Engine-level regression test for per-test observations derived
//! from aggregate coverage counters (FINDINGS 2026-11-01).
//!
//! `decide(a, b) = a && b` with two tests:
//!
//! - `loop_three` evaluates the decision three times, (T,T) -> T,
//!   (T,F) -> F and (F,-) -> F, so its counters are a = [2 T, 1 F],
//!   b = [1 T, 1 F].
//! - `once_tf` evaluates it once, (T,F) -> F.
//!
//! The old correlation collapsed a loop test to a single vector and
//! reported `a` UNEXERCISED. `a && b` is a tree (each condition at
//! one node), so the counters are edge flows and determine every
//! vector: a's true count is b's inflow, and the leaves carry
//! (T,T) x1, (T,F) x1, (F,-) x1 (rule 5 of
//! `correlate::observations_from_counts`).
//!
//! `loop_tt_then_f` ((T,T) then (F,-): a = [1 T, 1 F], b = [1 T,
//! 0 F]) has only one two-sided condition, so rule 4 already
//! determines its two vectors.
//!
//! `(a || b) && c` is not a tree: `c` is reached from `a = T` and
//! from `a = F, b = T`. A loop test over it whose counters do not
//! determine its vectors must be recorded as ambiguous, excluded
//! from pair finding, and its conditions reported unknown rather
//! than unexercised.

use floyd::correlate;
use floyd::coverage_index::{CoverageIndex, PathNormaliser};
use floyd::decision;
use floyd::masking::{self, ConditionStatus, Resolution, Variant};
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

/// Span-annotated MIR for `fn decide(a, b, c) -> bool { (a || b) && c }`:
/// `bb2` (`c`) has two predecessors.
const OR_AND_MIR: &str = r#"
fn decide(_1: bool, _2: bool, _3: bool) -> bool {
    debug a => _1;                       // in scope 0 at src/lib.rs:1:15: 1:16
    debug b => _2;                       // in scope 0 at src/lib.rs:1:24: 1:25
    debug c => _3;                       // in scope 0 at src/lib.rs:1:33: 1:34
    let mut _0: bool;                    // return place in scope 0 at src/lib.rs:1:45: 1:49

    bb0: {
        switchInt(copy _1) -> [0: bb1, otherwise: bb2]; // scope 0 at src/lib.rs:2:6: 2:7
    }

    bb1: {
        switchInt(copy _2) -> [0: bb3, otherwise: bb2]; // scope 0 at src/lib.rs:2:11: 2:12
    }

    bb2: {
        _0 = copy _3;                    // scope 0 at src/lib.rs:2:17: 2:18
        goto -> bb4;                     // scope 0 at src/lib.rs:2:5: 2:18
    }

    bb3: {
        _0 = const false;                // scope 0 at src/lib.rs:2:5: 2:18
        goto -> bb4;                     // scope 0 at src/lib.rs:2:5: 2:18
    }

    bb4: {
        return;                          // scope 0 at src/lib.rs:3:2: 3:2
    }
}
"#;

/// MIR prints `src/lib.rs` (relative to the directory rustc ran
/// in); llvm-cov reports `/abs/src/lib.rs`.
fn paths() -> PathNormaliser {
    PathNormaliser::new(Some(std::path::Path::new("/abs")))
}

/// One test's `llvm-cov export` for `decide`, with the function's
/// entry count and the (true, false) counters of `a` and `b`.
fn coverage(count: u64, a: (u64, u64), b: (u64, u64)) -> profile::CoverageReport {
    coverage_at(count, &[((2, 5, 2, 6), a), ((2, 10, 2, 11), b)])
}

/// [`coverage`] for [`OR_AND_MIR`]: counters of `a`, `b`, `c`.
fn or_and_coverage(
    count: u64,
    a: (u64, u64),
    b: (u64, u64),
    c: (u64, u64),
) -> profile::CoverageReport {
    coverage_at(
        count,
        &[((2, 6, 2, 7), a), ((2, 11, 2, 12), b), ((2, 17, 2, 18), c)],
    )
}

/// A branch region: (line, col, end line, end col) and (true, false).
type Branch = ((u32, u32, u32, u32), (u64, u64));

/// A one-function export whose branch regions are `branches`.
fn coverage_at(count: u64, branches: &[Branch]) -> profile::CoverageReport {
    let branches: Vec<String> = branches
        .iter()
        .map(|((l, c, el, ec), (t, f))| format!("[{l}, {c}, {el}, {ec}, {t}, {f}, 0, 0, 4]"))
        .collect();
    let json = format!(
        r#"{{
          "data": [{{
            "files": [],
            "functions": [{{
              "name": "_RNvCsb6BSQ92hH2b_19floyd_runtime_scout6decide",
              "count": {count},
              "filenames": ["/abs/src/lib.rs"],
              "regions": [[1, 1, 1, 40, {count}, 0, 0, 0]],
              "branches": [{branches}],
              "mcdc_records": []
            }}],
            "totals": {{}}
          }}],
          "type": "llvm.coverage.json.export",
          "version": "2.0.1"
        }}"#,
        branches = branches.join(", "),
    );
    profile::parse(&json).expect("coverage parses")
}

/// Observations of `tests` through the cargo-mode Pass 3 path.
fn observe(
    mir: &mir::Mir,
    tree: &floyd::DecisionTree,
    tests: &[(&str, profile::CoverageReport)],
) -> Vec<floyd::ConditionObservation> {
    let prepared = correlate::prepare_mir(mir, &paths());
    tests
        .iter()
        .flat_map(|(name, cov)| {
            let index = CoverageIndex::build(cov, &paths());
            correlate::observations_from_prepared_coverage(
                Some(name.to_string()),
                &prepared,
                0,
                &index,
                tree,
            )
        })
        .collect()
}

#[test]
fn loop_test_with_both_conditions_two_sided_is_resolved_by_tree_counters() {
    let mir = mir::parse_text(AND_MIR).expect("MIR parses");
    let tree = decision::decompose(&mir);
    assert_eq!(tree.decisions.len(), 1);

    let tests = [
        ("loop_three", coverage(3, (2, 1), (1, 1))),
        ("once_tf", coverage(1, (1, 0), (0, 1))),
    ];
    let observations = observe(&mir, &tree, &tests);

    assert_eq!(observations.len(), 4, "{observations:?}");
    assert!(observations.iter().all(|o| !o.ambiguous));
    let looped: Vec<_> = observations[..3]
        .iter()
        .map(|o| {
            assert_eq!(o.test_name.as_deref(), Some("loop_three"));
            assert_eq!(o.evaluations, Some(1));
            assert_eq!(o.resolved_by, Some(Resolution::TreeCounters));
            (
                o.inputs.get("a").copied(),
                o.inputs.get("b").copied(),
                o.result,
            )
        })
        .collect();
    assert_eq!(
        looped,
        [
            (Some(true), Some(true), Some(true)),
            (Some(true), Some(false), Some(false)),
            (Some(false), None, Some(false)),
        ]
    );

    let once = &observations[3];
    assert_eq!(once.evaluations, Some(1));
    assert_eq!(once.resolved_by, None);
    assert_eq!(once.inputs.get("a"), Some(&true));
    assert_eq!(once.inputs.get("b"), Some(&false));
    assert_eq!(once.result, Some(false));

    for variant in [Variant::Masking, Variant::UniqueCause] {
        let analysis = masking::analyze_with_runtime(&tree, &observations, variant);
        assert_eq!(analysis.ambiguous_observations, 0);
        for c in ["a", "b"] {
            assert!(
                matches!(analysis.condition_status[c], ConditionStatus::Exercised(_)),
                "{variant:?} {c}: {:?}",
                analysis.condition_status[c]
            );
        }
    }
}

#[test]
fn dag_loop_test_is_ambiguous_and_its_conditions_are_unknown_not_unexercised() {
    let mir = mir::parse_text(OR_AND_MIR).expect("MIR parses");
    let tree = decision::decompose(&mir);
    assert_eq!(tree.decisions.len(), 1);
    assert_eq!(
        tree.decisions[0],
        decision::Node::Ite {
            cond: "a".into(),
            then_branch: Box::new(decision::Node::Condition { name: "c".into() }),
            else_branch: Box::new(decision::Node::Ite {
                cond: "b".into(),
                then_branch: Box::new(decision::Node::Condition { name: "c".into() }),
                else_branch: Box::new(decision::Node::Const { value: false }),
            }),
        },
        "c is unfolded under both of its predecessors"
    );

    // (T,-,T), (F,T,F), (F,F,-), or (T,-,F), (F,T,T), (F,F,-): the
    // same counters, a = [1 T, 2 F], b = [1 T, 1 F], c = [1 T, 1 F].
    let tests = [("loop_three", or_and_coverage(3, (1, 2), (1, 1), (1, 1)))];
    let observations = observe(&mir, &tree, &tests);
    assert_eq!(observations.len(), 1);
    let looped = &observations[0];
    assert!(looped.ambiguous);
    assert!(looped.inputs.is_empty());
    assert_eq!(looped.result, None);
    assert_eq!(looped.resolved_by, None);
    assert_eq!(looped.evaluations, Some(3));
    assert_eq!(looped.counts["c"].true_count, 1);
    assert_eq!(looped.counts["c"].false_count, 1);

    for variant in [Variant::Masking, Variant::UniqueCause] {
        let analysis = masking::analyze_with_runtime(&tree, &observations, variant);
        assert_eq!(analysis.ambiguous_observations, 1);
        for c in ["a", "b", "c"] {
            match &analysis.condition_status[c] {
                ConditionStatus::Ambiguous { tests } => {
                    assert_eq!(tests, &vec!["loop_three".to_string()]);
                }
                other => panic!("{variant:?}: {c} should be unknown (ambiguous), got {other:?}"),
            }
        }
    }
}

#[test]
fn loop_with_one_two_sided_condition_is_resolved_and_demonstrates_mcdc() {
    // (T,T) then (F,-) in one test, (T,F) in another: the counters of
    // the loop test determine both its vectors, so all three rows
    // are observed and both conditions are exercised.
    let mir = mir::parse_text(AND_MIR).expect("MIR parses");
    let tree = decision::decompose(&mir);
    let tests = [
        ("loop_tt_then_f", coverage(2, (1, 1), (1, 0))),
        ("once_tf", coverage(1, (1, 0), (0, 1))),
    ];
    let observations = observe(&mir, &tree, &tests);
    assert_eq!(observations.len(), 3);
    assert!(observations.iter().all(|o| !o.ambiguous));
    assert_eq!(observations[0].evaluations, Some(2));
    assert_eq!(observations[0].result, Some(true));
    assert_eq!(observations[1].inputs.get("b"), None);
    assert_eq!(observations[1].result, Some(false));
    for variant in [Variant::Masking, Variant::UniqueCause] {
        let analysis = masking::analyze_with_runtime(&tree, &observations, variant);
        assert_eq!(analysis.ambiguous_observations, 0);
        for c in ["a", "b"] {
            assert!(
                matches!(analysis.condition_status[c], ConditionStatus::Exercised(_)),
                "{variant:?} {c}: {:?}",
                analysis.condition_status[c]
            );
        }
    }
}

#[test]
fn single_evaluation_tests_still_demonstrate_full_mcdc() {
    // Splitting the loop test into one-evaluation tests restores the
    // evidence: (T,T), (F,-), (T,F) -> 100% masking MC/DC.
    let mir = mir::parse_text(AND_MIR).expect("MIR parses");
    let tree = decision::decompose(&mir);
    let tests = [
        ("tt", coverage(1, (1, 0), (1, 0))),
        ("f", coverage(1, (0, 1), (0, 0))),
        ("tf", coverage(1, (1, 0), (0, 1))),
    ];
    let observations = observe(&mir, &tree, &tests);
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
    // The decision runs inside ONE call (a loop in the body): the
    // function entry count is 1, but the root condition `a` was
    // tested several times. The evaluation count must come from the
    // root, so the test is not read as a single vector.
    let mir = mir::parse_text(AND_MIR).expect("MIR parses");
    let tree = decision::decompose(&mir);

    // Two evaluations, one two-sided condition: both vectors, N = 2.
    let observations = observe(&mir, &tree, &[("twice", coverage(1, (1, 1), (1, 0)))]);
    assert_eq!(observations.len(), 2);
    assert!(observations
        .iter()
        .all(|o| !o.ambiguous && o.evaluations == Some(2)));

    // Three evaluations, two two-sided conditions: three vectors
    // from the tree's edge flows, one evaluation each.
    let observations = observe(&mir, &tree, &[("thrice", coverage(1, (2, 1), (1, 1)))]);
    assert_eq!(observations.len(), 3);
    assert!(observations
        .iter()
        .all(|o| !o.ambiguous && o.evaluations == Some(1)));
}
