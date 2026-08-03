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
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Variant {
    /// Masking MC/DC (CAST-10 default).
    #[default]
    Masking,
    /// Strict unique-cause MC/DC.
    UniqueCause,
}

/// One row of an enumerated truth table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TruthTableRow {
    /// Assignment of each condition to its boolean value.
    pub inputs: BTreeMap<String, bool>,
    /// Resulting decision outcome under this assignment.
    pub result: bool,
}

/// A pair of truth-table rows that together demonstrate that a single
/// condition independently affects the decision outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
#[derive(Debug, Default, Clone, serde::Serialize)]
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

/// One condition assignment observed in a single test execution.
///
/// Built by upstream stages (the `runner` per ADR-0002 produces one
/// per test invocation). Floyd treats observations as opaque inputs
/// to [`analyze_with_runtime`]; how they are obtained — per-test
/// `profraw` parsing, instrumented harness, manually constructed for
/// integration tests — is the caller's responsibility.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConditionObservation {
    /// Optional test identifier, preserved for diagnostic output.
    pub test_name: Option<String>,
    /// Values observed for each named condition.
    ///
    /// Short-circuited conditions are *omitted* — only conditions
    /// actually evaluated in this test appear. Callers that wish to
    /// represent unevaluated conditions explicitly should leave them
    /// out of this map rather than inventing a value.
    pub inputs: BTreeMap<String, bool>,
    /// The decision outcome under these inputs.
    pub result: bool,
}

/// Whether the test suite as observed demonstrates MC/DC independence
/// for one condition.
#[derive(Debug, Clone, serde::Serialize)]
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
}

/// Static MC/DC analysis combined with runtime evidence: which
/// conditions had their independence demonstrated by the observed
/// test runs and which did not.
///
/// Produced by [`analyze_with_runtime`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeAnalysis {
    /// The static analysis (truth table + all valid pairs).
    pub matrix: IndependenceMatrix,
    /// Per-condition status under the observed tests.
    pub condition_status: BTreeMap<String, ConditionStatus>,
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
pub fn analyze_with_runtime(
    tree: &DecisionTree,
    observations: &[ConditionObservation],
    variant: Variant,
) -> RuntimeAnalysis {
    let matrix = analyze_with_variant(tree, variant);
    let mut condition_status = BTreeMap::new();
    for (cond, pairs) in &matrix.independence_pairs {
        condition_status.insert(cond.clone(), classify_condition(pairs, observations));
    }
    RuntimeAnalysis {
        matrix,
        condition_status,
    }
}

fn classify_condition(
    pairs: &[IndependencePair],
    observations: &[ConditionObservation],
) -> ConditionStatus {
    for pair in pairs {
        let t1 = observations
            .iter()
            .any(|o| observation_covers_row(o, &pair.test_1));
        let t2 = observations
            .iter()
            .any(|o| observation_covers_row(o, &pair.test_2));
        if t1 && t2 {
            return ConditionStatus::Exercised(pair.clone());
        }
    }
    ConditionStatus::Unexercised
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
    if observation.result != row.result {
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

/// A smallest set of test vectors that demonstrates MC/DC for every
/// condition of one decision, with the pair chosen for each condition.
///
/// Produced by [`minimum_test_set`].
///
/// The corpus pins the contract this type implements.
/// `corpus/v0/patterns/003-nested-and-or/pattern.toml` records both the
/// target — *"the theoretical n+1 minimum for n=3 conditions"* — and the
/// mechanism that reaches it: independence pairs *"chosen to overlap
/// maximally"*. Overlap is the whole difficulty. A condition typically has
/// several valid pairs, and picking each condition's pair in isolation
/// yields a test set that is valid but larger than necessary, because
/// endpoints that could have been shared are not.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct MinimumTestSet {
    /// The chosen test vectors, in truth-table order.
    pub tests: Vec<TruthTableRow>,
    /// The independence pair selected for each condition. Every endpoint
    /// of every selected pair appears in [`Self::tests`].
    ///
    /// Conditions with no valid pair are absent: they cannot be
    /// demonstrated at all, so no choice of tests covers them.
    pub chosen_pairs: BTreeMap<String, IndependencePair>,
    /// Whether minimality was *proven*, either by exhausting the
    /// selection space or by reaching the `n + 1` floor below which no
    /// valid set exists. False means [`Self::tests`] is the smallest set
    /// found within [`SEARCH_BUDGET`] and is a sound upper bound only.
    pub proven_minimal: bool,
}

/// Ceiling on branch-and-bound nodes explored by [`minimum_test_set`].
///
/// Selecting one pair per condition is a set-cover-shaped search, so the
/// worst case is exponential in the condition count. The budget keeps the
/// analysis bounded on a pathological decision; exceeding it is reported
/// through [`MinimumTestSet::proven_minimal`] rather than by silently
/// returning a number that looks proven. Pruning against the incumbent does
/// most of the work — every corpus pattern, and every three-condition
/// function, finishes well inside the ceiling.
pub const SEARCH_BUDGET: usize = 1 << 20;

/// Compute a minimum-cardinality MC/DC test set for an analysed decision.
///
/// Every condition that has at least one valid independence pair
/// contributes exactly one pair, and the result is the smallest union of
/// those pairs' endpoints. Search order puts the most constrained
/// conditions first and prunes any branch that has already matched the
/// incumbent, so the common case terminates at the `n + 1` floor without
/// enumerating the space.
///
/// Determinism: pairs are considered in [`analyze_with_variant`] order and
/// ties are broken by first discovery, so a given matrix always yields the
/// same set. Qualification evidence that changed between runs would be
/// worthless.
pub fn minimum_test_set(matrix: &IndependenceMatrix) -> MinimumTestSet {
    minimum_test_set_within(matrix, SEARCH_BUDGET)
}

/// Compute a minimum MC/DC test set under an explicit node ceiling.
///
/// Exposed so the ceiling is reachable in a test. A mechanism that only
/// ever runs with a budget nothing can exhaust is a mechanism nobody has
/// checked, and the whole point of [`MinimumTestSet::proven_minimal`] is
/// to be trustworthy in the case that does exhaust it.
pub fn minimum_test_set_within(matrix: &IndependenceMatrix, budget: usize) -> MinimumTestSet {
    let choices = condition_choices(matrix);
    let mut state = SearchState {
        floor: absolute_floor(&choices),
        budget: Budget::new(budget),
        current: Selection::default(),
        best: None,
    };
    explore(&choices, 0, &mut state);

    // `explore` records a selection as soon as one branch reaches full depth,
    // so `None` means the ceiling stopped it before any complete selection —
    // including a ceiling of zero. The default claims nothing.
    let Some(selection) = &state.best else {
        return MinimumTestSet::default();
    };
    let proven_minimal = !state.budget.exhausted || selection.rows.len() <= state.floor;
    assemble(matrix, &choices, selection, proven_minimal)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// The size below which no valid test set can exist, used to stop the
/// search once it provably cannot improve.
///
/// Two, not `n + 1`. The `n + 1` figure that
/// `corpus/v0/patterns/003-nested-and-or/pattern.toml` calls "the
/// theoretical n+1 minimum" is Chilenski's result for *singular* Boolean
/// expressions — those in which every condition appears exactly once. Every
/// v0 pattern is singular, so the two bounds coincide across the whole
/// corpus, which is what makes `n + 1` look like a law.
///
/// It is not one under masking MC/DC. Masking only requires the other
/// conditions to be *masked* in both rows of a pair, not held constant, so a
/// single pair of rows can be a valid independence pair for several
/// conditions at once, and one shared pair then demonstrates all of them.
/// The three-condition function with truth table `0x2b` needs just two
/// tests. Using `n + 1` as the floor made the search stop at four and return
/// a non-minimal set for exactly that shape.
fn absolute_floor(choices: &[ConditionChoices]) -> usize {
    if choices.is_empty() {
        return 0;
    }
    2
}

/// Candidate pairs for one condition, reduced to truth-table row indices.
///
/// Indices rather than [`TruthTableRow`] clones because the search's inner
/// loop is set membership: two conditions "share" a test exactly when they
/// name the same row.
struct ConditionChoices {
    condition: String,
    /// Each entry is `(index into the condition's pair list, the pair's
    /// two truth-table row indices)`.
    pairs: Vec<(usize, [usize; 2])>,
}

/// One pair chosen per [`ConditionChoices`] entry, with the union of every
/// chosen endpoint.
#[derive(Default, Clone)]
struct Selection {
    chosen: Vec<usize>,
    rows: BTreeSet<usize>,
}

/// Reduce a matrix to the per-condition search space, most constrained
/// first.
///
/// Ordering is a search heuristic only, not a semantic choice: a condition
/// with one valid pair forces its endpoints, so taking it first lets later
/// conditions reuse those rows and lets the bound prune earlier. Ties keep
/// the matrix's own sorted-by-name order for determinism.
fn condition_choices(matrix: &IndependenceMatrix) -> Vec<ConditionChoices> {
    let mut choices: Vec<ConditionChoices> = matrix
        .independence_pairs
        .iter()
        .filter(|(_, pairs)| !pairs.is_empty())
        .map(|(condition, pairs)| ConditionChoices {
            condition: condition.clone(),
            pairs: indexed_endpoints(&matrix.truth_table, pairs),
        })
        .filter(|entry| !entry.pairs.is_empty())
        .collect();
    choices.sort_by_key(|entry| entry.pairs.len());
    choices
}

/// Map each pair to the truth-table indices of its two endpoints.
///
/// A pair whose endpoint is not a row of this truth table is dropped
/// rather than guessed at: it cannot be part of a test set drawn from the
/// table, and inventing an index would corrupt the sharing arithmetic.
fn indexed_endpoints(
    truth_table: &[TruthTableRow],
    pairs: &[IndependencePair],
) -> Vec<(usize, [usize; 2])> {
    pairs
        .iter()
        .enumerate()
        .filter_map(|(index, pair)| {
            let first = row_position(truth_table, &pair.test_1)?;
            let second = row_position(truth_table, &pair.test_2)?;
            Some((index, [first, second]))
        })
        .collect()
}

/// Locate a row in the truth table by its condition assignment.
///
/// The assignment is the row's identity: [`enumerate_truth_table`] emits
/// one row per assignment, so a match is unique.
fn row_position(truth_table: &[TruthTableRow], row: &TruthTableRow) -> Option<usize> {
    truth_table.iter().position(|r| r.inputs == row.inputs)
}

/// Bounded node counter for the branch-and-bound search.
///
/// Remembers that it ran out rather than only reporting what is left, so
/// the caller can distinguish "searched the space" from "stopped early"
/// after the fact.
struct Budget {
    remaining: usize,
    exhausted: bool,
}

impl Budget {
    fn new(limit: usize) -> Self {
        Budget {
            remaining: limit,
            exhausted: false,
        }
    }

    /// Consume one node, or record exhaustion and refuse.
    fn consume(&mut self) -> bool {
        if self.remaining == 0 {
            self.exhausted = true;
            return false;
        }
        self.remaining -= 1;
        true
    }
}

/// Mutable state threaded through the branch-and-bound recursion.
struct SearchState {
    floor: usize,
    budget: Budget,
    current: Selection,
    best: Option<Selection>,
}

impl SearchState {
    /// True when the running selection has already matched the incumbent,
    /// so no completion of it can beat one.
    fn cannot_beat_incumbent(&self) -> bool {
        self.best
            .as_ref()
            .is_some_and(|incumbent| self.current.rows.len() >= incumbent.rows.len())
    }

    /// True once the incumbent has reached the `n + 1` floor, below which
    /// no valid selection exists — so searching on cannot improve it.
    fn reached_floor(&self) -> bool {
        self.best
            .as_ref()
            .is_some_and(|found| found.rows.len() <= self.floor)
    }

    /// Accept the running selection as the new incumbent if it is smaller.
    ///
    /// The test is repeated here even though [`Self::cannot_beat_incumbent`]
    /// already pruned this branch on entry, because that prune is an
    /// *optimisation*: disable it and an unconditional assignment here would
    /// let a later, larger selection overwrite a smaller one, and the
    /// function would quietly stop returning minima. Minimality is this
    /// type's invariant, so it is enforced where the value is stored.
    fn record(&mut self) {
        if self.cannot_beat_incumbent() {
            return;
        }
        self.best = Some(self.current.clone());
    }
}

/// Branch and bound over one pair per condition, keeping the selection
/// whose endpoint union is smallest.
fn explore(choices: &[ConditionChoices], depth: usize, state: &mut SearchState) {
    if !state.budget.consume() || state.cannot_beat_incumbent() {
        return;
    }
    let Some(entry) = choices.get(depth) else {
        state.record();
        return;
    };
    for (pair_index, endpoints) in &entry.pairs {
        try_pair(choices, depth, (*pair_index, endpoints), state);
        if state.reached_floor() {
            return;
        }
    }
}

/// Take one pair at `depth`, recurse, then undo the selection.
fn try_pair(
    choices: &[ConditionChoices],
    depth: usize,
    pair: (usize, &[usize; 2]),
    state: &mut SearchState,
) {
    let (pair_index, endpoints) = pair;
    let added = extend_rows(&mut state.current, endpoints);
    state.current.chosen.push(pair_index);
    explore(choices, depth + 1, state);
    state.current.chosen.pop();
    for row in &added {
        state.current.rows.remove(row);
    }
}

/// Add a pair's endpoints to the running selection, returning only those
/// the selection did not already contain.
///
/// The caller removes exactly this list on backtrack. Removing both
/// endpoints unconditionally would discard a row another condition still
/// depends on, which is the sharing the search exists to find.
fn extend_rows(current: &mut Selection, endpoints: &[usize; 2]) -> Vec<usize> {
    let added: Vec<usize> = endpoints
        .iter()
        .copied()
        .filter(|row| !current.rows.contains(row))
        .collect();
    current.rows.extend(added.iter().copied());
    added
}

/// Rebuild a [`MinimumTestSet`] from the winning selection.
fn assemble(
    matrix: &IndependenceMatrix,
    choices: &[ConditionChoices],
    selection: &Selection,
    proven_minimal: bool,
) -> MinimumTestSet {
    let mut chosen_pairs = BTreeMap::new();
    for (entry, pair_index) in choices.iter().zip(&selection.chosen) {
        if let Some(pair) = matrix
            .independence_pairs
            .get(&entry.condition)
            .and_then(|pairs| pairs.get(*pair_index))
        {
            chosen_pairs.insert(entry.condition.clone(), pair.clone());
        }
    }
    let tests = selection
        .rows
        .iter()
        .filter_map(|row| matrix.truth_table.get(*row).cloned())
        .collect();
    MinimumTestSet {
        tests,
        chosen_pairs,
        proven_minimal,
    }
}

/// Collect the set of atomic condition names referenced in a [`Node`],
/// returned in deterministic sorted order.
fn collect_conditions(node: &Node) -> Vec<String> {
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
        ConditionObservation {
            test_name: Some(name.to_string()),
            inputs: inputs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            result,
        }
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

    // -----------------------------------------------------------------------
    // Minimum test set
    // -----------------------------------------------------------------------

    /// Re-check a candidate test set with the runtime checker, so the
    /// oracle is [`analyze_with_runtime`] rather than this module's own
    /// arithmetic. A set that does not exercise every condition is not a
    /// test set at all, however small it is.
    fn exercises_every_condition(tree: &DecisionTree, set: &MinimumTestSet) -> bool {
        let observations: Vec<ConditionObservation> = set
            .tests
            .iter()
            .map(|row| ConditionObservation {
                test_name: None,
                inputs: row.inputs.clone(),
                result: row.result,
            })
            .collect();
        let analysis = analyze_with_runtime(tree, &observations, Variant::Masking);
        analysis
            .matrix
            .conditions
            .iter()
            .all(|c| matches!(analysis.condition_status[c], ConditionStatus::Exercised(_)))
    }

    #[test]
    fn and_minimum_test_set_matches_corpus_001() {
        let tree = and_tree();
        let set = minimum_test_set(&analyze(&tree));
        // Corpus 001 declares a 3-test minimum set: n + 1 for n = 2.
        assert_eq!(set.tests.len(), 3, "set={:?}", set.tests);
        assert!(set.proven_minimal);
        assert!(exercises_every_condition(&tree, &set));
    }

    #[test]
    fn or_minimum_test_set_matches_corpus_002() {
        let tree = or_tree();
        let set = minimum_test_set(&analyze(&tree));
        assert_eq!(set.tests.len(), 3, "set={:?}", set.tests);
        assert!(set.proven_minimal);
        assert!(exercises_every_condition(&tree, &set));
    }

    #[test]
    fn nested_minimum_test_set_matches_corpus_003() {
        let tree = nested_tree();
        let set = minimum_test_set(&analyze(&tree));
        // `corpus/v0/patterns/003-nested-and-or/pattern.toml` pins four
        // vectors, "the theoretical n+1 minimum for n=3 conditions".
        assert_eq!(set.tests.len(), 4, "set={:?}", set.tests);
        assert!(set.proven_minimal);
        assert!(exercises_every_condition(&tree, &set));
    }

    #[test]
    fn minimum_test_set_shares_endpoints_instead_of_taking_each_first_pair() {
        // The defect this pins: selecting every condition's first valid
        // pair independently is valid but not minimal. On corpus 003 it
        // yields five vectors, because `c`'s first pair — (F,F,F)/(F,F,T) —
        // shares no endpoint with `a`'s or `b`'s, while (T,F,F)/(T,F,T)
        // reuses the vector `b` already needs.
        let matrix = analyze(&nested_tree());
        let independent: BTreeSet<&BTreeMap<String, bool>> = matrix
            .independence_pairs
            .values()
            .flat_map(|pairs| pairs.first())
            .flat_map(|p| [&p.test_1.inputs, &p.test_2.inputs])
            .collect();
        let set = minimum_test_set(&matrix);
        assert_eq!(independent.len(), 5, "first-pair union should be 5");
        assert!(
            set.tests.len() < independent.len(),
            "minimum {} should beat first-pair union {}",
            set.tests.len(),
            independent.len()
        );
    }

    #[test]
    fn minimum_test_set_reports_a_pair_for_every_covered_condition() {
        for tree in [and_tree(), or_tree(), nested_tree()] {
            let matrix = analyze(&tree);
            let set = minimum_test_set(&matrix);
            for cond in &matrix.conditions {
                let pair = &set.chosen_pairs[cond];
                for row in [&pair.test_1, &pair.test_2] {
                    assert!(
                        set.tests.iter().any(|t| t.inputs == row.inputs),
                        "{cond}: chosen endpoint {:?} absent from the test set",
                        row.inputs
                    );
                }
            }
        }
    }

    #[test]
    fn an_exhausted_budget_reports_an_upper_bound_instead_of_claiming_a_minimum() {
        // Sweep ceilings from "cannot finish one selection" upward. Some
        // ceiling in this range stops the search after it has a complete
        // selection but before it can rule out a smaller one; that is the
        // state `proven_minimal = false` exists to describe, and it is
        // unreachable at the production ceiling.
        let tree = nested_tree();
        let matrix = analyze(&tree);
        let unproven: Vec<MinimumTestSet> = (1..64)
            .map(|ceiling| minimum_test_set_within(&matrix, ceiling))
            .filter(|set| !set.proven_minimal && !set.tests.is_empty())
            .collect();
        assert!(
            !unproven.is_empty(),
            "no ceiling in 1..64 produced an unproven non-empty set"
        );
        let proven = minimum_test_set(&matrix).tests.len();
        for set in &unproven {
            // An unproven set is still a *valid* set: `best` is only ever
            // recorded at full depth, so every condition has a chosen pair.
            // It is an upper bound, so never smaller than the real minimum.
            assert!(set.tests.len() >= proven);
            assert!(exercises_every_condition(&tree, set));
        }
    }

    #[test]
    fn a_zero_budget_returns_nothing_and_claims_nothing() {
        let set = minimum_test_set_within(&analyze(&nested_tree()), 0);
        assert!(set.tests.is_empty());
        assert!(!set.proven_minimal, "an unsearched space is not proven");
    }

    #[test]
    fn the_production_budget_proves_every_corpus_pattern() {
        // Pins the claim in `SEARCH_BUDGET`'s own documentation: the floor
        // terminates these searches, the ceiling never does.
        for tree in [and_tree(), or_tree(), nested_tree()] {
            assert!(minimum_test_set(&analyze(&tree)).proven_minimal);
        }
    }

    /// Build the decision computing the boolean function whose truth table
    /// is the bits of `mask`, as a complete `Ite` tree over `a`, `b`, `c`.
    ///
    /// Bit `i` of `mask` is the result for the assignment encoding `i` the
    /// same way [`enumerate_truth_table`] does: `a` is bit 0, `b` bit 1,
    /// `c` bit 2. Sweeping `mask` over `0..256` therefore sweeps every
    /// three-condition boolean function.
    fn function_of_three(mask: u32) -> DecisionTree {
        let leaf = |a: u32, b: u32, c: u32| const_(mask >> (a | (b << 1) | (c << 2)) & 1 == 1);
        let branch_on_c = |a: u32, b: u32| ite("c", leaf(a, b, 1), leaf(a, b, 0));
        let branch_on_b = |a: u32| ite("b", branch_on_c(a, 1), branch_on_c(a, 0));
        DecisionTree {
            decisions: vec![ite("a", branch_on_b(1), branch_on_b(0))],
        }
    }

    /// Smallest test set found by exhaustive enumeration of row subsets.
    ///
    /// Deliberately the dumbest correct algorithm: it shares no code with
    /// [`minimum_test_set`]'s branch and bound, so agreement between the two
    /// is evidence rather than a tautology. Only viable because a decision
    /// with `n` conditions has `2^n` rows, hence `2^(2^n)` subsets.
    fn brute_force_minimum(matrix: &IndependenceMatrix) -> Option<usize> {
        let rows = matrix.truth_table.len();
        let covered: Vec<&Vec<IndependencePair>> = matrix
            .independence_pairs
            .values()
            .filter(|pairs| !pairs.is_empty())
            .collect();
        (0..(1u32 << rows))
            .filter(|subset| {
                covered.iter().all(|pairs| {
                    pairs.iter().any(|pair| {
                        [&pair.test_1, &pair.test_2].iter().all(|row| {
                            row_position(&matrix.truth_table, row)
                                .is_some_and(|index| subset >> index & 1 == 1)
                        })
                    })
                })
            })
            .map(|subset| subset.count_ones() as usize)
            .min()
    }

    #[test]
    fn branch_and_bound_agrees_with_brute_force_on_every_three_condition_function() {
        // 256 functions, each cross-checked against an independent
        // exhaustive search. Catches any pruning or early-exit rule that is
        // sound on the corpus shapes but wrong in general.
        let mut checked = 0;
        for mask in 0..256u32 {
            let tree = function_of_three(mask);
            let matrix = analyze(&tree);
            let set = minimum_test_set(&matrix);
            let Some(expected) = brute_force_minimum(&matrix) else {
                continue;
            };
            assert!(set.proven_minimal, "mask {mask:#04x}: not proven");
            assert_eq!(
                set.tests.len(),
                expected,
                "mask {mask:#04x}: branch and bound gave {}, brute force {expected}",
                set.tests.len()
            );
            // Only meaningful when every condition is demonstrable at all.
            // A constant function (mask 0x00, 0xff) still reports three
            // conditions, because the `Ite` tree branches on them, yet no
            // pair exists for any of them — there is nothing to exercise.
            let all_demonstrable = matrix
                .conditions
                .iter()
                .all(|c| !matrix.independence_pairs[c].is_empty());
            if all_demonstrable {
                assert!(exercises_every_condition(&tree, &set), "mask {mask:#04x}");
            }
            checked += 1;
        }
        assert!(checked > 200, "only {checked} functions were checkable");
    }

    #[test]
    fn a_decision_with_no_conditions_yields_an_empty_proven_set() {
        let tree = DecisionTree {
            decisions: vec![const_(true)],
        };
        let set = minimum_test_set(&analyze(&tree));
        assert!(set.tests.is_empty());
        assert!(set.chosen_pairs.is_empty());
        // Vacuously minimal: there is no condition to demonstrate, so no
        // test can shrink the set further.
        assert!(set.proven_minimal);
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
}
