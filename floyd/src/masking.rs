//! Masking analysis.
//!
//! Consumes a [`DecisionTree`] from the decomposer stage and produces
//! the [`IndependenceMatrix`] that the report stage renders.
//!
//! Floyd supports two MC/DC variants, selectable per analysis run:
//!
//! - **masking** (default) — the CAST-10 variant used by most modern
//!   qualified MC/DC tools. A condition's independence pair may have
//!   *other* conditions at differing values, as long as those other
//!   conditions are *masked* in both rows of the pair (i.e. flipping
//!   them in those rows would not change the result).
//! - **unique-cause** — strict variant where all other conditions
//!   must be held literally constant between the two members of an
//!   independence pair.
//!
//! For most simple decisions the two variants produce the same pairs;
//! they diverge on expressions where masking creates additional
//! valid pairings.

use crate::decision::Node;
use crate::DecisionTree;
use std::collections::{BTreeMap, BTreeSet};

/// Which MC/DC variant to compute.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Variant {
    /// Masking MC/DC (CAST-10 default).
    #[default]
    Masking,
    /// Strict unique-cause MC/DC.
    UniqueCause,
}

/// One row of an enumerated truth table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TruthTableRow {
    /// Assignment of each condition to its boolean value.
    pub inputs: BTreeMap<String, bool>,
    /// Resulting decision outcome under this assignment.
    pub result: bool,
}

/// A pair of truth-table rows that together demonstrate that a single
/// condition independently affects the decision outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndependencePair {
    /// First test case.
    pub test_1: TruthTableRow,
    /// Second test case (differs in the named condition; outcome differs).
    pub test_2: TruthTableRow,
}

/// Output of [`analyze`].
///
/// Carries the truth table and per-condition independence pairs for
/// one decision. Phase 0 analyses one decision per call; multi-decision
/// support arrives with multi-decision corpus patterns.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndependenceMatrix {
    /// Which variant the analysis was computed under.
    pub variant: Variant,
    /// Atomic conditions referenced in the decision, sorted by name.
    pub conditions: Vec<String>,
    /// Full truth table (`2^n` rows for `n` conditions).
    pub truth_table: Vec<TruthTableRow>,
    /// Map from condition name to all valid independence pairs that
    /// demonstrate its MC/DC independence under [`Self::variant`].
    pub independence_pairs: BTreeMap<String, Vec<IndependencePair>>,
}

/// Compute the MC/DC independence matrix for a decision tree, under
/// the default [`Variant::Masking`] variant.
pub fn analyze(tree: &DecisionTree) -> IndependenceMatrix {
    analyze_with_variant(tree, Variant::Masking)
}

/// One condition assignment observed in a single test execution,
/// or the record that a test's evidence could not be reduced to one.
///
/// Built by upstream stages (the `runner` per ADR-0002 produces one
/// per test invocation). Floyd treats observations as opaque inputs
/// to [`analyze_with_runtime`]; how they are obtained — per-test
/// `profraw` parsing, instrumented harness, manually constructed for
/// integration tests — is the caller's responsibility.
///
/// An observation is either a **condition vector** (`ambiguous ==
/// false`: `inputs` and `result` describe evaluations that really
/// occurred) or **ambiguous** (`ambiguous == true`: the test
/// evaluated the decision several times with differing condition
/// values, so per-condition aggregate counters cannot say which
/// vectors occurred). Ambiguous observations carry no `inputs` and
/// no `result`, only the raw [`Self::counts`]; [`analyze_with_runtime`]
/// never uses them to demonstrate an independence pair. See
/// [`crate::correlate::observations_from_counts`] for the rule that
/// decides between the two.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConditionObservation {
    /// Optional test identifier, preserved for diagnostic output.
    pub test_name: Option<String>,
    /// Values observed for each named condition.
    ///
    /// Short-circuited conditions are *omitted* — only conditions
    /// actually evaluated in this test appear. Callers that wish to
    /// represent unevaluated conditions explicitly should leave them
    /// out of this map rather than inventing a value. Empty for an
    /// ambiguous observation.
    pub inputs: BTreeMap<String, bool>,
    /// The decision outcome under these inputs. `None` for an
    /// ambiguous observation, whose outcome is not attributable to
    /// any one condition vector.
    pub result: Option<bool>,
    /// True when the test's coverage counters could not be reduced
    /// to condition vectors known to have occurred. Excluded from
    /// pair finding.
    #[serde(default)]
    pub ambiguous: bool,
    /// How many times the test evaluated the decision, when known
    /// (runtime-derived observations always set it; hand-built ones
    /// may leave it `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluations: Option<u64>,
    /// Raw per-condition counters of an ambiguous observation, keyed
    /// by condition name. Empty for a condition vector.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counts: BTreeMap<String, BranchCounts>,
}

impl ConditionObservation {
    /// A condition vector that occurred: `inputs` evaluated to
    /// `result`.
    pub fn vector(test_name: Option<String>, inputs: BTreeMap<String, bool>, result: bool) -> Self {
        Self {
            test_name,
            inputs,
            result: Some(result),
            ambiguous: false,
            evaluations: None,
            counts: BTreeMap::new(),
        }
    }

    /// A test whose aggregate counters do not identify the condition
    /// vectors it evaluated. Keeps the raw counters for reporting.
    pub fn ambiguous(
        test_name: Option<String>,
        evaluations: u64,
        counts: BTreeMap<String, BranchCounts>,
    ) -> Self {
        Self {
            test_name,
            inputs: BTreeMap::new(),
            result: None,
            ambiguous: true,
            evaluations: Some(evaluations),
            counts,
        }
    }
}

/// Aggregate runtime counters for one condition within one test:
/// how many of its evaluations were true and how many false.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BranchCounts {
    /// Evaluations of the condition that were true.
    pub true_count: u64,
    /// Evaluations of the condition that were false.
    pub false_count: u64,
}

impl BranchCounts {
    /// Total number of evaluations of the condition.
    pub fn total(&self) -> u64 {
        self.true_count.saturating_add(self.false_count)
    }
}

/// Whether the test suite as observed demonstrates MC/DC independence
/// for one condition.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ConditionStatus {
    /// At least one valid independence pair had both required input
    /// assignments observed. MC/DC for this condition is demonstrated.
    Exercised(IndependencePair),
    /// No valid independence pair for this condition had both
    /// required input assignments among the observed tests. Caller
    /// should ask the user to add tests with the specific inputs
    /// from one of the unexercised pairs in
    /// [`IndependenceMatrix::independence_pairs`].
    Unexercised,
    /// Not demonstrated by any condition-vector observation, but
    /// some independence pair could have been completed by ambiguous
    /// observations (tests that evaluated the decision more than once
    /// with differing values): every row of the pair that no vector
    /// covers is *possible* in some ambiguous test, i.e. that test's
    /// counters took every outcome the row's evaluation path needs.
    /// Whether the test really evaluated that row is unknown from
    /// aggregate counters, so the condition is neither credited nor
    /// reported as unexercised. A row whose path needs an outcome
    /// that a test's counter shows was never taken cannot have
    /// occurred in that test; if that rules out every ambiguous test
    /// for every pair, the condition is [`Self::Unexercised`].
    Ambiguous {
        /// Names of the ambiguous tests that could hold a missing row
        /// of a completable pair.
        tests: Vec<String>,
    },
}

/// Where a decision came from in source.
///
/// Attached optionally to [`RuntimeAnalysis`] so downstream reports
/// (HTML, SARIF, LCOV, qualification kits) can cite the function
/// name, source file, and line number a decision lives at — without
/// reconstructing it from the surrounding build context.
///
/// Every field is optional. Callers populate what they know; the
/// engine itself populates nothing (provenance is a reporting
/// concern, not an analysis concern).
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DecisionProvenance {
    /// Source file path, as rustc reports it (usually relative to
    /// the crate root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    /// Rust-level function name the decision lives in, as rustc
    /// prints it in the MIR header (impl methods read
    /// `<impl at FILE:L:C: L:C>::name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,
    /// Report name taken from the coverage symbol the decision's
    /// source spans joined to: demangled without crate hashes, with
    /// generic arguments removed (for example
    /// `<chrono::offset::fixed::FixedOffset>::east_opt`). `None`
    /// when no coverage record joined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 1-based line number where the decision starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The lift that turned the decision's non-Boolean arms into its
    /// outcome ([`crate::decision::Lift`]), when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lift: Option<crate::decision::Lift>,
}

/// Static MC/DC analysis combined with runtime evidence: which
/// conditions had their independence demonstrated by the observed
/// test runs and which did not.
///
/// Produced by [`analyze_with_runtime`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuntimeAnalysis {
    /// The static analysis (truth table + all valid pairs).
    pub matrix: IndependenceMatrix,
    /// Per-condition status under the observed tests.
    pub condition_status: BTreeMap<String, ConditionStatus>,
    /// Optional source-location metadata — populated by callers that
    /// know where the decision came from (e.g. the `cargo floyd`
    /// driver attaches it after decomposition). Older JSON reports
    /// without this field deserialize correctly thanks to
    /// `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<DecisionProvenance>,
    /// Number of ambiguous observations excluded from pair finding
    /// (see [`ConditionObservation::ambiguous`]).
    #[serde(default)]
    pub ambiguous_observations: usize,
    /// For each [`ConditionStatus::Unexercised`] condition that some
    /// ambiguous observation evaluated: why each such test cannot
    /// hold a row the condition's first independence pair is missing
    /// (the counter of an outcome that row needs is zero in that
    /// test). Lets reports say the condition is unexercised *with
    /// the counts*, rather than leave the reader to wonder why an
    /// evaluated condition is not unknown. Absent for conditions no
    /// ambiguous test evaluated.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ruled_out: BTreeMap<String, Vec<RuledOut>>,
}

/// Why one ambiguous observation cannot hold one missing row of an
/// independence pair: the row's evaluation path needs `condition` to
/// take `value`, and the test's counter for `condition` shows it
/// never did. See [`RuntimeAnalysis::ruled_out`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuledOut {
    /// The ambiguous test.
    pub test: String,
    /// How many times it evaluated the decision, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluations: Option<u64>,
    /// The pair row no observation covers.
    pub row: TruthTableRow,
    /// A condition on the row's evaluation path.
    pub condition: String,
    /// The value the row needs `condition` to take.
    pub value: bool,
    /// The test's counters for `condition`; the one for `value` is 0.
    pub counts: BranchCounts,
}

/// Combine static MC/DC analysis with observed test runs.
///
/// For each condition, scans the valid independence pairs produced
/// by [`analyze_with_variant`] and returns the first pair whose
/// `test_1` and `test_2` input assignments were both observed.
/// A condition is [`ConditionStatus::Exercised`] iff such a pair
/// exists.
///
/// Observations match by exact `inputs` equality. Conditions
/// short-circuited in a given test are omitted from that test's
/// [`ConditionObservation::inputs`] map and therefore cannot
/// participate in a pair match — which is the correct MC/DC
/// behaviour: a condition that was never actually evaluated cannot
/// be said to have its independence demonstrated.
///
/// Ambiguous observations ([`ConditionObservation::ambiguous`]) are
/// excluded from pair finding under every [`Variant`]. They only
/// decide between [`ConditionStatus::Ambiguous`] and
/// [`ConditionStatus::Unexercised`] for a condition no pair
/// demonstrates; see [`ConditionStatus::Ambiguous`] for the rule.
pub fn analyze_with_runtime(
    tree: &DecisionTree,
    observations: &[ConditionObservation],
    variant: Variant,
) -> RuntimeAnalysis {
    let matrix = analyze_with_variant(tree, variant);
    let (ambiguous, vectors): (Vec<&ConditionObservation>, Vec<&ConditionObservation>) =
        observations.iter().partition(|o| o.ambiguous);
    let mut condition_status = BTreeMap::new();
    let mut ruled_out = BTreeMap::new();
    if let Some(decision) = tree.decisions.first() {
        for (cond, pairs) in &matrix.independence_pairs {
            let (status, why) = classify_condition(cond, pairs, decision, &vectors, &ambiguous);
            if !why.is_empty() {
                ruled_out.insert(cond.clone(), why);
            }
            condition_status.insert(cond.clone(), status);
        }
    }
    RuntimeAnalysis {
        matrix,
        condition_status,
        provenance: None,
        ambiguous_observations: ambiguous.len(),
        ruled_out,
    }
}

/// Classify one condition, and for an unexercised one that an
/// ambiguous test evaluated, say why each such test is ruled out.
///
/// Only condition-vector observations (`observations`) can
/// demonstrate a pair. Otherwise the condition is
/// [`ConditionStatus::Ambiguous`] iff some pair is *completable*:
/// each of its rows is covered by a vector or possible in an
/// ambiguous observation ([`row_possible_in`]). The tests listed are
/// the ambiguous ones that could hold a missing row of a completable
/// pair. If no pair is completable, the condition is
/// [`ConditionStatus::Unexercised`]: for every pair, some row no
/// vector covers needs an outcome that no ambiguous test took, so no
/// test can have demonstrated it.
///
/// This is the decision-level check too: a row's outcome is fixed by
/// its path, so a row that is possible in a test is one whose path,
/// and therefore outcome, the test's counters allow.
fn classify_condition(
    cond: &str,
    pairs: &[IndependencePair],
    decision: &Node,
    observations: &[&ConditionObservation],
    ambiguous: &[&ConditionObservation],
) -> (ConditionStatus, Vec<RuledOut>) {
    let covered = |row: &TruthTableRow| observations.iter().any(|o| observation_covers_row(o, row));
    for pair in pairs {
        if covered(&pair.test_1) && covered(&pair.test_2) {
            return (ConditionStatus::Exercised(pair.clone()), Vec::new());
        }
    }
    let mut takers: BTreeSet<usize> = BTreeSet::new();
    for pair in pairs {
        let mut pair_takers = Vec::new();
        let completable = [&pair.test_1, &pair.test_2]
            .into_iter()
            .filter(|row| !covered(row))
            .all(|row| {
                let before = pair_takers.len();
                pair_takers.extend(
                    (0..ambiguous.len()).filter(|&i| row_possible_in(ambiguous[i], row, decision)),
                );
                pair_takers.len() > before
            });
        if completable {
            takers.extend(pair_takers);
        }
    }
    if !takers.is_empty() {
        let tests = takers
            .into_iter()
            .map(|i| test_label(ambiguous[i]))
            .collect();
        return (ConditionStatus::Ambiguous { tests }, Vec::new());
    }
    let why = pairs
        .first()
        .map(|pair| {
            [&pair.test_1, &pair.test_2]
                .into_iter()
                .filter(|row| !covered(row))
                .flat_map(|row| {
                    ambiguous
                        .iter()
                        .filter(|o| o.counts.get(cond).is_some_and(|c| c.total() > 0))
                        .filter_map(move |o| rule_out(o, row, decision))
                })
                .collect()
        })
        .unwrap_or_default();
    (ConditionStatus::Unexercised, why)
}

fn test_label(o: &ConditionObservation) -> String {
    o.test_name.clone().unwrap_or_else(|| "?".to_string())
}

/// True iff the ambiguous observation `o` could have evaluated `row`:
/// every condition on the row's evaluation path (the conditions the
/// decision tests under `row`, short-circuited ones excluded) has a
/// non-zero counter in `o` for the value the row gives it.
///
/// Sound: if `o` evaluated `row` (or any vector covering it, which
/// follows the same path), each condition on that path was tested
/// with that value at least once, so its counter for the value is
/// non-zero. A zero counter therefore proves `o` never evaluated the
/// row. A condition with no counter at all in `o` (its span did not
/// join) is no evidence either way and does not rule the row out.
fn row_possible_in(o: &ConditionObservation, row: &TruthTableRow, decision: &Node) -> bool {
    rule_out(o, row, decision).is_none() && evaluation_path(decision, &row.inputs).is_some()
}

/// The first condition on `row`'s evaluation path whose counter in
/// `o` is zero for the value the row needs, as a [`RuledOut`].
fn rule_out(o: &ConditionObservation, row: &TruthTableRow, decision: &Node) -> Option<RuledOut> {
    let path = evaluation_path(decision, &row.inputs)?;
    path.into_iter().find_map(|(name, value)| {
        let counts = *o.counts.get(&name)?;
        let taken = if value {
            counts.true_count
        } else {
            counts.false_count
        };
        (taken == 0).then(|| RuledOut {
            test: test_label(o),
            evaluations: o.evaluations,
            row: row.clone(),
            condition: name,
            value,
            counts,
        })
    })
}

/// The conditions `decision` tests, in order, when its conditions
/// take the values `value_of` gives them, with the value each takes,
/// and the outcome. Conditions short-circuited under those values are
/// not on the path. `None` if the walk reaches a condition `value_of`
/// has no value for.
pub(crate) fn evaluation_path(
    decision: &Node,
    values: &BTreeMap<String, bool>,
) -> Option<Vec<(String, bool)>> {
    evaluation_walk(decision, |c| values.get(c).copied()).map(|(path, _)| path)
}

/// [`evaluation_path`] with the values supplied by a function, and the
/// decision outcome alongside the path.
pub(crate) fn evaluation_walk(
    decision: &Node,
    value_of: impl Fn(&str) -> Option<bool>,
) -> Option<(Vec<(String, bool)>, bool)> {
    let mut path = Vec::new();
    let mut node = decision;
    loop {
        match node {
            Node::Const { value } => return Some((path, *value)),
            Node::Condition { name } => {
                let v = value_of(name)?;
                path.push((name.clone(), v));
                return Some((path, v));
            }
            Node::Ite {
                cond,
                then_branch,
                else_branch,
            } => {
                let v = value_of(cond)?;
                path.push((cond.clone(), v));
                node = if v { then_branch } else { else_branch };
            }
        }
    }
}

/// True iff `observation` is compatible with `row` — every condition
/// the observation actually saw agrees with `row`'s expected value,
/// and the results match.
///
/// Conditions present in `row` but absent from `observation.inputs`
/// are treated as wildcards: they were short-circuited in this
/// observation, which under masking MC/DC means their value did
/// not affect the result — the observation truly represents the
/// row's input class, regardless of what the missing condition's
/// source value happened to be.
fn observation_covers_row(observation: &ConditionObservation, row: &TruthTableRow) -> bool {
    if observation.ambiguous || observation.result != Some(row.result) {
        return false;
    }
    observation
        .inputs
        .iter()
        .all(|(k, v)| row.inputs.get(k) == Some(v))
}

/// Compute the MC/DC independence matrix for a decision tree under
/// an explicit variant.
///
/// Phase 0 scope: analyses the first decision in `tree` only.
/// Multi-decision support lands with multi-decision corpus patterns.
pub fn analyze_with_variant(tree: &DecisionTree, variant: Variant) -> IndependenceMatrix {
    let mut matrix = IndependenceMatrix {
        variant,
        ..IndependenceMatrix::default()
    };

    let Some(decision) = tree.decisions.first() else {
        return matrix;
    };

    let conditions = collect_conditions(decision);
    matrix.truth_table = enumerate_truth_table(decision, &conditions);

    for c in &conditions {
        let pairs = find_pairs(c, &matrix.truth_table, variant);
        matrix.independence_pairs.insert(c.clone(), pairs);
    }
    matrix.conditions = conditions;
    matrix
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Collect the set of atomic condition names referenced in a [`Node`],
/// returned in deterministic sorted order.
pub(crate) fn collect_conditions(node: &Node) -> Vec<String> {
    let mut set = BTreeSet::new();
    walk_conditions(node, &mut set);
    set.into_iter().collect()
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
    }
}

/// Evaluate a [`Node`] under a given condition assignment.
fn evaluate(node: &Node, env: &BTreeMap<String, bool>) -> bool {
    match node {
        Node::Condition { name } => *env.get(name).expect("env covers all conditions"),
        Node::Const { value } => *value,
        Node::Ite {
            cond,
            then_branch,
            else_branch,
        } => {
            if *env.get(cond).expect("env covers all conditions") {
                evaluate(then_branch, env)
            } else {
                evaluate(else_branch, env)
            }
        }
    }
}

/// Enumerate the full `2^n`-row truth table for a decision and its
/// condition list (assumed to be in sorted order).
fn enumerate_truth_table(decision: &Node, conditions: &[String]) -> Vec<TruthTableRow> {
    let n = conditions.len();
    let mut table = Vec::with_capacity(1 << n);
    for bits in 0..(1u64 << n) {
        let mut inputs = BTreeMap::new();
        for (i, c) in conditions.iter().enumerate() {
            inputs.insert(c.clone(), (bits >> i) & 1 == 1);
        }
        let result = evaluate(decision, &inputs);
        table.push(TruthTableRow { inputs, result });
    }
    table
}

/// Find all valid independence pairs for `condition` under the given
/// MC/DC variant.
fn find_pairs(
    condition: &str,
    truth_table: &[TruthTableRow],
    variant: Variant,
) -> Vec<IndependencePair> {
    let mut pairs = Vec::new();
    for i in 0..truth_table.len() {
        for j in (i + 1)..truth_table.len() {
            let row_i = &truth_table[i];
            let row_j = &truth_table[j];

            // Must differ at `condition`.
            let c_i = row_i.inputs.get(condition).copied().unwrap_or(false);
            let c_j = row_j.inputs.get(condition).copied().unwrap_or(false);
            if c_i == c_j {
                continue;
            }

            // Result must differ.
            if row_i.result == row_j.result {
                continue;
            }

            if pair_satisfies_variant(condition, row_i, row_j, truth_table, variant) {
                pairs.push(IndependencePair {
                    test_1: row_i.clone(),
                    test_2: row_j.clone(),
                });
            }
        }
    }
    pairs
}

/// Check the variant-specific constraint on the *other* conditions
/// of a candidate independence pair.
fn pair_satisfies_variant(
    condition: &str,
    row_i: &TruthTableRow,
    row_j: &TruthTableRow,
    truth_table: &[TruthTableRow],
    variant: Variant,
) -> bool {
    for (other, &v_i) in &row_i.inputs {
        if other == condition {
            continue;
        }
        let v_j = row_j.inputs.get(other).copied().unwrap_or(false);
        if v_i == v_j {
            continue;
        }
        match variant {
            // Strict: all non-tested conditions must be identical.
            Variant::UniqueCause => return false,
            // Masking: `other` may differ if it's masked in both rows
            // (its value doesn't affect the result in either).
            Variant::Masking => {
                if !is_masked_in(other, row_i, truth_table)
                    || !is_masked_in(other, row_j, truth_table)
                {
                    return false;
                }
            }
        }
    }
    true
}

/// True iff `condition` does not affect the result in `row`'s
/// assignment — i.e. the truth-table row obtained by flipping
/// `condition` has the same result.
fn is_masked_in(condition: &str, row: &TruthTableRow, truth_table: &[TruthTableRow]) -> bool {
    let mut flipped = row.inputs.clone();
    if let Some(v) = flipped.get_mut(condition) {
        *v = !*v;
    }
    truth_table
        .iter()
        .any(|r| r.inputs == flipped && r.result == row.result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(name: &str) -> Node {
        Node::Condition {
            name: name.to_string(),
        }
    }
    fn const_(value: bool) -> Node {
        Node::Const { value }
    }
    fn ite(c: &str, t: Node, e: Node) -> Node {
        Node::Ite {
            cond: c.to_string(),
            then_branch: Box::new(t),
            else_branch: Box::new(e),
        }
    }
    fn row(inputs: &[(&str, bool)], result: bool) -> TruthTableRow {
        TruthTableRow {
            inputs: inputs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            result,
        }
    }

    // && : Ite { a, b, false }
    fn and_tree() -> DecisionTree {
        DecisionTree {
            decisions: vec![ite("a", cond("b"), const_(false))],
        }
    }

    // || : Ite { a, true, b }
    fn or_tree() -> DecisionTree {
        DecisionTree {
            decisions: vec![ite("a", const_(true), cond("b"))],
        }
    }

    // (a && b) || c : Ite { a, Ite { b, true, c }, c }
    fn nested_tree() -> DecisionTree {
        DecisionTree {
            decisions: vec![ite("a", ite("b", const_(true), cond("c")), cond("c"))],
        }
    }

    // -----------------------------------------------------------------------
    // Truth tables
    // -----------------------------------------------------------------------

    #[test]
    fn and_truth_table_correct() {
        let m = analyze(&and_tree());
        assert_eq!(m.conditions, vec!["a", "b"]);
        assert_eq!(m.truth_table.len(), 4);
        assert!(m
            .truth_table
            .contains(&row(&[("a", false), ("b", false)], false)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", false), ("b", true)], false)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", true), ("b", false)], false)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", true), ("b", true)], true)));
    }

    #[test]
    fn or_truth_table_correct() {
        let m = analyze(&or_tree());
        assert_eq!(m.conditions, vec!["a", "b"]);
        assert_eq!(m.truth_table.len(), 4);
        assert!(m
            .truth_table
            .contains(&row(&[("a", false), ("b", false)], false)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", false), ("b", true)], true)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", true), ("b", false)], true)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", true), ("b", true)], true)));
    }

    #[test]
    fn nested_truth_table_correct() {
        let m = analyze(&nested_tree());
        assert_eq!(m.conditions, vec!["a", "b", "c"]);
        assert_eq!(m.truth_table.len(), 8);
        // (a && b) || c
        assert!(m
            .truth_table
            .contains(&row(&[("a", false), ("b", false), ("c", false)], false)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", false), ("b", false), ("c", true)], true)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", false), ("b", true), ("c", false)], false)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", true), ("b", false), ("c", false)], false)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", true), ("b", true), ("c", false)], true)));
        assert!(m
            .truth_table
            .contains(&row(&[("a", true), ("b", true), ("c", true)], true)));
    }

    // -----------------------------------------------------------------------
    // Independence pairs against the corpus expected pairs
    // -----------------------------------------------------------------------

    #[test]
    fn and_independence_pairs_match_corpus_001() {
        let m = analyze(&and_tree());
        // Corpus 001 declares:
        //   a's pair: (F,T) -> F  vs  (T,T) -> T
        //   b's pair: (T,F) -> F  vs  (T,T) -> T
        let a_pair = IndependencePair {
            test_1: row(&[("a", false), ("b", true)], false),
            test_2: row(&[("a", true), ("b", true)], true),
        };
        let b_pair = IndependencePair {
            test_1: row(&[("a", true), ("b", false)], false),
            test_2: row(&[("a", true), ("b", true)], true),
        };
        assert!(
            m.independence_pairs["a"].contains(&a_pair),
            "expected a-pair in {:?}",
            m.independence_pairs["a"]
        );
        assert!(
            m.independence_pairs["b"].contains(&b_pair),
            "expected b-pair in {:?}",
            m.independence_pairs["b"]
        );
    }

    #[test]
    fn or_independence_pairs_match_corpus_002() {
        let m = analyze(&or_tree());
        // Corpus 002 declares:
        //   a's pair: (F,F) -> F  vs  (T,F) -> T
        //   b's pair: (F,F) -> F  vs  (F,T) -> T
        let a_pair = IndependencePair {
            test_1: row(&[("a", false), ("b", false)], false),
            test_2: row(&[("a", true), ("b", false)], true),
        };
        let b_pair = IndependencePair {
            test_1: row(&[("a", false), ("b", false)], false),
            test_2: row(&[("a", false), ("b", true)], true),
        };
        assert!(
            m.independence_pairs["a"].contains(&a_pair),
            "expected a-pair in {:?}",
            m.independence_pairs["a"]
        );
        assert!(
            m.independence_pairs["b"].contains(&b_pair),
            "expected b-pair in {:?}",
            m.independence_pairs["b"]
        );
    }

    #[test]
    fn nested_independence_pairs_match_corpus_003() {
        let m = analyze(&nested_tree());
        // Corpus 003 declares:
        //   a's pair: (F,T,F) -> F  vs  (T,T,F) -> T
        //   b's pair: (T,F,F) -> F  vs  (T,T,F) -> T
        //   c's pair: (T,F,F) -> F  vs  (T,F,T) -> T
        let a_pair = IndependencePair {
            test_1: row(&[("a", false), ("b", true), ("c", false)], false),
            test_2: row(&[("a", true), ("b", true), ("c", false)], true),
        };
        let b_pair = IndependencePair {
            test_1: row(&[("a", true), ("b", false), ("c", false)], false),
            test_2: row(&[("a", true), ("b", true), ("c", false)], true),
        };
        let c_pair = IndependencePair {
            test_1: row(&[("a", true), ("b", false), ("c", false)], false),
            test_2: row(&[("a", true), ("b", false), ("c", true)], true),
        };
        assert!(
            m.independence_pairs["a"].contains(&a_pair),
            "expected a-pair in {:?}",
            m.independence_pairs["a"]
        );
        assert!(
            m.independence_pairs["b"].contains(&b_pair),
            "expected b-pair in {:?}",
            m.independence_pairs["b"]
        );
        assert!(
            m.independence_pairs["c"].contains(&c_pair),
            "expected c-pair in {:?}",
            m.independence_pairs["c"]
        );
    }

    // -----------------------------------------------------------------------
    // Runtime extension: classify each condition as exercised or not
    // based on observed test runs.
    // -----------------------------------------------------------------------

    fn obs(name: &str, inputs: &[(&str, bool)], result: bool) -> ConditionObservation {
        ConditionObservation::vector(
            Some(name.to_string()),
            inputs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            result,
        )
    }

    #[test]
    fn and_with_ff_tt_masking_exercises_a_only() {
        // ff: a=F, b short-circuited (omitted from observation inputs)
        // tt: a=T, b=T
        //
        // Under masking MC/DC the ff observation represents the
        // equivalence class {(a=F, b=T), (a=F, b=F)} (b is masked
        // when a is false), so it covers test_1 of a's pair
        // (a=F, b=T) -> F. The tt observation covers test_2 of the
        // same pair. -> a is Exercised.
        //
        // b's pair {(a=T, b=F) -> F, (a=T, b=T) -> T} needs an
        // observation with a=T AND b=F; tt has a=T, b=T, and no
        // observation covers (a=T, b=F). -> b is Unexercised.
        let obs_list = [
            obs("ff", &[("a", false)], false),
            obs("tt", &[("a", true), ("b", true)], true),
        ];
        let r = analyze_with_runtime(&and_tree(), &obs_list, Variant::Masking);
        assert!(matches!(
            r.condition_status["a"],
            ConditionStatus::Exercised(_)
        ));
        assert!(matches!(
            r.condition_status["b"],
            ConditionStatus::Unexercised
        ));
    }

    #[test]
    fn and_with_full_corpus_min_set_exercises_both() {
        // Corpus pattern 001's declared minimum test set:
        //   (F, T) -> F   (a's pair test 1, b's pair test 1)
        //   (T, F) -> F   (b's pair test 1, NOT included since (T,F) needed)
        //   (T, T) -> T
        // Actually the declared minimum_test_set is {(F,T),(T,F),(T,T)} — 3 tests.
        let obs_list = [
            obs("ft", &[("a", false), ("b", true)], false),
            obs("tf", &[("a", true), ("b", false)], false),
            obs("tt", &[("a", true), ("b", true)], true),
        ];
        let r = analyze_with_runtime(&and_tree(), &obs_list, Variant::Masking);
        assert!(matches!(
            r.condition_status["a"],
            ConditionStatus::Exercised(_)
        ));
        assert!(matches!(
            r.condition_status["b"],
            ConditionStatus::Exercised(_)
        ));
    }

    #[test]
    fn and_with_only_a_pair_leaves_b_unexercised() {
        // Pair for a: (F,T) vs (T,T). Both observed.
        // Pair for b requires (T,F) — not observed.
        let obs_list = [
            obs("ft", &[("a", false), ("b", true)], false),
            obs("tt", &[("a", true), ("b", true)], true),
        ];
        let r = analyze_with_runtime(&and_tree(), &obs_list, Variant::Masking);
        assert!(matches!(
            r.condition_status["a"],
            ConditionStatus::Exercised(_)
        ));
        assert!(matches!(
            r.condition_status["b"],
            ConditionStatus::Unexercised
        ));
    }

    #[test]
    fn or_with_partial_observations_exercises_a_only() {
        // ff: a=F, b=F (no short-circuit — a is F under ||, so b
        //   IS evaluated)
        // tt: a=T, b short-circuited (omitted from observation
        //   inputs)
        //
        // Under masking MC/DC, tt represents the class
        // {(a=T, b=F), (a=T, b=T)} (b is masked when a is true
        // under ||), so it covers test_2 of a's pair
        // (a=T, b=F) -> T. ff covers test_1. -> a Exercised.
        //
        // b's pair {(a=F, b=F) -> F, (a=F, b=T) -> T} needs an
        // observation with a=F AND b=T; we have ff (a=F, b=F) but
        // nothing with b=T. -> b Unexercised.
        let obs_list = [
            obs("ff", &[("a", false), ("b", false)], false),
            obs("tt", &[("a", true)], true),
        ];
        let r = analyze_with_runtime(&or_tree(), &obs_list, Variant::Masking);
        assert!(matches!(
            r.condition_status["a"],
            ConditionStatus::Exercised(_)
        ));
        assert!(matches!(
            r.condition_status["b"],
            ConditionStatus::Unexercised
        ));
    }

    #[test]
    fn nested_full_min_set_exercises_all_three() {
        // Corpus 003's minimum test set: 4 tests.
        let obs_list = [
            obs("1", &[("a", false), ("b", true), ("c", false)], false),
            obs("2", &[("a", true), ("b", true), ("c", false)], true),
            obs("3", &[("a", true), ("b", false), ("c", false)], false),
            obs("4", &[("a", true), ("b", false), ("c", true)], true),
        ];
        let r = analyze_with_runtime(&nested_tree(), &obs_list, Variant::Masking);
        for c in ["a", "b", "c"] {
            assert!(
                matches!(r.condition_status[c], ConditionStatus::Exercised(_)),
                "{c} should be exercised; status={:?}",
                r.condition_status[c]
            );
        }
    }

    fn ambiguous_obs(
        name: &str,
        evaluations: u64,
        raw: &[(&str, u64, u64)],
    ) -> ConditionObservation {
        ConditionObservation::ambiguous(
            Some(name.to_string()),
            evaluations,
            raw.iter()
                .map(|(k, t, f)| {
                    (
                        k.to_string(),
                        BranchCounts {
                            true_count: *t,
                            false_count: *f,
                        },
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn ambiguous_observations_never_demonstrate_a_pair_in_either_variant() {
        // Even an ambiguous record whose fields were filled with a
        // full vector must be ignored: the flag, not the contents,
        // excludes it.
        let mut forged = ambiguous_obs("loop", 3, &[("a", 2, 1), ("b", 1, 1)]);
        forged.inputs = [("a".to_string(), false), ("b".to_string(), true)]
            .into_iter()
            .collect();
        forged.result = Some(false);
        let obs_list = [forged, obs("tt", &[("a", true), ("b", true)], true)];
        for variant in [Variant::Masking, Variant::UniqueCause] {
            let r = analyze_with_runtime(&and_tree(), &obs_list, variant);
            assert_eq!(r.ambiguous_observations, 1);
            for c in ["a", "b"] {
                match &r.condition_status[c] {
                    ConditionStatus::Ambiguous { tests } => {
                        assert_eq!(tests, &vec!["loop".to_string()], "{variant:?} {c}")
                    }
                    other => panic!("{variant:?} {c}: expected Ambiguous, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn ambiguous_status_only_for_conditions_the_ambiguous_test_evaluated() {
        // The ambiguous test never evaluated b (b = [0, 0]): b's gap
        // is a genuine Unexercised, not an ambiguity.
        let obs_list = [
            ambiguous_obs("loop", 2, &[("a", 1, 1), ("b", 0, 0)]),
            obs("tt", &[("a", true), ("b", true)], true),
        ];
        let r = analyze_with_runtime(&and_tree(), &obs_list, Variant::Masking);
        assert!(matches!(
            r.condition_status["a"],
            ConditionStatus::Ambiguous { .. }
        ));
        assert!(matches!(
            r.condition_status["b"],
            ConditionStatus::Unexercised
        ));
    }

    #[test]
    fn vector_evidence_wins_over_ambiguous_evidence() {
        let obs_list = [
            ambiguous_obs("loop", 3, &[("a", 2, 1), ("b", 1, 1)]),
            obs("ff", &[("a", false)], false),
            obs("tt", &[("a", true), ("b", true)], true),
        ];
        let r = analyze_with_runtime(&and_tree(), &obs_list, Variant::Masking);
        assert!(matches!(
            r.condition_status["a"],
            ConditionStatus::Exercised(_)
        ));
        assert!(matches!(
            r.condition_status["b"],
            ConditionStatus::Ambiguous { .. }
        ));
    }

    /// `b == 32 || b == 10` as decomposed from anyhow's
    /// `src/ensure.rs:60` closure `|b| b == b' ' || b == b'\n'`.
    fn anyhow_whitespace_tree() -> DecisionTree {
        DecisionTree {
            decisions: vec![ite("b == 32", const_(true), cond("b == 10"))],
        }
    }

    #[test]
    fn missing_outcome_never_taken_anywhere_is_unexercised() {
        // anyhow `test_whitespace` (cut from its llvm-cov export): 9
        // evaluations, `b == 32` = [1 T, 8 F], `b == 10` = [0 T, 8 F],
        // kept ambiguous here (as before rule 4 of
        // `observations_from_counts`), plus a single-pass test that
        // saw (F,F) -> F. `b == 10`'s only pair needs
        // (b == 32 = F, b == 10 = T) -> T; no test ever took
        // `b == 10` = T, so it is unexercised, not unknown.
        let obs_list = [
            ambiguous_obs(
                "test_whitespace",
                9,
                &[("b == 32", 1, 8), ("b == 10", 0, 8)],
            ),
            obs("test_as", &[("b == 32", false), ("b == 10", false)], false),
        ];
        for variant in [Variant::Masking, Variant::UniqueCause] {
            let r = analyze_with_runtime(&anyhow_whitespace_tree(), &obs_list, variant);
            assert!(
                matches!(r.condition_status["b == 10"], ConditionStatus::Unexercised),
                "{variant:?}: {:?}",
                r.condition_status["b == 10"]
            );
            let why = &r.ruled_out["b == 10"];
            assert_eq!(why.len(), 1, "{why:?}");
            assert_eq!(why[0].test, "test_whitespace");
            assert_eq!(why[0].evaluations, Some(9));
            assert_eq!(why[0].condition, "b == 10");
            assert!(why[0].value);
            assert_eq!(
                why[0].counts,
                BranchCounts {
                    true_count: 0,
                    false_count: 8
                }
            );
            assert_eq!(
                why[0].row,
                row(&[("b == 32", false), ("b == 10", true)], true)
            );
            // `b == 32`'s missing row (T) -> T is possible in the
            // ambiguous test (it took `b == 32` = T once).
            match &r.condition_status["b == 32"] {
                ConditionStatus::Ambiguous { tests } => {
                    assert_eq!(tests, &vec!["test_whitespace".to_string()])
                }
                other => panic!("{variant:?}: b == 32: {other:?}"),
            }
            assert!(!r.ruled_out.contains_key("b == 32"));
        }
    }

    #[test]
    fn missing_outcome_taken_only_in_multi_evaluation_tests_is_ambiguous() {
        // chrono `from_hms_nano_opt` (src/naive/time/mod.rs:429),
        // `h || m || s || (n1 && s59) || n2`, with the counts of
        // `offset::local::tests::test_leap_second` (2 evaluations)
        // kept ambiguous, and single-evaluation vectors that never
        // had `sec != 59` = T while `nano >= 1e9` = T.
        let tree = chrono_tree();
        let f = false;
        let obs_list = [
            ambiguous_obs(
                "test_leap_second",
                2,
                &[
                    ("hour >= 24", 0, 2),
                    ("min >= 60", 0, 2),
                    ("sec >= 60", 0, 2),
                    ("nano >= 1000000000", 2, 0),
                    ("sec != 59", 1, 1),
                    ("nano >= 2000000000", 0, 1),
                ],
            ),
            obs("plain", &chrono_row(f, f, f, Some(f), None, Some(f)), false),
            obs(
                "leap",
                &chrono_row(f, f, f, Some(true), Some(false), Some(false)),
                false,
            ),
        ];
        let r = analyze_with_runtime(&tree, &obs_list, Variant::Masking);
        for c in ["nano >= 1000000000", "sec != 59"] {
            match &r.condition_status[c] {
                ConditionStatus::Ambiguous { tests } => {
                    assert_eq!(tests, &vec!["test_leap_second".to_string()], "{c}")
                }
                other => panic!("{c}: {other:?}"),
            }
        }
        // `hour >= 24` = T was never taken by the ambiguous test.
        assert!(matches!(
            r.condition_status["hour >= 24"],
            ConditionStatus::Unexercised
        ));
        assert_eq!(r.ruled_out["hour >= 24"][0].condition, "hour >= 24");
    }

    #[test]
    fn missing_outcome_taken_in_a_single_evaluation_test_is_exercised() {
        // The anyhow case plus a single-evaluation test that took
        // `b == 10` = T: the pair is found from vectors.
        let obs_list = [
            ambiguous_obs(
                "test_whitespace",
                9,
                &[("b == 32", 1, 8), ("b == 10", 0, 8)],
            ),
            obs("test_as", &[("b == 32", false), ("b == 10", false)], false),
            obs("newline", &[("b == 32", false), ("b == 10", true)], true),
        ];
        let r = analyze_with_runtime(&anyhow_whitespace_tree(), &obs_list, Variant::Masking);
        assert!(matches!(
            r.condition_status["b == 10"],
            ConditionStatus::Exercised(_)
        ));
        assert!(r.ruled_out.is_empty());
    }

    #[test]
    fn row_needing_two_outcomes_from_one_test_needs_both_counters() {
        // a && b: b's missing row (T,F) -> F needs a = T AND b = F in
        // the same test. "loop_a" took a = T but never evaluated b
        // false; "loop_b" has no a = T. Neither can hold the row.
        let obs_list = [
            ambiguous_obs("loop_a", 2, &[("a", 1, 1), ("b", 1, 0)]),
            ambiguous_obs("loop_b", 2, &[("a", 0, 2), ("b", 0, 0)]),
            obs("tt", &[("a", true), ("b", true)], true),
        ];
        let r = analyze_with_runtime(&and_tree(), &obs_list, Variant::Masking);
        assert!(matches!(
            r.condition_status["b"],
            ConditionStatus::Unexercised
        ));
        // Only loop_a evaluated b, so only it is listed.
        let why = &r.ruled_out["b"];
        assert_eq!(why.len(), 1);
        assert_eq!(
            (
                why[0].test.as_str(),
                why[0].condition.as_str(),
                why[0].value
            ),
            ("loop_a", "b", false)
        );
        // a's missing row (F,-) -> F is possible in both.
        match &r.condition_status["a"] {
            ConditionStatus::Ambiguous { tests } => {
                assert_eq!(tests, &vec!["loop_a".to_string(), "loop_b".to_string()])
            }
            other => panic!("{other:?}"),
        }
    }

    /// chrono `NaiveTime::from_hms_nano_opt`'s decision
    /// `hour >= 24 || min >= 60 || sec >= 60
    ///  || (nano >= 1_000_000_000 && sec != 59) || nano >= 2_000_000_000`.
    fn chrono_tree() -> DecisionTree {
        let n2 = || cond("nano >= 2000000000");
        DecisionTree {
            decisions: vec![ite(
                "hour >= 24",
                const_(true),
                ite(
                    "min >= 60",
                    const_(true),
                    ite(
                        "sec >= 60",
                        const_(true),
                        ite(
                            "nano >= 1000000000",
                            ite("sec != 59", const_(true), n2()),
                            n2(),
                        ),
                    ),
                ),
            )],
        }
    }

    fn chrono_row(
        h: bool,
        m: bool,
        s: bool,
        n1: Option<bool>,
        s59: Option<bool>,
        n2: Option<bool>,
    ) -> Vec<(&'static str, bool)> {
        let mut v = vec![("hour >= 24", h), ("min >= 60", m), ("sec >= 60", s)];
        for (k, x) in [
            ("nano >= 1000000000", n1),
            ("sec != 59", s59),
            ("nano >= 2000000000", n2),
        ] {
            if let Some(x) = x {
                v.push((k, x));
            }
        }
        v
    }

    #[test]
    fn old_observation_json_still_deserializes() {
        // Pre-ambiguity observation shape: `result` is a bare bool,
        // no `ambiguous` / `evaluations` / `counts`.
        let old = r#"{"test_name": "t", "inputs": {"a": true}, "result": true}"#;
        let o: ConditionObservation = serde_json::from_str(old).expect("parse old shape");
        assert!(!o.ambiguous);
        assert_eq!(o.result, Some(true));
        assert_eq!(o.evaluations, None);
        assert!(o.counts.is_empty());
    }

    // -----------------------------------------------------------------------
    // Variant semantics
    // -----------------------------------------------------------------------

    #[test]
    fn unique_cause_subset_of_masking() {
        for tree in [and_tree(), or_tree(), nested_tree()] {
            let masking = analyze_with_variant(&tree, Variant::Masking);
            let unique = analyze_with_variant(&tree, Variant::UniqueCause);
            for (c, unique_pairs) in &unique.independence_pairs {
                let masking_pairs = &masking.independence_pairs[c];
                for p in unique_pairs {
                    assert!(
                        masking_pairs.contains(p),
                        "{c}: unique-cause pair missing from masking set: {p:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn runtime_analysis_json_round_trip() {
        // The public types derive Deserialize alongside Serialize so
        // downstream tools (reporters, dashboards, qualification kits)
        // can parse the engine's JSON output back into live types.
        // Build a non-trivial analysis, serialize it, deserialize it,
        // and check the recovered value equals the original.
        let tree = and_tree();
        let obs = vec![
            obs("t_ff", &[("a", false)], false),
            obs("t_tf", &[("a", true), ("b", false)], false),
            obs("t_tt", &[("a", true), ("b", true)], true),
        ];

        let analysis = analyze_with_runtime(&tree, &obs, Variant::Masking);
        let json = serde_json::to_string(&analysis).expect("serialize");
        let round: RuntimeAnalysis = serde_json::from_str(&json).expect("deserialize");

        // Compare via re-serialisation. RuntimeAnalysis itself has no
        // PartialEq (it carries ConditionStatus, which does not), so
        // round-trip equality is checked structurally through the
        // canonical JSON representation.
        let json2 = serde_json::to_string(&round).expect("re-serialize");
        assert_eq!(json, json2, "deserialize-then-serialize must be identity");
    }

    #[test]
    fn variant_json_round_trip_kebab_case() {
        // Variant uses rename_all = "kebab-case" on serialize; the
        // deserialize path must accept the same spelling.
        let v: Variant = serde_json::from_str("\"masking\"").unwrap();
        assert_eq!(v, Variant::Masking);
        let v: Variant = serde_json::from_str("\"unique-cause\"").unwrap();
        assert_eq!(v, Variant::UniqueCause);
    }

    #[test]
    fn provenance_round_trips_when_present() {
        let analysis = RuntimeAnalysis {
            matrix: IndependenceMatrix::default(),
            condition_status: BTreeMap::new(),
            provenance: Some(DecisionProvenance {
                function_name: Some("allow_actuation".to_string()),
                source_file: Some("src/lib.rs".to_string()),
                line: Some(42),
                display_name: Some("<brakes::Controller>::allow_actuation".to_string()),
                lift: None,
            }),
            ambiguous_observations: 0,
            ruled_out: BTreeMap::new(),
        };
        let json = serde_json::to_string(&analysis).expect("serialize");
        assert!(json.contains("\"provenance\""));
        assert!(json.contains("\"allow_actuation\""));
        assert!(json.contains("\"src/lib.rs\""));
        assert!(json.contains("42"));
        let round: RuntimeAnalysis = serde_json::from_str(&json).expect("deserialize");
        let p = round.provenance.expect("provenance preserved");
        assert_eq!(p.function_name.as_deref(), Some("allow_actuation"));
        assert_eq!(p.source_file.as_deref(), Some("src/lib.rs"));
        assert_eq!(p.line, Some(42));
        assert_eq!(
            p.display_name.as_deref(),
            Some("<brakes::Controller>::allow_actuation")
        );
    }

    #[test]
    fn provenance_omitted_when_none() {
        let analysis = RuntimeAnalysis {
            matrix: IndependenceMatrix::default(),
            condition_status: BTreeMap::new(),
            provenance: None,
            ambiguous_observations: 0,
            ruled_out: BTreeMap::new(),
        };
        let json = serde_json::to_string(&analysis).expect("serialize");
        assert!(
            !json.contains("provenance"),
            "None provenance must not appear in JSON (skip_serializing_if)"
        );
    }

    #[test]
    fn old_json_without_provenance_still_deserializes() {
        // Backwards compatibility: a report from floyd 0.2.3 (no
        // provenance field) must still parse into the new shape.
        let old = r#"{
            "matrix": {
                "variant": "masking",
                "conditions": [],
                "truth_table": [],
                "independence_pairs": {}
            },
            "condition_status": {}
        }"#;
        let parsed: RuntimeAnalysis = serde_json::from_str(old).expect("parse 0.2.3 shape");
        assert!(parsed.provenance.is_none());
    }
}
