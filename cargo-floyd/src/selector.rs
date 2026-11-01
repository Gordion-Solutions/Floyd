//! Smarter decision-picker (Phase 4 selector).
//!
//! Replaces the former "first non-empty decision tree wins" logic
//! with a scorer that prefers candidate functions actually exercised
//! by the test suite.
//!
//! A **candidate** is one decomposable function in a MIR, paired with
//! its provenance (name, file, line) and the observations that
//! fell out of correlating the chosen tree against each test's
//! coverage. The score is simply the number of observations; ties
//! are broken by condition count (richer analysis is preferred when
//! two candidates are equally observed).
//!
//! The previous "first wins" selector picked uninteresting functions
//! on real-world crates — auto-derived impls, methods on `self`,
//! closures none of the tests call. Rather than invent increasingly
//! baroque filters, this selector lets the test suite itself vote.

use floyd::decision::Node;
use floyd::masking::ConditionObservation;
use floyd::{DecisionProvenance, DecisionTree};
use std::collections::BTreeSet;

/// One per-function MC/DC candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The decomposed single-decision tree for this function.
    pub tree: DecisionTree,
    /// Source-location metadata for the function.
    pub provenance: DecisionProvenance,
    /// Per-test observations against this candidate's tree.
    pub observations: Vec<ConditionObservation>,
}

/// Outcome of [`pick_best`].
///
/// The three variants let the caller tell apart genuinely different
/// failure modes — "the engine recognises no decision in this code
/// at all" is a very different story from "the engine found
/// decisions but your tests never exercised them".
#[derive(Debug)]
pub enum PickResult {
    /// No decomposable candidate was found. Either the test
    /// binaries have no boolean decisions in the shapes this engine
    /// recognises, or every candidate had zero Condition leaves
    /// (its decision tree is Const / Const only).
    NoCandidates,
    /// Decomposable candidates were found, but none was exercised by
    /// any observed test. Floyd declines rather than report a
    /// spurious `0% MC/DC` verdict against a candidate the suite
    /// never called.
    AllUnobserved,
    /// A candidate was picked and should be reported.
    Picked(Candidate),
}

/// Pick the best candidate from a collection.
///
/// Scoring (lexicographic, descending):
///
/// 1. observation count — a candidate exercised by many tests
///    produces a richer MC/DC verdict than one no test touches.
/// 2. candidate tier — see [`tier`]. When no candidate has
///    observations (common on real crates whose decisions use
///    `match`/`if let`/`?` the engine does not yet decompose),
///    the tiebreak would otherwise pick an arbitrary closure or
///    test-internal function. The tier ordering surfaces a
///    user-written top-level `pub fn` over a `{closure#N}` body
///    over a `#[test]` function's internals.
/// 3. condition count — richer tree beats a single-condition one.
pub fn pick_best(candidates: Vec<Candidate>) -> PickResult {
    // A candidate with zero Condition leaves is not an MC/DC decision —
    // its tree is just Const / Const branches, so there is nothing to
    // report. Filter these out before scoring; otherwise the selector
    // would prefer a well-exercised `const true` function over a
    // real multi-condition decision that happens to be exercised less.
    let mut viable: Vec<Candidate> = candidates
        .into_iter()
        .filter(|c| count_conditions(&c.tree) > 0)
        .collect();
    if viable.is_empty() {
        return PickResult::NoCandidates;
    }
    viable.sort_by(|a, b| {
        b.observations
            .len()
            .cmp(&a.observations.len())
            .then_with(|| tier(&b.provenance).cmp(&tier(&a.provenance)))
            .then_with(|| count_conditions(&b.tree).cmp(&count_conditions(&a.tree)))
    });
    let top = viable.into_iter().next().expect("non-empty");
    // After the observation-descending sort, a top with zero
    // observations means EVERY candidate was unobserved. Decline so
    // the caller can emit the dedicated "found decisions, no test
    // called them" message rather than a misleading 0%-MC/DC
    // verdict against a function the test suite never ran through.
    if top.observations.is_empty() {
        return PickResult::AllUnobserved;
    }
    PickResult::Picked(top)
}

/// Candidate quality tier. Higher is better.
///
/// - `2` — top-level library function (user-visible API, the kind of
///   decision an engineer wants an MC/DC verdict for).
/// - `1` — closure body in library code. Still legitimate but is
///   less likely to be what a reader of the engine's output is
///   asking about, and the demangled closure symbol does not always
///   land in the coverage report cleanly.
/// - `0` — anything inside a test file or a `::tests::` module.
///   The engine should only pick these when nothing else is on
///   offer, because a test's internal closure getting flagged as
///   "the decision" of the crate is confusing.
fn tier(provenance: &DecisionProvenance) -> u8 {
    let name = provenance.function_name.as_deref().unwrap_or("");
    let file = provenance.source_file.as_deref().unwrap_or("");
    // Test-adjacent code: integration-test files live under
    // `tests/`, and `#[cfg(test)] mod tests { ... }` nests under
    // `::tests::` inside the lib.
    if file.contains("/tests/") || file.starts_with("tests/") || name.contains("::tests::") {
        return 0;
    }
    // Closure in library code.
    if name.contains("::{closure#") {
        return 1;
    }
    2
}

fn count_conditions(tree: &DecisionTree) -> usize {
    let mut set: BTreeSet<String> = BTreeSet::new();
    for node in &tree.decisions {
        walk_conditions(node, &mut set);
    }
    set.len()
}

fn walk_conditions(node: &Node, set: &mut BTreeSet<String>) {
    match node {
        Node::Condition { name } => {
            set.insert(name.clone());
        }
        Node::Const { .. } => {}
        Node::Ite {
            cond,
            then_branch,
            else_branch,
        } => {
            set.insert(cond.clone());
            walk_conditions(then_branch, set);
            walk_conditions(else_branch, set);
        }
        _ => {} // Node is #[non_exhaustive]; future variants are tolerated.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn unwrap_picked(r: PickResult) -> Candidate {
        match r {
            PickResult::Picked(c) => c,
            PickResult::NoCandidates => panic!("expected Picked, got NoCandidates"),
            PickResult::AllUnobserved => panic!("expected Picked, got AllUnobserved"),
        }
    }

    fn candidate(name: &str, n_obs: usize, n_conds: usize) -> Candidate {
        // Build a tree with n_conds unique Condition leaves (nested
        // Ites). Fill observations with placeholder entries so .len()
        // scoring works.
        let mut node = Node::Const { value: false };
        for i in 0..n_conds {
            node = Node::Ite {
                cond: format!("c{i}"),
                then_branch: Box::new(Node::Condition {
                    name: format!("c{i}"),
                }),
                else_branch: Box::new(node),
            };
        }
        Candidate {
            tree: DecisionTree {
                decisions: vec![node],
            },
            provenance: DecisionProvenance {
                function_name: Some(name.to_string()),
                source_file: None,
                line: None,
            },
            observations: (0..n_obs)
                .map(|i| ConditionObservation {
                    test_name: Some(format!("t{i}")),
                    inputs: BTreeMap::new(),
                    result: false,
                })
                .collect(),
        }
    }

    #[test]
    fn pick_best_returns_none_on_empty_input() {
        assert!(matches!(pick_best(vec![]), PickResult::NoCandidates));
    }

    #[test]
    fn pick_best_prefers_higher_observation_count() {
        let picked = unwrap_picked(pick_best(vec![
            candidate("lonely", 0, 5),
            candidate("busy", 3, 1),
        ]));
        assert_eq!(picked.provenance.function_name.as_deref(), Some("busy"));
    }

    #[test]
    fn pick_best_tie_breaks_by_condition_count() {
        // Equal observation count — prefer the one with more conditions.
        let picked = unwrap_picked(pick_best(vec![
            candidate("simple", 2, 1),
            candidate("rich", 2, 4),
        ]));
        assert_eq!(picked.provenance.function_name.as_deref(), Some("rich"));
    }

    #[test]
    fn pick_best_handles_single_candidate() {
        // With an observed single candidate, picker returns it.
        let picked = unwrap_picked(pick_best(vec![candidate("only", 1, 1)]));
        assert_eq!(picked.provenance.function_name.as_deref(), Some("only"));
    }

    #[test]
    fn pick_best_returns_all_unobserved_when_no_candidate_was_called() {
        // Decomposable candidates exist but no test exercised any of
        // them — Floyd declines so the caller can emit the
        // dedicated "found decisions, no test called them" message
        // rather than a misleading `0% MC/DC` verdict against a
        // function the suite never ran through.
        let picked = pick_best(vec![candidate("unobserved", 0, 1)]);
        assert!(matches!(picked, PickResult::AllUnobserved));
    }

    #[test]
    fn pick_best_rejects_candidates_with_zero_conditions() {
        // A tree whose only Node is a Const has no Condition leaves —
        // it isn't an MC/DC decision regardless of observation count.
        // The selector must skip it even if observed many times.
        let no_cond = Candidate {
            tree: DecisionTree {
                decisions: vec![Node::Const { value: true }],
            },
            provenance: DecisionProvenance {
                function_name: Some("const_fn".to_string()),
                source_file: None,
                line: None,
            },
            observations: (0..5)
                .map(|i| ConditionObservation {
                    test_name: Some(format!("t{i}")),
                    inputs: BTreeMap::new(),
                    result: true,
                })
                .collect(),
        };
        // real_decision has 1 condition, 1 observation — should still win.
        let real = candidate("real", 1, 1);
        let picked = unwrap_picked(pick_best(vec![no_cond, real]));
        assert_eq!(picked.provenance.function_name.as_deref(), Some("real"));

        // All-zero-condition input must return None.
        let only_const = Candidate {
            tree: DecisionTree {
                decisions: vec![Node::Const { value: false }],
            },
            provenance: DecisionProvenance::default(),
            observations: vec![],
        };
        assert!(matches!(
            pick_best(vec![only_const]),
            PickResult::NoCandidates
        ));
    }

    fn candidate_with_provenance(
        name: &str,
        n_obs: usize,
        n_conds: usize,
        source_file: Option<&str>,
    ) -> Candidate {
        let mut c = candidate(name, n_obs, n_conds);
        c.provenance.source_file = source_file.map(String::from);
        c
    }

    #[test]
    fn pick_best_prefers_pub_fn_over_closure_when_observations_tie() {
        // Both candidates have zero observations (the engine declined
        // every decomposable function the suite exercised), both have
        // the same condition count. Without the tier rule, the sort
        // is undefined and either could win. With it, the top-level
        // `pub fn` name beats the closure.
        let pub_fn = candidate_with_provenance("percent_decode", 1, 1, Some("src/lib.rs"));
        let closure = candidate_with_provenance(
            "<impl at src/lib.rs:289:1: 289:27>::if_any::{closure#0}",
            1,
            1,
            Some("src/lib.rs"),
        );
        let picked = unwrap_picked(pick_best(vec![closure, pub_fn]));
        assert_eq!(
            picked.provenance.function_name.as_deref(),
            Some("percent_decode")
        );
    }

    #[test]
    fn pick_best_prefers_lib_closure_over_test_file_candidate() {
        // When the only non-test candidate is a closure, the engine
        // still prefers it over a test file's own body — a test
        // function being surfaced as "the decision of the crate"
        // is more confusing than a lib closure.
        let test_fn = candidate_with_provenance(
            "it_handles_panic::{closure#1}",
            1,
            1,
            Some("tests/panic.rs"),
        );
        let lib_closure =
            candidate_with_provenance("<impl ...>::if_any::{closure#0}", 1, 1, Some("src/lib.rs"));
        let picked = unwrap_picked(pick_best(vec![test_fn, lib_closure]));
        assert_eq!(
            picked.provenance.function_name.as_deref(),
            Some("<impl ...>::if_any::{closure#0}")
        );
    }

    #[test]
    fn pick_best_observation_count_still_wins_over_tier() {
        // Tier is a tiebreak, not a primary signal. A well-observed
        // test-file candidate beats an unobserved pub lib fn —
        // observations are the real evidence of what the test suite
        // is actually exercising.
        let observed_test = candidate_with_provenance("test_internal", 5, 1, Some("tests/t.rs"));
        let unobserved_lib = candidate_with_provenance("lib_fn", 0, 1, Some("src/lib.rs"));
        let picked = unwrap_picked(pick_best(vec![observed_test, unobserved_lib]));
        assert_eq!(
            picked.provenance.function_name.as_deref(),
            Some("test_internal")
        );
    }

    #[test]
    fn pick_best_detects_cfg_test_mod_tests_by_name() {
        // `#[cfg(test)] mod tests { ... }` nests under `::tests::`
        // inside the lib's own path. The classifier should spot it
        // regardless of the source file (which is still `src/lib.rs`).
        let in_lib_tests_mod = candidate_with_provenance(
            "my_crate::tests::sanity::{closure#0}",
            1,
            1,
            Some("src/lib.rs"),
        );
        let regular_lib_fn = candidate_with_provenance("my_crate::parse", 1, 1, Some("src/lib.rs"));
        let picked = unwrap_picked(pick_best(vec![in_lib_tests_mod, regular_lib_fn]));
        assert_eq!(
            picked.provenance.function_name.as_deref(),
            Some("my_crate::parse")
        );
    }

    #[test]
    fn count_conditions_counts_unique_names() {
        // Build  Ite { a, Ite { b, c, c }, b }  — unique names {a, b, c}.
        let tree = DecisionTree {
            decisions: vec![Node::Ite {
                cond: "a".to_string(),
                then_branch: Box::new(Node::Ite {
                    cond: "b".to_string(),
                    then_branch: Box::new(Node::Condition {
                        name: "c".to_string(),
                    }),
                    else_branch: Box::new(Node::Condition {
                        name: "c".to_string(),
                    }),
                }),
                else_branch: Box::new(Node::Condition {
                    name: "b".to_string(),
                }),
            }],
        };
        assert_eq!(count_conditions(&tree), 3);
    }
}
