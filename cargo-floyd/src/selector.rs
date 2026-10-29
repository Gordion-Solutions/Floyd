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

/// Pick the best candidate from a collection.
///
/// Scoring (lexicographic, descending):
///   1. observation count — a candidate exercised by many tests
///      produces a richer MC/DC verdict than one no test touches
///   2. condition count — richer tree beats a single-condition one
pub fn pick_best(candidates: Vec<Candidate>) -> Option<Candidate> {
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
        return None;
    }
    viable.sort_by(|a, b| {
        b.observations
            .len()
            .cmp(&a.observations.len())
            .then_with(|| count_conditions(&b.tree).cmp(&count_conditions(&a.tree)))
    });
    viable.into_iter().next()
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
        assert!(pick_best(vec![]).is_none());
    }

    #[test]
    fn pick_best_prefers_higher_observation_count() {
        let picked =
            pick_best(vec![candidate("lonely", 0, 5), candidate("busy", 3, 1)]).expect("non-empty");
        assert_eq!(picked.provenance.function_name.as_deref(), Some("busy"));
    }

    #[test]
    fn pick_best_tie_breaks_by_condition_count() {
        // Equal observation count — prefer the one with more conditions.
        let picked =
            pick_best(vec![candidate("simple", 2, 1), candidate("rich", 2, 4)]).expect("non-empty");
        assert_eq!(picked.provenance.function_name.as_deref(), Some("rich"));
    }

    #[test]
    fn pick_best_handles_single_candidate() {
        let picked = pick_best(vec![candidate("only", 0, 1)]).expect("non-empty");
        assert_eq!(picked.provenance.function_name.as_deref(), Some("only"));
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
        let picked = pick_best(vec![no_cond, real]).expect("non-empty");
        assert_eq!(picked.provenance.function_name.as_deref(), Some("real"));

        // All-zero-condition input must return None.
        let only_const = Candidate {
            tree: DecisionTree {
                decisions: vec![Node::Const { value: false }],
            },
            provenance: DecisionProvenance::default(),
            observations: vec![],
        };
        assert!(pick_best(vec![only_const]).is_none());
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
