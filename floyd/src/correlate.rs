//! Correlate static MIR decisions with runtime coverage data.
//!
//! Per [ADR-0002]'s resolution of open question Q4, the runtime
//! correlation Floyd needs is a source-span join: every atomic
//! condition referenced in the MIR is also a `branches` entry in
//! `llvm-cov export` output, and both sides carry the same
//! `(file, line:col)` span. The MIR side tells us *which* conditions
//! belong to *which* decision (and how they compose into an ITE);
//! the runtime side tells us how many times each condition evaluated
//! true / false.
//!
//! This module performs that join, producing a [`DecisionMap`] that
//! pairs every MIR function with the runtime counts for its atomic
//! conditions, keyed by source name.
//!
//! ## Algorithm
//!
//! 1. For each [`crate::mir::MirFunction`], walk its blocks and
//!    collect a `name -> SourceSpan` map. `switchInt` terminators
//!    contribute their `discr` local's name (the LHS condition being
//!    *tested*) at the terminator's span; `AssignCopy` statements
//!    whose source local has a `debug` name contribute that name at
//!    the statement's span (the RHS condition being *read*). If a
//!    name appears in multiple places, the first occurrence wins.
//! 2. Find the function's coverage record by **source span**: the
//!    [`CoverageIndex`] keys every `llvm-cov` record by its
//!    normalised code and branch spans, merges the records of one
//!    source function (generic monomorphisations) by summing their
//!    counters, and returns the one function whose spans contain the
//!    decision's condition spans (and, when the MIR came from the
//!    instrumented compilation, the body's own coverage-mapping
//!    spans). Function names are never compared:
//!    MIR names impl methods `<impl at FILE:L:C: L:C>::m` and trims
//!    paths, coverage names monomorphised symbols, and neither can
//!    be reconciled with the other.
//! 3. For each `(condition_name, span)` pair, take the merged
//!    function's branch region at exactly that span and record its
//!    `true_count` / `false_count`.
//!
//! Spans compare structurally after [`PathNormaliser`] has put both
//! files in canonical form: same file, same start/end line, same
//! start/end column.
//!
//! [ADR-0002]: ../../../architecture/decisions/0002-runtime-pipeline.md

use crate::coverage_index::{CoverageIndex, FunctionId, FunctionMatch, PathNormaliser};
use crate::decision::{self, DecisionTree, Node};
use crate::masking::{BranchCounts, ConditionObservation, Resolution};
use crate::mir::{Mir, MirFunction, MirStatement, MirTerminator, SourceSpan};
use crate::profile::CoverageReport;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The result of joining MIR decision structure with runtime
/// coverage data.
///
/// Carries one [`FunctionRuntime`] per MIR function. Functions for
/// which no matching coverage entry was found are still present;
/// their `conditions` map is empty.
#[derive(Debug, Default, Clone)]
pub struct DecisionMap {
    /// Per-function runtime data, in MIR-declaration order.
    pub functions: Vec<FunctionRuntime>,
}

/// Runtime coverage data for one function's atomic conditions.
#[derive(Debug, Clone)]
pub struct FunctionRuntime {
    /// Function name as it appears in the MIR (unmangled).
    pub name: String,
    /// Per-condition runtime data, keyed by source name.
    pub conditions: BTreeMap<String, ConditionRuntime>,
}

/// Runtime counts for one atomic condition, joined to its MIR source
/// location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionRuntime {
    /// The source span where this condition appears in the program
    /// (file in [`PathNormaliser`] canonical form).
    pub span: SourceSpan,
    /// Number of times the condition evaluated true across the runs
    /// represented by the [`CoverageReport`].
    pub true_count: u64,
    /// Number of times the condition evaluated false.
    pub false_count: u64,
}

/// Join MIR decisions to runtime coverage data by source span.
///
/// Each MIR function is looked up in a [`CoverageIndex`] of
/// `coverage` by all of its condition spans; conditions are then
/// joined to the found function's branch regions by exact
/// (normalised) span equality. A function with no coverage record,
/// or whose spans are ambiguous, gets an empty `conditions` map:
/// safe failure rather than wrong data. `paths` must be built with
/// the root rustc ran in, so relative MIR paths and absolute
/// `llvm-cov` paths normalise to the same string.
pub fn correlate(mir: &Mir, coverage: &CoverageReport, paths: &PathNormaliser) -> DecisionMap {
    let index = CoverageIndex::build(coverage, paths);
    let prepared = prepare_mir(mir, paths);
    let mut map = DecisionMap::default();
    for (f, spans) in mir.functions.iter().zip(&prepared.function_spans) {
        let mut conditions = BTreeMap::new();
        if let FunctionMatch::Found(cov_fn) = index.resolve(spans.values(), Some(&f.name)) {
            for (name, span) in spans {
                if let Some(counts) = cov_fn.branch_counts(span) {
                    conditions.insert(
                        name.clone(),
                        ConditionRuntime {
                            span: span.clone(),
                            true_count: counts.true_count,
                            false_count: counts.false_count,
                        },
                    );
                }
            }
        }
        map.functions.push(FunctionRuntime {
            name: f.name.clone(),
            conditions,
        });
    }
    map
}

/// The condition a decision tests first: the `cond` of its root
/// [`Node::Ite`], or the leaf of a bare [`Node::Condition`].
///
/// An ITE tree is evaluated from its root, so this condition is
/// tested on every entry into the decision and is never
/// short-circuited. Its `true_count + false_count` is therefore the
/// number of times the decision was evaluated. `None` for a
/// constant decision, which tests no condition.
fn root_condition(node: &Node) -> Option<&str> {
    match node {
        Node::Ite { cond, .. } => Some(cond),
        Node::Condition { name } => Some(name),
        _ => None,
    }
}

/// Reduce one test's aggregate per-condition counters for one
/// decision to the observations those counters prove occurred.
///
/// `counts` maps condition names to the test's `llvm-cov` branch
/// counters. Those counters are TOTALS over every evaluation of the
/// decision in the test, and each condition is counted on its own,
/// so they do not record which values occurred *together*. A test
/// that runs `a && b` as (T,T) and then (F,-) leaves `a = [1 T, 1 F]`,
/// `b = [1 T, 0 F]`; collapsing each condition independently gives
/// one vector, (T,T), and silently drops the (F,-) evaluation. Worse,
/// for some decisions the collapsed vector never occurred at all.
/// Floyd therefore only derives a vector when the counters determine
/// it.
///
/// # Evaluation count
///
/// The number of times the test evaluated the decision, `N`, is the
/// root condition's `true_count + false_count`. The root (the `cond`
/// of the outermost [`Node::Ite`], or the leaf of a bare
/// [`Node::Condition`]) is tested on every entry into the decision
/// and is never short-circuited. This is the
/// primary signal. `function_count`
/// ([`crate::profile::FunctionCoverage::count`], when known) counts entries into the
/// enclosing function, not evaluations of the decision: a function
/// that evaluates the decision inside a loop has `N` greater than its
/// entry count, and a decision behind an early `?` return has `N`
/// smaller. It is used only as a consistency check (see rule 0).
///
/// # Rules, in order
///
/// 0. **Consistent counters.** Every condition of the decision is
///    tested at most once per evaluation, so its total must not
///    exceed `N`; and a decision evaluated `N > 0` times means its
///    function was entered, so `function_count` must not be
///    `Some(0)`. Inconsistent counters cannot be attributed and the
///    test is recorded as ambiguous.
/// 1. **Single evaluation.** `N == 1`: each evaluated condition has
///    exactly one non-zero counter, which is its value. One vector.
/// 2. **Uniform repeated evaluations.** `N > 1` but every condition
///    was only ever true or only ever false in this test. The ITE
///    path is fully determined by the condition values, so all `N`
///    evaluations followed the same path with the same values. One
///    vector (the same one rule 1 would derive).
/// 3. **Single-condition decision.** A decision over one condition
///    that was both true and false yields two vectors, `{c: T}` and
///    `{c: F}`, however many evaluations there were: with one
///    condition the vector is the condition's value, so each
///    non-zero counter proves its vector occurred.
/// 4. **Tree counters.** `N > 1`, any number of two-sided
///    conditions, the decision's [`Node`] tree is a tree of
///    distinct counters (every condition sits at exactly one node,
///    and no two conditions share a counter), and the counters are
///    flow-consistent over that tree. One vector per leaf the
///    evaluations reached, each with `evaluations` equal to the
///    number of evaluations that reached that leaf and
///    `resolved_by: tree_counters` (see "Why rule 4 is sound").
/// 5. **One two-sided condition.** `N > 1`, exactly one condition
///    was both true and false, every other condition was one-sided,
///    and the counters account exactly for one evaluation path per
///    value of the two-sided condition. Two vectors, one per value
///    (see "Why rule 5 is sound" below for the proof and the exact
///    consistency check), each with `evaluations: N` and
///    `resolved_by: single_two_sided`. Tried only when rule 4 does
///    not apply (a decision that is not a tree, two conditions on
///    one counter, or counters that do not fit the tree): where
///    both apply they give the same vectors, but rule 4 gives each
///    vector its own evaluation count and its checks are the
///    stricter.
/// 6. Otherwise the test is **ambiguous**: recorded with its
///    evaluation count and raw counters, no vector, no result.
///
/// # Why rule 4 is sound
///
/// Read the decision's [`Node`] tree as a flow graph: each
/// [`Node::Ite`] and each [`Node::Condition`] leaf is a node with a
/// true edge and a false edge (an `Ite`'s edges lead to its
/// `then_branch` / `else_branch`; a `Condition` leaf's edges lead to
/// the outcomes `true` / `false`), and each [`Node::Const`] is an
/// outcome leaf. Rule 4 first checks that this graph is a **tree of
/// distinct counters**:
///
/// - every condition name occurs at exactly one node. The
///   decomposer gives a MIR block reached from two predecessors (a
///   condition the source can reach along two different walks, like
///   `c` in `(a || b) && c`) one subtree per predecessor, with the
///   same name; and counters are joined per name, through the
///   name's span. A repeated name is therefore one counter shared by
///   two nodes, and the check rejects it;
/// - no two condition names were joined to the same span (the
///   caller says so: [`join_test`] compares the decision's spans;
///   hand-built counters are distinct by construction).
///
/// So each node `v` has its own counter `(t_v, f_v)`, and since the
/// nodes form a tree, each node other than the root has exactly one
/// incoming edge (from its parent `u`, the edge `u`'s value `x`
/// selects). Every evaluation walks from the root to one leaf,
/// testing each condition on its walk once (a condition occurs at
/// one node only). Let `in(v)` be the number of the test's
/// evaluations that reach `v`. Then:
///
/// - `in(root) = N`, by definition of `N`;
/// - an evaluation reaching `v` tests `v`'s condition once and
///   leaves along exactly one edge, and `v`'s counter counts exactly
///   the evaluations that tested `v` (no other node shares it), so
///   `t_v` evaluations take `v`'s true edge, `f_v` its false edge,
///   and `t_v + f_v = in(v)`;
/// - an evaluation reaches a non-root `v` only along `v`'s one
///   incoming edge, so `in(v)` is `t_u` or `f_u` of its parent;
/// - an evaluation reaches leaf `l` only along `l`'s one incoming
///   edge, so the number of evaluations that end at `l` is that
///   edge's flow.
///
/// Each leaf `l` is reached by one root-to-leaf walk, which fixes
/// the value of every condition on it and leaves every other
/// condition short-circuited: one vector, with `l`'s outcome. The
/// vectors of distinct leaves differ (they part at some node). So
/// the counters determine the full multiset of vectors: leaf `l`'s
/// vector occurred exactly `flow(l)` times, and no other vector
/// occurred. The rule emits every leaf with a non-zero flow,
/// computing each flow top-down from the root's `N` with the edge
/// counters of the tree (never by matching names to positions).
///
/// The rule fires only when the counters fit this model: for every
/// node, `t_v + f_v` equals the flow of the edge into it (`N` at the
/// root), a node whose inflow is non-zero has a counter (a node
/// with no counter and no inflow is unreached), and the leaf flows
/// sum to `N` (implied by the node checks; checked anyway). A
/// violation means the counters were not produced by this tree
/// (a missing or merged region, a decomposition that differs from
/// the compiled code) and the test falls through to rule 5.
///
/// On a graph that is not a tree the counters need not determine
/// the vectors: in `(a || b) && c`, `c` is reached from `a = T` and
/// from `a = F, b = T`, and (T,-,T) + (F,T,F) leaves the same
/// counters (a = [1 T, 1 F], b = [1 T, 0 F], c = [1 T, 1 F]) as
/// (T,-,F) + (F,T,T), though the two pairs of vectors have their
/// `true` outcome on different walks. Such tests stay ambiguous
/// unless rule 5 applies. So do tests of `(a && b) || c`, where
/// (T,F,T) + (F,-,F) and (T,F,F) + (F,-,T) leave the same counters.
///
/// # Why rule 5 is sound
///
/// Let `p` be the only condition with both counters non-zero
/// (`p = [k_T T, k_F F]`) and let every other condition `d` with a
/// non-zero counter be one-sided with value `v_d`. Each evaluation
/// walks the decision tree from the root. Whenever it tests a
/// condition `d != p`, `d` takes `v_d` (that is the only value it ever
/// took in this test); whenever it tests `p`, `p` takes `T` or `F`.
/// A condition with no evaluations in this test is never tested.
/// So the walk is a function of `p`'s value alone (and of nothing if
/// it never reaches `p`): there are at most two possible evaluation
/// vectors, `P_T` (walk with `p = T`) and `P_F` (walk with `p = F`).
/// Every evaluation that reaches `p` with value `x` is exactly `P_x`.
///
/// The rule only fires when the counters are *exactly* those of
/// `k_T` evaluations of `P_T` plus `k_F` evaluations of `P_F`:
///
/// - `p` is on both walks, once each, and `k_T + k_F = N`, so every
///   evaluation reached `p` (none took a `p`-free path: the walk
///   before `p` is the same for all evaluations, so either all reach
///   `p` or none does);
/// - no walk reaches a condition that has no evaluations in this
///   test (such a walk could not have happened);
/// - no condition is tested twice on one walk;
/// - for every condition `d`, its counter equals the number of
///   times the two walks, weighted by `k_T` and `k_F`, test it with
///   each value; conditions on neither walk have zero counters.
///
/// Under those checks, `P_T` occurred `k_T > 0` times and `P_F`
/// occurred `k_F > 0` times, and nothing else occurred, so both
/// vectors are sound and complete. The outcome of each comes from
/// the tree. Short-circuited conditions are simply off a walk: a
/// condition only on `P_T` has a counter total of `k_T`, below `N`.
/// When any check fails, the test is ambiguous (rule 6). With two
/// or more two-sided conditions the walks are no longer a function
/// of one value, so rule 5 does not apply; such a test is resolved
/// only by rule 4, when the decision is a tree.
///
/// `N == 0` (the test never evaluated the decision) or a root with
/// no counters yields no observation at all. Condition names in
/// `counts` that the decision does not reference are ignored.
///
/// Each name in `counts` is taken to be its own counter; [`join_test`]
/// additionally withholds rule 4 when two of the decision's
/// conditions were joined to one span.
pub fn observations_from_counts(
    test_name: Option<String>,
    decision: &Node,
    counts: &BTreeMap<String, BranchCounts>,
    function_count: Option<u64>,
) -> Vec<ConditionObservation> {
    reduce_counts(test_name, decision, counts, function_count, true)
}

/// [`observations_from_counts`], with `distinct_counters` false when
/// two of the decision's conditions share one counter (rule 4 then
/// does not apply).
fn reduce_counts(
    test_name: Option<String>,
    decision: &Node,
    counts: &BTreeMap<String, BranchCounts>,
    function_count: Option<u64>,
    distinct_counters: bool,
) -> Vec<ConditionObservation> {
    let Some(root) = root_condition(decision) else {
        return Vec::new();
    };
    let Some(evaluations) = counts.get(root).map(BranchCounts::total) else {
        return Vec::new();
    };
    if evaluations == 0 {
        return Vec::new();
    }
    let conditions = crate::masking::collect_conditions(decision);
    let decision_counts: BTreeMap<String, BranchCounts> = conditions
        .iter()
        .filter_map(|c| counts.get(c).map(|n| (c.clone(), *n)))
        .collect();

    // Rule 0: counters that cannot describe `evaluations`
    // evaluations of this decision are not attributable.
    let consistent =
        decision_counts.values().all(|c| c.total() <= evaluations) && function_count != Some(0);
    if !consistent {
        return vec![ConditionObservation::ambiguous(
            test_name,
            evaluations,
            decision_counts,
        )];
    }

    // Rules 1 and 2: one vector. With `evaluations == 1` every
    // condition is one-sided, so rule 1 is the `N == 1` case of
    // rule 2; both are spelled out for the reader.
    let one_sided = |c: &BranchCounts| c.true_count == 0 || c.false_count == 0;
    if evaluations == 1 || decision_counts.values().all(one_sided) {
        let inputs: BTreeMap<String, bool> = decision_counts
            .iter()
            .filter(|(_, c)| c.total() > 0)
            .map(|(name, c)| (name.clone(), c.true_count > 0))
            .collect();
        return vector_observation(test_name, decision, inputs, evaluations)
            .into_iter()
            .collect();
    }

    // Rule 3.
    if conditions.len() == 1 {
        return single_condition_rule(test_name, decision, root, evaluations);
    }

    // Rule 4: tried first because, where it applies, it is exact
    // per vector (each leaf's own flow), not `N` on every vector.
    if distinct_counters {
        if let Some(obs) = tree_counters_rule(&test_name, decision, &decision_counts, evaluations) {
            return obs;
        }
    }

    // Rule 5: the fallback for decisions that are not trees (or
    // whose counters are shared).
    if let Some(obs) = one_two_sided_rule(&test_name, decision, &decision_counts, evaluations) {
        return obs;
    }

    // Rule 6.
    vec![ConditionObservation::ambiguous(
        test_name,
        evaluations,
        decision_counts,
    )]
}

/// Single-condition rule: a decision over ONE condition that the
/// test evaluated both true and false (in any number of
/// evaluations, e.g. in a loop) yields two vectors, `{c: T}` and
/// `{c: F}`.
///
/// This is sound where the general case is not: with one condition
/// the vector *is* the condition's value, so `true_count > 0` proves
/// that `{c: T}` occurred and `false_count > 0` proves that `{c: F}`
/// occurred. No combination of values across conditions has to be
/// guessed. Called only when both counters are non-zero (the
/// one-sided case is rule 2 of [`observations_from_counts`]).
fn single_condition_rule(
    test_name: Option<String>,
    decision: &Node,
    condition: &str,
    evaluations: u64,
) -> Vec<ConditionObservation> {
    [true, false]
        .into_iter()
        .filter_map(|value| {
            let inputs = BTreeMap::from([(condition.to_string(), value)]);
            vector_observation(test_name.clone(), decision, inputs, evaluations)
        })
        .collect()
}

/// Rule 5 of [`observations_from_counts`]: exactly one condition is
/// two-sided, so the counters determine every evaluation. See "Why
/// rule 5 is sound" on [`observations_from_counts`]; this function
/// performs every check listed there and returns `None` (the test
/// stays ambiguous) when one fails.
fn one_two_sided_rule(
    test_name: &Option<String>,
    decision: &Node,
    counts: &BTreeMap<String, BranchCounts>,
    evaluations: u64,
) -> Option<Vec<ConditionObservation>> {
    let two_sided = |c: &&BranchCounts| c.true_count > 0 && c.false_count > 0;
    let mut pivots = counts.iter().filter(|(_, c)| two_sided(c));
    let (pivot, pivot_counts) = pivots.next()?;
    if pivots.next().is_some() || pivot_counts.total() != evaluations {
        return None;
    }
    let fixed = |name: &str| -> Option<bool> {
        if name == pivot {
            return None;
        }
        counts
            .get(name)
            .filter(|c| c.total() > 0)
            .map(|c| c.true_count > 0)
    };
    let mut expected: BTreeMap<&str, BranchCounts> = BTreeMap::new();
    let mut out = Vec::new();
    for (value, times) in [
        (true, pivot_counts.true_count),
        (false, pivot_counts.false_count),
    ] {
        let (path, result) = crate::masking::evaluation_walk(decision, |name| {
            if name == pivot {
                Some(value)
            } else {
                fixed(name)
            }
        })?;
        let mut inputs = BTreeMap::new();
        for (name, v) in &path {
            if inputs.insert(name.clone(), *v).is_some() {
                return None; // tested twice on one walk
            }
            let e = expected
                .entry(counts.get_key_value(name)?.0.as_str())
                .or_default();
            if *v {
                e.true_count += times;
            } else {
                e.false_count += times;
            }
        }
        if !inputs.contains_key(pivot) {
            return None;
        }
        let mut obs = ConditionObservation::vector(test_name.clone(), inputs, result);
        obs.evaluations = Some(evaluations);
        obs.resolved_by = Some(Resolution::SingleTwoSided);
        out.push(obs);
    }
    let exact = counts.iter().all(|(name, c)| {
        expected
            .get(name.as_str())
            .map_or(c.total() == 0, |e| e == c)
    });
    exact.then_some(out)
}

/// Rule 4 of [`observations_from_counts`]: the decision is a tree
/// of distinct counters, so the counters are the flows on its edges
/// and every leaf's inflow is the number of evaluations that took
/// its vector. See "Why rule 4 is sound" on
/// [`observations_from_counts`]; this function checks the tree shape
/// and every flow condition listed there, and returns `None` (the
/// test falls through to rule 5) when one fails. The caller has
/// checked that no two condition names share a counter.
fn tree_counters_rule(
    test_name: &Option<String>,
    decision: &Node,
    counts: &BTreeMap<String, BranchCounts>,
    evaluations: u64,
) -> Option<Vec<ConditionObservation>> {
    if !is_tree(decision) {
        return None;
    }
    let mut leaves = Vec::new();
    tree_flows(decision, evaluations, counts, &mut Vec::new(), &mut leaves)?;
    if leaves.iter().map(|(_, _, k)| k).sum::<u64>() != evaluations {
        return None;
    }
    Some(
        leaves
            .into_iter()
            .map(|(path, result, k)| {
                let mut obs = ConditionObservation::vector(
                    test_name.clone(),
                    path.into_iter().collect(),
                    result,
                );
                obs.evaluations = Some(k);
                obs.resolved_by = Some(Resolution::TreeCounters);
                obs
            })
            .collect(),
    )
}

/// True iff every condition of `decision` occurs at exactly one
/// node: its [`Node`] tree is then a tree of conditions, each with
/// one incoming edge, rather than a DAG unfolded into one subtree
/// per predecessor (which repeats the shared condition's name).
fn is_tree(decision: &Node) -> bool {
    fn walk<'n>(node: &'n Node, seen: &mut BTreeSet<&'n str>) -> bool {
        match node {
            Node::Const { .. } => true,
            Node::Condition { name } => seen.insert(name),
            Node::Ite {
                cond,
                then_branch,
                else_branch,
            } => seen.insert(cond) && walk(then_branch, seen) && walk(else_branch, seen),
        }
    }
    walk(decision, &mut BTreeSet::new())
}

/// A leaf of a decision tree some evaluations reached: the walk from
/// the root (each condition tested, with its value), the outcome, and
/// the number of evaluations that took it.
type Leaf = (Vec<(String, bool)>, bool, u64);

/// Push every leaf below `node` that `inflow > 0` evaluations reach,
/// as (walk from the root, outcome, flow), depth first, true edge
/// first. `path` is the walk from the root to `node`. `None` if a
/// node's counter does not total its inflow, or a node with inflow
/// has no counter.
fn tree_flows(
    node: &Node,
    inflow: u64,
    counts: &BTreeMap<String, BranchCounts>,
    path: &mut Vec<(String, bool)>,
    leaves: &mut Vec<Leaf>,
) -> Option<()> {
    let counter = |name: &str| -> Option<BranchCounts> {
        match counts.get(name) {
            Some(c) if c.total() == inflow => Some(*c),
            None if inflow == 0 => Some(BranchCounts::default()),
            _ => None,
        }
    };
    match node {
        Node::Const { value } => {
            if inflow > 0 {
                leaves.push((path.clone(), *value, inflow));
            }
        }
        Node::Condition { name } => {
            let c = counter(name)?;
            for (value, flow) in [(true, c.true_count), (false, c.false_count)] {
                if flow > 0 {
                    let mut walk = path.clone();
                    walk.push((name.clone(), value));
                    leaves.push((walk, value, flow));
                }
            }
        }
        Node::Ite {
            cond,
            then_branch,
            else_branch,
        } => {
            let c = counter(cond)?;
            for (value, branch, flow) in [
                (true, then_branch, c.true_count),
                (false, else_branch, c.false_count),
            ] {
                path.push((cond.clone(), value));
                let ok = tree_flows(branch, flow, counts, path, leaves);
                path.pop();
                ok?;
            }
        }
    }
    Some(())
}

/// A condition-vector observation whose result is evaluated from
/// `inputs`. `None` if the decision needs a condition missing from
/// `inputs` (inconsistent counters).
fn vector_observation(
    test_name: Option<String>,
    decision: &Node,
    inputs: BTreeMap<String, bool>,
    evaluations: u64,
) -> Option<ConditionObservation> {
    let result = decision::evaluate_partial(decision, &inputs)?;
    let mut obs = ConditionObservation::vector(test_name, inputs, result);
    obs.evaluations = Some(evaluations);
    Some(obs)
}

/// A MIR with per-function condition-span maps pre-computed.
///
/// Pass 3 joins every candidate against every test's coverage;
/// rebuilding `collect_condition_spans` on each call would walk the
/// same function `candidates × tests` times. `PreparedMir` builds
/// every function's span map ONCE, with each span's file already in
/// [`PathNormaliser`] canonical form, so Pass 3 is just lookup +
/// join against runtime counts.
pub struct PreparedMir<'m> {
    /// The MIR itself. Borrowed to keep the pre-computation cost
    /// out of ownership.
    pub mir: &'m Mir,
    /// Per function, in `mir.functions` order: condition name →
    /// normalised [`SourceSpan`]. Conditions whose span is in
    /// `/rustc/` standard-library code are left out; they can never
    /// join.
    pub function_spans: Vec<BTreeMap<String, SourceSpan>>,
    /// Per function: the normalised spans of the body's coverage
    /// mappings ([`MirFunction::coverage_spans`]); empty when the
    /// MIR does not carry them.
    pub mapping_spans: Vec<Vec<SourceSpan>>,
}

/// Build a [`PreparedMir`] for the given MIR, computing each
/// function's normalised condition-span map once up-front.
pub fn prepare_mir<'m>(mir: &'m Mir, paths: &PathNormaliser) -> PreparedMir<'m> {
    let function_spans = mir
        .functions
        .iter()
        .map(|f| {
            collect_condition_spans(f)
                .into_iter()
                .filter_map(|(name, span)| Some((name, paths.span(&span)?)))
                .collect()
        })
        .collect();
    let mapping_spans = mir
        .functions
        .iter()
        .map(|f| {
            f.coverage_spans
                .iter()
                .filter_map(|s| paths.span(s))
                .collect()
        })
        .collect();
    PreparedMir {
        mir,
        function_spans,
        mapping_spans,
    }
}

impl PreparedMir<'_> {
    /// The normalised spans of the conditions the first decision of
    /// `tree` tests, for function `fn_idx`. These, not every named
    /// local in the body, locate the decision's coverage record.
    pub fn decision_spans(&self, fn_idx: usize, tree: &DecisionTree) -> Vec<SourceSpan> {
        let (Some(spans), Some(decision)) =
            (self.function_spans.get(fn_idx), tree.decisions.first())
        else {
            return Vec::new();
        };
        let mut out: Vec<SourceSpan> = crate::masking::collect_conditions(decision)
            .iter()
            .filter_map(|c| spans.get(c).cloned())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The spans that locate function `fn_idx`'s coverage record:
    /// its decision spans plus its coverage-mapping spans. The
    /// mapping spans are the record's own regions, so they find the
    /// record even when a decomposed condition sits at a span with
    /// no region.
    pub fn lookup_spans(&self, fn_idx: usize, tree: &DecisionTree) -> Vec<SourceSpan> {
        let mut out = self.decision_spans(fn_idx, tree);
        if let Some(mapped) = self.mapping_spans.get(fn_idx) {
            out.extend(mapped.iter().cloned());
        }
        out.sort();
        out.dedup();
        out
    }
}

/// One test's join of one candidate: which coverage function its
/// decision spans resolved to, and the observations its counters
/// yield.
#[derive(Debug, Clone)]
pub struct TestJoin<'i> {
    /// The coverage function the decision spans resolved to.
    pub matched: FunctionMatch<'i>,
    /// Observations for this test; empty unless `matched` is
    /// [`FunctionMatch::Found`] and the test evaluated the decision.
    pub observations: Vec<ConditionObservation>,
}

/// Join one candidate to one test's [`CoverageIndex`].
///
/// The candidate's lookup spans ([`PreparedMir::lookup_spans`]: its
/// decision spans and its body's coverage-mapping spans) select its
/// coverage function; each condition's counters are the
/// merged function's branch region at the condition's span. The
/// counters are reduced with [`observations_from_counts`], with the
/// merged function's entry count as the consistency cross-check: a
/// vector only when the counters determine it, otherwise one
/// ambiguous observation. No observations when the test did not
/// reach the decision or the spans do not resolve to one function.
pub fn join_test<'i>(
    test_name: Option<String>,
    prepared: &PreparedMir<'_>,
    fn_idx: usize,
    index: &'i CoverageIndex,
    tree: &DecisionTree,
) -> TestJoin<'i> {
    let none = |matched| TestJoin {
        matched,
        observations: Vec::new(),
    };
    let (Some(spans), Some(decision_node)) =
        (prepared.function_spans.get(fn_idx), tree.decisions.first())
    else {
        return none(FunctionMatch::Missing);
    };
    let mir_name = prepared.mir.functions.get(fn_idx).map(|f| f.name.as_str());
    let matched = index.resolve(&prepared.lookup_spans(fn_idx, tree), mir_name);
    let FunctionMatch::Found(cov_fn) = matched else {
        return none(matched);
    };
    let counts: BTreeMap<String, BranchCounts> = spans
        .iter()
        .filter_map(|(name, span)| Some((name.clone(), cov_fn.branch_counts(span)?)))
        .collect();
    TestJoin {
        observations: reduce_counts(
            test_name,
            decision_node,
            &counts,
            Some(cov_fn.count),
            distinct_spans(decision_node, spans),
        ),
        matched,
    }
}

/// True iff no two conditions of `decision` were joined to the same
/// span, so each has its own counter (rule 4 of
/// [`observations_from_counts`] needs that).
fn distinct_spans(decision: &Node, spans: &BTreeMap<String, SourceSpan>) -> bool {
    let mut seen = BTreeSet::new();
    crate::masking::collect_conditions(decision)
        .iter()
        .filter_map(|c| spans.get(c))
        .all(|span| seen.insert(span))
}

/// The observations of [`join_test`] alone.
pub fn observations_from_prepared_coverage(
    test_name: Option<String>,
    prepared: &PreparedMir<'_>,
    fn_idx: usize,
    index: &CoverageIndex,
    tree: &DecisionTree,
) -> Vec<ConditionObservation> {
    join_test(test_name, prepared, fn_idx, index, tree).observations
}

/// Why coverage cannot say whether a candidate ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotMeasurableReason {
    /// The MIR was emitted by the instrumented compilation, and this
    /// body carries no coverage mapping: rustc did not instrument it
    /// (`#[automatically_derived]` impls such as serde derives,
    /// `#[coverage(off)]` items, bodies made of macro-expanded code
    /// with no usable source span). `llvm-cov` has no record for it.
    Uninstrumented,
    /// Every condition span lies in a file outside the workspace.
    /// The candidate is a workspace function (Floyd only reads MIR of
    /// the primary package), so those conditions were expanded from
    /// a dependency's macro; `llvm-cov` maps such code to the macro
    /// call site, never to the definition span MIR prints.
    ExternalMacro,
    /// No test's coverage has a record with a code or branch region
    /// at any of the decision's spans.
    NoCoverageRecord,
    /// The decision's spans cannot be attributed to one source
    /// function: they fall in more than one coverage function, the
    /// coverage records at those spans are different functions that
    /// no exact crate-relative path picks between, or
    /// another candidate with a different MIR name resolves to the
    /// same coverage function.
    AmbiguousSpans,
    /// The decision's coverage record exists, but it has no branch
    /// region at any of the decision's condition spans. rustc emits
    /// no branch counters for some decisions: `match` arms on an
    /// enum, `matches!` patterns, and some closure and macro
    /// bodies. Their arms carry only code regions, and Floyd does not
    /// derive condition outcomes from those.
    NoBranchRegion,
}

/// A candidate whose execution coverage cannot measure, with why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotMeasurable {
    /// The reason class.
    pub reason: NotMeasurableReason,
    /// Human-readable detail for reports.
    pub detail: String,
}

/// Whether a candidate's function ran, as coverage can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverageStatus {
    /// A test's coverage shows the function entered at least once.
    Invoked,
    /// The function has a coverage record and no test entered it.
    NeverInvoked,
    /// Coverage cannot say; see the reason.
    NotMeasurable(NotMeasurable),
}

/// One candidate joined to every test's coverage.
#[derive(Debug, Clone)]
pub struct CandidateCoverage {
    /// Whether the function ran. `invoked` in reports is
    /// `status == Invoked`; it uses the same span join as
    /// `observations`.
    pub status: CoverageStatus,
    /// Observations from every test, in test order. Empty when the
    /// candidate is not measurable.
    pub observations: Vec<ConditionObservation>,
    /// Report name from the coverage symbols ([`crate::coverage_index::display_name`]),
    /// when the join found one.
    pub display_name: Option<String>,
    /// Keys of the coverage functions the candidate resolved to
    /// (one per distinct key across tests).
    pub function_keys: Vec<FunctionId>,
    /// `true` when a found coverage function's entry count is
    /// non-zero in some test, whatever [`Self::status`] says. A
    /// [`NotMeasurableReason::NoBranchRegion`] candidate can be
    /// entered or not.
    pub entered: bool,
    /// Conditions of the decision with no branch region in any found
    /// coverage function: their span has no region, or they have no
    /// span. Empty when no coverage function was found.
    pub conditions_without_branch_region: Vec<String>,
}

impl CandidateCoverage {
    /// A candidate classified before any join was attempted.
    pub fn not_measurable(reason: NotMeasurableReason, detail: String) -> Self {
        CandidateCoverage {
            status: CoverageStatus::NotMeasurable(NotMeasurable { reason, detail }),
            observations: Vec::new(),
            display_name: None,
            function_keys: Vec::new(),
            entered: false,
            conditions_without_branch_region: Vec::new(),
        }
    }

    /// `true` iff [`Self::status`] is [`CoverageStatus::Invoked`].
    pub fn invoked(&self) -> bool {
        self.status == CoverageStatus::Invoked
    }
}

/// Classify a candidate from its MIR alone, before any join.
///
/// - [`NotMeasurableReason::Uninstrumented`]: `mir_instrumented` is
///   `true` (the MIR came from the `-Cinstrument-coverage`
///   compilation, so rustc printed coverage mappings for the bodies
///   it instrumented) and this body has none
///   ([`MirFunction::coverage_mappings`] is 0). This is rustc's own
///   decision, not a heuristic over names or spans.
/// - [`NotMeasurableReason::ExternalMacro`]: `workspace` is `true`
///   (paths are normalised against the workspace root) and every
///   decision span lies outside the root.
///
/// `None` when neither applies; the join then decides.
pub fn mir_not_measurable(
    prepared: &PreparedMir<'_>,
    fn_idx: usize,
    tree: &DecisionTree,
    mir_instrumented: bool,
    paths: Option<&PathNormaliser>,
) -> Option<NotMeasurable> {
    let f = prepared.mir.functions.get(fn_idx)?;
    if mir_instrumented && f.coverage_mappings == 0 {
        return Some(NotMeasurable {
            reason: NotMeasurableReason::Uninstrumented,
            detail: "rustc emitted no coverage mapping for this body (automatically \
                     derived impl, #[coverage(off)], or macro-expanded code); llvm-cov \
                     has no record of it"
                .to_string(),
        });
    }
    let paths = paths?;
    let spans = prepared.decision_spans(fn_idx, tree);
    if !spans.is_empty() && spans.iter().all(|s| paths.is_outside_root(&s.file)) {
        return Some(NotMeasurable {
            reason: NotMeasurableReason::ExternalMacro,
            detail: format!(
                "the decision's conditions are expanded from a macro defined outside \
                 the workspace ({}); llvm-cov maps that code to the call site",
                spans[0].file
            ),
        });
    }
    None
}

/// Join one candidate to every test's [`CoverageIndex`].
///
/// The same per-test join ([`join_test`]) gives both the
/// observations and the status, so `invoked` and the observations
/// can never disagree about which coverage function is the
/// candidate's:
///
/// - any test's spans resolve ambiguously →
///   [`NotMeasurableReason::AmbiguousSpans`], observations dropped;
/// - no test has a record at the spans →
///   [`NotMeasurableReason::NoCoverageRecord`];
/// - the found functions have no branch region at any of the
///   decision's condition spans →
///   [`NotMeasurableReason::NoBranchRegion`], observations dropped;
/// - otherwise [`CoverageStatus::Invoked`] if a found function's
///   (merged) entry count is non-zero in some test, else
///   [`CoverageStatus::NeverInvoked`].
pub fn join_candidate(
    prepared: &PreparedMir<'_>,
    fn_idx: usize,
    tree: &DecisionTree,
    tests: &[(String, CoverageIndex)],
) -> CandidateCoverage {
    let mut observations = Vec::new();
    let mut function_keys: Vec<FunctionId> = Vec::new();
    let mut display_name = None;
    let mut ambiguous: Option<String> = None;
    let mut invoked = false;
    let conditions: Vec<String> = tree
        .decisions
        .first()
        .map(crate::masking::collect_conditions)
        .unwrap_or_default();
    let condition_spans = prepared.function_spans.get(fn_idx);
    let span_of = |c: &String| condition_spans.and_then(|spans| spans.get(c));
    let mut with_branch: BTreeSet<&str> = BTreeSet::new();
    let mut other_branches = false;
    for (test_name, index) in tests {
        let join = join_test(Some(test_name.clone()), prepared, fn_idx, index, tree);
        match join.matched {
            FunctionMatch::Missing => {}
            FunctionMatch::Ambiguous(why) => {
                ambiguous.get_or_insert(why);
            }
            FunctionMatch::Found(f) => {
                invoked |= f.count > 0;
                for c in &conditions {
                    if span_of(c).is_some_and(|s| f.branch_counts(s).is_some()) {
                        with_branch.insert(c.as_str());
                    }
                }
                other_branches |= !f.branches.is_empty();
                if display_name.is_none() {
                    display_name = Some(f.display_name().to_string());
                }
                if !function_keys.contains(f.id()) {
                    function_keys.push(f.id().clone());
                }
            }
        }
        observations.extend(join.observations);
    }
    let conditions_without_branch_region: Vec<String> = if function_keys.is_empty() {
        Vec::new()
    } else {
        conditions
            .iter()
            .filter(|c| !with_branch.contains(c.as_str()))
            .cloned()
            .collect()
    };
    let status = if let Some(detail) = ambiguous {
        observations.clear();
        CoverageStatus::NotMeasurable(NotMeasurable {
            reason: NotMeasurableReason::AmbiguousSpans,
            detail,
        })
    } else if function_keys.is_empty() {
        CoverageStatus::NotMeasurable(NotMeasurable {
            reason: NotMeasurableReason::NoCoverageRecord,
            detail: format!(
                "no record in the coverage of {} test(s) has a code or branch region at \
                 the decision's spans",
                tests.len()
            ),
        })
    } else if with_branch.is_empty() {
        observations.clear();
        CoverageStatus::NotMeasurable(NotMeasurable {
            reason: NotMeasurableReason::NoBranchRegion,
            detail: format!(
                "the coverage record has no branch region at the decision's condition \
                 spans{} (rustc emits none for `match` arms on enums, `matches!` patterns \
                 and some closures and macros); the function was {}",
                if other_branches {
                    ", only elsewhere in the function"
                } else {
                    ""
                },
                if invoked { "entered" } else { "never entered" }
            ),
        })
    } else if invoked {
        CoverageStatus::Invoked
    } else {
        CoverageStatus::NeverInvoked
    };
    CandidateCoverage {
        status,
        observations,
        display_name,
        function_keys,
        entered: invoked,
        conditions_without_branch_region,
    }
}

/// Mark candidates that share a coverage function with a candidate
/// of a different MIR name as [`NotMeasurableReason::AmbiguousSpans`]
/// and drop their observations.
///
/// `candidates` pairs each candidate's MIR function name with its
/// join. Two MIR bodies with the same name and the same coverage
/// function are one source function seen twice (a `const fn`'s
/// runtime body and its `MIR FOR CTFE` body, or a module compiled
/// into several test crates). Different names on one coverage
/// function mean the spans do not identify the function (for
/// example a body whose MIR carries spans inlined from another
/// function), so neither candidate's counters can be trusted.
pub fn mark_shared_coverage(candidates: &mut [(&str, &mut CandidateCoverage)]) {
    let mut names_by_key: HashMap<&FunctionId, BTreeSet<&str>> = HashMap::new();
    for (name, c) in candidates.iter() {
        for key in &c.function_keys {
            names_by_key.entry(key).or_default().insert(name);
        }
    }
    let shared: Vec<(FunctionId, Vec<String>)> = names_by_key
        .into_iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(key, names)| (key.clone(), names.into_iter().map(String::from).collect()))
        .collect();
    for (key, names) in shared {
        for (_, c) in candidates.iter_mut() {
            if c.function_keys.contains(&key) {
                c.observations.clear();
                c.status = CoverageStatus::NotMeasurable(NotMeasurable {
                    reason: NotMeasurableReason::AmbiguousSpans,
                    detail: format!(
                        "candidates {} resolve to the same coverage function",
                        names.join(", ")
                    ),
                });
            }
        }
    }
}

/// Build a [`crate::masking::DecisionProvenance`] for a MIR function
/// — the function name, source file, and start line that identify
/// the decision for reports and qualification artefacts.
///
/// Source file and line are taken from the earliest condition span
/// in the function's MIR, which the engine already collects for
/// coverage correlation. The function name comes from
/// [`MirFunction::name`] verbatim.
pub fn provenance_for(f: &MirFunction) -> crate::masking::DecisionProvenance {
    let spans = collect_condition_spans(f);
    let earliest = spans.values().min_by_key(|s| (s.start_line, s.start_col));
    crate::masking::DecisionProvenance {
        function_name: Some(f.name.clone()),
        source_file: earliest.map(|s| s.file.clone()),
        line: earliest.map(|s| s.start_line),
        display_name: None,
        lift: None,
    }
}

/// Collect a `name -> SourceSpan` map for every named condition that
/// appears in the function's MIR.
///
/// Both `switchInt` discriminants (condition tests) and `AssignCopy`
/// sources (condition reads) are considered. First occurrence wins.
/// Spans are as rustc printed them; [`prepare_mir`] normalises them.
fn collect_condition_spans(f: &MirFunction) -> BTreeMap<String, SourceSpan> {
    let mut spans = BTreeMap::new();
    for block in &f.blocks {
        if let MirTerminator::SwitchInt {
            discr,
            targets,
            otherwise,
            span: Some(span),
            ..
        } = &block.terminator
        {
            // Debug-named discriminant: bool short-circuits and the
            // inline-compare temporaries (whose debug names are set
            // by name propagation) land here.
            if let Some(name) = f.debug_names.get(discr) {
                spans.entry(name.clone()).or_insert_with(|| span.clone());
            }
            // Match / if-let synthetic conditions: tie
            // `x == 0`, `x == Option::variant_1`, etc. to the
            // switchInt's source span so coverage branch counts
            // for the arm land on the right condition name. Without
            // this, every `pub fn f(x: Option<..>) -> bool { if let
            // Some(_) = x { .. } else { .. } }` reports 0
            // observations even when its tests exercise both arms.
            if let Some(names) = crate::decision::match_switchint_condition_names(
                block, f, *discr, targets, *otherwise,
            ) {
                for name in names {
                    spans.entry(name).or_insert_with(|| span.clone());
                }
            }
        }
        for stmt in &block.statements {
            match stmt {
                MirStatement::AssignCopy {
                    src,
                    span: Some(span),
                    ..
                } => {
                    if let Some(name) = f.debug_names.get(src) {
                        spans.entry(name.clone()).or_insert_with(|| span.clone());
                    }
                }
                MirStatement::AssignUnaryNot {
                    src,
                    span: Some(span),
                    ..
                } => {
                    // `_dst = Not(copy _src)` — rustc emits a
                    // dedicated branch counter on the Not's OUTPUT,
                    // so the natural condition identity at this
                    // span is `!<src_name>` (its true/false counts
                    // reflect `!<src_name>`'s runtime value). The
                    // decomposer encodes the terminal value as
                    // `Node::Condition { name: "!<src_name>" }` to
                    // match.
                    if let Some(name) = f.debug_names.get(src) {
                        spans
                            .entry(format!("!{name}"))
                            .or_insert_with(|| span.clone());
                    }
                }
                MirStatement::AssignBinaryCompare {
                    op,
                    lhs,
                    rhs,
                    span: Some(span),
                    ..
                } => {
                    // Inline-compare pattern `_T = Op(lhs, rhs);
                    // switchInt(_T) -> ...` (e.g. `byte >= 32 &&
                    // byte <= 126` in base64's `is_valid_b64_symbol`).
                    // The decomposer synthesises a condition name
                    // like `byte >= 32` via `compare_condition_name`
                    // and feeds it to the tree. Without this entry
                    // the span map had NO name for `_T`, so Pass 3
                    // correlation produced zero observations
                    // against runtime branch counts even though
                    // llvm-cov reports the branch region at exactly
                    // the compare's statement span. Mirror the
                    // decomposer's naming so correlation lands.
                    if let Some(name) = crate::decision::compare_condition_name(*op, lhs, rhs, f) {
                        spans.entry(name).or_insert_with(|| span.clone());
                    }
                }
                _ => {}
            }
        }
    }
    spans
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `_X = discriminant(_scrutinee); switchInt(_X) -> ...` for a
    /// 2-variant enum should surface the "scrutinee == variant_N"
    /// condition name mapped to the switchInt's source span, so
    /// coverage branch counts for the arm land under that name
    /// during correlation.
    #[test]
    fn collect_condition_spans_covers_enum_switchint_variant() {
        let mir_text = r#"
fn decide(_1: Option<i32>) -> bool {
    debug x => _1;
    let mut _0: bool;
    let mut _2: isize;

    bb0: {
        _2 = discriminant(_1);           // scope 0 at src/lib.rs:5:12: 5:19
        switchInt(move _2) -> [1: bb2, 0: bb1, otherwise: bb3]; // scope 0 at src/lib.rs:5:12: 5:19
    }

    bb1: {
        _0 = const false;
        goto -> bb4;
    }

    bb2: {
        _0 = const true;
        goto -> bb4;
    }

    bb3: {
        unreachable;
    }

    bb4: {
        return;
    }
}
"#;
        let mir = crate::mir::parse_text(mir_text).expect("MIR parses");
        let f = mir.functions.first().expect("one function parsed");
        let spans = collect_condition_spans(f);
        // Decomposer surfaces `x == variant_1` (the `Some` arm value,
        // since the Option type carries generic parameters and so is
        // not a "simple type identifier"). That exact string must be
        // present in the span map so runtime counts for the switchInt
        // branch region are joined to the condition name.
        assert!(
            spans.contains_key("x == variant_1"),
            "spans did not contain the synthesised match condition name; got: {:?}",
            spans.keys().collect::<Vec<_>>()
        );
    }
    use crate::mir;
    use crate::profile;

    /// Span-annotated MIR for `fn decide(a, b) -> bool { a && b }`.
    /// Captured during ADR-0002 scouting via
    /// `rustc --emit=mir -Zmir-include-spans` on nightly 1.97.
    const SPANNED_AND_MIR: &str = r#"
fn decide(_1: bool, _2: bool) -> bool {
    debug a => _1;                       // in scope 0 at /tmp/floyd-runtime-scout/src/lib.rs:1:15: 1:16
    debug b => _2;                       // in scope 0 at /tmp/floyd-runtime-scout/src/lib.rs:1:24: 1:25
    let mut _0: bool;                    // return place in scope 0 at /tmp/floyd-runtime-scout/src/lib.rs:1:36: 1:40

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at /tmp/floyd-runtime-scout/src/lib.rs:2:5: 2:6
    }

    bb1: {
        _0 = copy _2;                    // scope 0 at /tmp/floyd-runtime-scout/src/lib.rs:2:10: 2:11
        goto -> bb3;                     // scope 0 at /tmp/floyd-runtime-scout/src/lib.rs:2:5: 2:11
    }

    bb2: {
        _0 = const false;                // scope 0 at /tmp/floyd-runtime-scout/src/lib.rs:2:5: 2:11
        goto -> bb3;                     // scope 0 at /tmp/floyd-runtime-scout/src/lib.rs:2:5: 2:11
    }

    bb3: {
        return;                          // scope 0 at /tmp/floyd-runtime-scout/src/lib.rs:3:2: 3:2
    }
}
"#;

    /// Coverage JSON for the same function, with two tests (`ff` and
    /// `tt`) merged — same fixture as floyd::profile tests.
    const COVERAGE_JSON: &str = r#"
    {
      "data": [
        {
          "files": [],
          "functions": [
            {
              "name": "_RNvCsb6BSQ92hH2b_19floyd_runtime_scout6decide",
              "count": 2,
              "filenames": ["/tmp/floyd-runtime-scout/src/lib.rs"],
              "regions": [[1, 1, 1, 40, 2, 0, 0, 0]],
              "branches": [
                [2, 5, 2, 6, 1, 1, 0, 0, 4],
                [2, 10, 2, 11, 1, 0, 0, 0, 4]
              ],
              "mcdc_records": []
            }
          ],
          "totals": {}
        }
      ],
      "type": "llvm.coverage.json.export",
      "version": "2.0.1"
    }
    "#;

    /// The scout fixture has absolute paths on both sides.
    fn scout_paths() -> PathNormaliser {
        PathNormaliser::new(None)
    }

    #[test]
    fn correlate_produces_one_function_entry() {
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let cov = profile::parse(COVERAGE_JSON).expect("coverage parses");
        let map = correlate(&mir, &cov, &scout_paths());
        assert_eq!(map.functions.len(), 1);
        assert_eq!(map.functions[0].name, "decide");
    }

    #[test]
    fn correlate_joins_a_and_b_counts() {
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let cov = profile::parse(COVERAGE_JSON).expect("coverage parses");
        let map = correlate(&mir, &cov, &scout_paths());
        let f = &map.functions[0];

        // From the per-test data:
        //   ff exercised a=F, b not evaluated (short-circuit)
        //   tt exercised a=T, b=T
        // Aggregated counts:
        let a = f.conditions.get("a").expect("a present");
        assert_eq!(a.true_count, 1);
        assert_eq!(a.false_count, 1);
        assert_eq!(a.span.start_line, 2);
        assert_eq!(a.span.start_col, 5);

        let b = f.conditions.get("b").expect("b present");
        assert_eq!(b.true_count, 1);
        // The headline ADR-0002 finding: short-circuit visible at the
        // runtime layer because false_count stays at 0.
        assert_eq!(b.false_count, 0);
        assert_eq!(b.span.start_col, 10);
    }

    #[test]
    fn correlate_joins_by_span_whatever_the_symbol_says() {
        // The join never reads the symbol: a record whose name has
        // nothing in common with `decide` still joins by its spans.
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let renamed = COVERAGE_JSON.replace("6decide", "8different");
        let cov = profile::parse(&renamed).expect("coverage parses");
        let map = correlate(&mir, &cov, &scout_paths());
        assert_eq!(map.functions[0].conditions.len(), 2);
    }

    #[test]
    fn correlate_drops_function_whose_spans_have_no_record() {
        // Same symbol, same lines, but the record is for another
        // file: no record contains decide's spans.
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let elsewhere = COVERAGE_JSON.replace("/tmp/floyd-runtime-scout/", "/tmp/elsewhere/");
        let cov = profile::parse(&elsewhere).expect("coverage parses");
        let map = correlate(&mir, &cov, &scout_paths());
        // decide is in MIR but not in coverage; we get an entry with no conditions populated.
        assert_eq!(map.functions.len(), 1);
        assert!(map.functions[0].conditions.is_empty());
    }

    // -----------------------------------------------------------------------
    // observations_from_counts: when aggregate counters yield a vector
    // -----------------------------------------------------------------------

    fn cond(name: &str) -> Node {
        Node::Condition {
            name: name.to_string(),
        }
    }

    fn konst(value: bool) -> Node {
        Node::Const { value }
    }

    fn ite(c: &str, then_branch: Node, else_branch: Node) -> Node {
        Node::Ite {
            cond: c.to_string(),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
        }
    }

    /// `a && b`.
    fn and_node() -> Node {
        ite("a", cond("b"), konst(false))
    }

    fn counts(entries: &[(&str, u64, u64)]) -> BTreeMap<String, BranchCounts> {
        entries
            .iter()
            .map(|(n, t, f)| {
                (
                    n.to_string(),
                    BranchCounts {
                        true_count: *t,
                        false_count: *f,
                    },
                )
            })
            .collect()
    }

    fn inputs(entries: &[(&str, bool)]) -> BTreeMap<String, bool> {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn single_evaluation_yields_the_vector() {
        // (T,F) -> F, evaluated once.
        let obs = observations_from_counts(
            Some("once_tf".into()),
            &and_node(),
            &counts(&[("a", 1, 0), ("b", 0, 1)]),
            Some(1),
        );
        assert_eq!(obs.len(), 1);
        let o = &obs[0];
        assert!(!o.ambiguous);
        assert_eq!(o.inputs, inputs(&[("a", true), ("b", false)]));
        assert_eq!(o.result, Some(false));
        assert_eq!(o.evaluations, Some(1));
        assert!(o.counts.is_empty());
    }

    #[test]
    fn single_evaluation_omits_short_circuited_condition() {
        // (F,-) -> F: b never evaluated.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 0, 1), ("b", 0, 0)]),
            Some(1),
        );
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].inputs, inputs(&[("a", false)]));
        assert_eq!(obs[0].result, Some(false));
    }

    /// `(a || b) && c`: `c` is reached from `a = T` and from
    /// `a = F, b = T`, so the decomposer unfolds it into two nodes.
    fn or_and_node() -> Node {
        let c = || ite("c", konst(true), konst(false));
        ite("a", c(), ite("b", c(), konst(false)))
    }

    /// The observations' (inputs, result, evaluations, resolved_by).
    type Resolved = (
        BTreeMap<String, bool>,
        Option<bool>,
        Option<u64>,
        Option<Resolution>,
    );

    fn resolved(obs: &[ConditionObservation]) -> Vec<Resolved> {
        obs.iter()
            .map(|o| {
                assert!(!o.ambiguous, "{o:?}");
                assert!(o.counts.is_empty(), "{o:?}");
                (o.inputs.clone(), o.result, o.evaluations, o.resolved_by)
            })
            .collect()
    }

    fn tree_vector(entries: &[(&str, bool)], result: bool, k: u64) -> Resolved {
        (
            inputs(entries),
            Some(result),
            Some(k),
            Some(Resolution::TreeCounters),
        )
    }

    #[test]
    fn and_with_both_conditions_two_sided_resolves_by_tree_counters() {
        // (T,T), (T,F), (F,-): a and b both two-sided, so rule 5 could
        // not apply; `a && b` is a tree, a's true count (2) is b's
        // inflow, and the three leaves carry 1 each.
        let obs = observations_from_counts(
            Some("loop_three".into()),
            &and_node(),
            &counts(&[("a", 2, 1), ("b", 1, 1)]),
            Some(1),
        );
        assert_eq!(
            resolved(&obs),
            [
                tree_vector(&[("a", true), ("b", true)], true, 1),
                tree_vector(&[("a", true), ("b", false)], false, 1),
                tree_vector(&[("a", false)], false, 1),
            ]
        );
        // Leaf flows are evaluation counts, not 1 each.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 7, 4), ("b", 5, 2)]),
            None,
        );
        assert_eq!(
            resolved(&obs),
            [
                tree_vector(&[("a", true), ("b", true)], true, 5),
                tree_vector(&[("a", true), ("b", false)], false, 2),
                tree_vector(&[("a", false)], false, 4),
            ]
        );
        assert_eq!(
            obs.iter().map(|o| o.test_name.clone()).collect::<Vec<_>>(),
            [None, None, None]
        );
    }

    #[test]
    fn and_or_tree_resolves_every_leaf() {
        // a && (b || c): a tree (each condition at one node).
        // (T,T,-) x3, (T,F,T) x1, (T,F,F) x2, (F,-,-) x4.
        let and_or = ite("a", ite("b", konst(true), cond("c")), konst(false));
        let raw = counts(&[("a", 6, 4), ("b", 3, 3), ("c", 1, 2)]);
        let obs = observations_from_counts(Some("t".into()), &and_or, &raw, Some(10));
        assert_eq!(
            resolved(&obs),
            [
                tree_vector(&[("a", true), ("b", true)], true, 3),
                tree_vector(&[("a", true), ("b", false), ("c", true)], true, 1),
                tree_vector(&[("a", true), ("b", false), ("c", false)], false, 2),
                tree_vector(&[("a", false)], false, 4),
            ]
        );
        assert!(obs.iter().all(|o| o.test_name.as_deref() == Some("t")));

        // A leaf no evaluation reached is not emitted: c never true.
        let raw = counts(&[("a", 2, 1), ("b", 1, 1), ("c", 0, 1)]);
        let obs = observations_from_counts(None, &and_or, &raw, None);
        assert_eq!(
            resolved(&obs),
            [
                tree_vector(&[("a", true), ("b", true)], true, 1),
                tree_vector(&[("a", true), ("b", false), ("c", false)], false, 1),
                tree_vector(&[("a", false)], false, 1),
            ]
        );
    }

    #[test]
    fn dag_or_and_stays_ambiguous() {
        // (a || b) && c: (T,-,T) + (F,T,F) and (T,-,F) + (F,T,T) both
        // leave a = [1, 1], b = [1, 0], c = [1, 1]. c sits at two
        // nodes, so the graph is not a tree and the counters cannot
        // split c's counts between them.
        assert!(!is_tree(&or_and_node()));
        let raw = counts(&[("a", 1, 1), ("b", 1, 0), ("c", 1, 1)]);
        let obs = observations_from_counts(Some("dag".into()), &or_and_node(), &raw, Some(2));
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
        assert!(obs[0].inputs.is_empty());
        assert_eq!(obs[0].resolved_by, None);
        assert_eq!(obs[0].counts, raw);

        // Even when the counters happen to be flow-consistent on one
        // of c's nodes (b never reached c): no partial resolution.
        let raw = counts(&[("a", 2, 1), ("b", 0, 1), ("c", 1, 1)]);
        let obs = observations_from_counts(None, &or_and_node(), &raw, None);
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
    }

    #[test]
    fn inconsistent_tree_counters_stay_ambiguous() {
        let and_or = ite("a", ite("b", konst(true), cond("c")), konst(false));
        for raw in [
            // b's total (5) is not a's true count (6).
            counts(&[("a", 6, 4), ("b", 2, 3), ("c", 1, 2)]),
            // c's total (2) is not b's false count (3).
            counts(&[("a", 6, 4), ("b", 3, 3), ("c", 1, 1)]),
            // c counted, but b never false: c is unreachable.
            counts(&[("a", 2, 1), ("b", 2, 0), ("c", 1, 1)]),
            // b has inflow (a true twice) but no counter.
            counts(&[("a", 2, 1), ("c", 1, 1)]),
        ] {
            let obs = observations_from_counts(None, &and_or, &raw, None);
            assert_eq!(obs.len(), 1, "{raw:?}: {obs:?}");
            assert!(obs[0].ambiguous, "{raw:?}");
            assert_eq!(obs[0].resolved_by, None);
        }
    }

    #[test]
    fn shared_counter_withholds_the_tree_rule() {
        // a ? b : c with b and c joined to one span: the counter
        // fits both nodes' inflow (2 each) but cannot be split.
        let select = ite("a", cond("b"), cond("c"));
        let raw = counts(&[("a", 2, 2), ("b", 1, 1), ("c", 1, 1)]);
        let distinct = reduce_counts(None, &select, &raw, None, true);
        assert_eq!(distinct.len(), 4);
        assert!(distinct.iter().all(|o| !o.ambiguous));
        let shared = reduce_counts(None, &select, &raw, None, false);
        assert_eq!(shared.len(), 1);
        assert!(shared[0].ambiguous);

        // join_test's check: two names at one span.
        let span = |line| SourceSpan {
            file: "src/lib.rs".into(),
            start_line: line,
            start_col: 1,
            end_line: line,
            end_col: 2,
        };
        let spans = |c| {
            BTreeMap::from([
                ("a".to_string(), span(1)),
                ("b".to_string(), span(2)),
                ("c".to_string(), span(c)),
            ])
        };
        assert!(distinct_spans(&select, &spans(3)));
        assert!(!distinct_spans(&select, &spans(2)));
    }

    #[test]
    fn single_two_sided_rule_records_its_name() {
        // `(a || b) && c` is not a tree (c sits at two nodes), so
        // rule 4 declines; only a is two-sided: (T,-,T) -> T twice,
        // (F,T,T) -> T once.
        let obs = observations_from_counts(
            None,
            &or_and_node(),
            &counts(&[("a", 2, 1), ("b", 1, 0), ("c", 3, 0)]),
            None,
        );
        assert_eq!(obs.len(), 2);
        assert!(
            obs.iter()
                .all(|o| o.resolved_by == Some(Resolution::SingleTwoSided)
                    && o.evaluations == Some(3))
        );
        assert_eq!(obs[0].inputs, inputs(&[("a", true), ("c", true)]));
        assert_eq!(obs[0].result, Some(true));
        assert_eq!(
            obs[1].inputs,
            inputs(&[("a", false), ("b", true), ("c", true)])
        );
        assert_eq!(obs[1].result, Some(true));
        let json = serde_json::to_value(&obs[0]).unwrap();
        assert_eq!(json["resolved_by"], "single_two_sided");

        // On a tree the same shape of counters is resolved by rule 4,
        // tried first, with each vector's own count.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 1, 1), ("b", 1, 0)]),
            None,
        );
        assert_eq!(
            resolved(&obs),
            vec![
                tree_vector(&[("a", true), ("b", true)], true, 1),
                tree_vector(&[("a", false)], false, 1),
            ]
        );

        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 2, 1), ("b", 1, 1)]),
            None,
        );
        assert_eq!(
            serde_json::to_value(&obs[0]).unwrap()["resolved_by"],
            "tree_counters"
        );
        // Single-evaluation vectors carry no `resolved_by`.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 1, 0), ("b", 1, 0)]),
            None,
        );
        assert!(serde_json::to_value(&obs[0])
            .unwrap()
            .get("resolved_by")
            .is_none());
    }

    #[test]
    fn one_two_sided_condition_resolves_both_vectors() {
        // (T,T) then (F,-): the old collapse recorded (T,T) and lost
        // the (F,-) evaluation. Only a is two-sided and b (on a = T's
        // path only) counts exactly a's one true, so the two vectors
        // are determined. `a && b` is a tree, so rule 4 resolves it
        // first, one evaluation per vector; rule 5 alone gives the
        // same vectors, each with the test's N = 2.
        let raw = counts(&[("a", 1, 1), ("b", 1, 0)]);
        let obs =
            observations_from_counts(Some("loop_tt_then_f".into()), &and_node(), &raw, Some(1));
        assert_eq!(
            resolved(&obs),
            vec![
                tree_vector(&[("a", true), ("b", true)], true, 1),
                tree_vector(&[("a", false)], false, 1),
            ]
        );
        assert!(obs.iter().all(|o| !o.ambiguous && o.counts.is_empty()));

        let obs = one_two_sided_rule(&None, &and_node(), &raw, 2).expect("rule 5 applies");
        assert_eq!(obs.len(), 2);
        assert!(
            obs.iter()
                .all(|o| o.evaluations == Some(2)
                    && o.resolved_by == Some(Resolution::SingleTwoSided))
        );
        assert_eq!(obs[0].inputs, inputs(&[("a", true), ("b", true)]));
        assert_eq!(obs[0].result, Some(true));
        assert_eq!(obs[1].inputs, inputs(&[("a", false)]));
        assert_eq!(obs[1].result, Some(false));
    }

    #[test]
    fn one_two_sided_condition_behind_a_short_circuit_resolves() {
        // a && b with a = T every time and b two-sided: (T,T) x2,
        // (T,F) x1. The pivot is not the root.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 3, 0), ("b", 2, 1)]),
            None,
        );
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].inputs, inputs(&[("a", true), ("b", true)]));
        assert_eq!(obs[0].result, Some(true));
        assert_eq!(obs[1].inputs, inputs(&[("a", true), ("b", false)]));
        assert_eq!(obs[1].result, Some(false));
    }

    #[test]
    fn short_circuit_inconsistency_stays_ambiguous() {
        // a two-sided, b one-sided T. a = T's walk tests b, so b must
        // count exactly a's true_count (2); it counts 1. Some a = T
        // evaluation is unaccounted for: stay ambiguous.
        // Neither rule 4 (b's total is not a's true count) nor rule 5
        // accepts it.
        let short = counts(&[("a", 2, 1), ("b", 1, 0)]);
        let obs = observations_from_counts(None, &and_node(), &short, None);
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
        assert_eq!(obs[0].counts, short);
        assert!(one_two_sided_rule(&None, &and_node(), &short, 3).is_none());

        // b counted on a = F's walk, which short-circuits it.
        let behind = counts(&[("a", 1, 1), ("b", 2, 0)]);
        let obs = observations_from_counts(None, &and_node(), &behind, None);
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
        assert!(one_two_sided_rule(&None, &and_node(), &behind, 2).is_none());

        // a = T's walk needs b, which this test never evaluated.
        let never = counts(&[("a", 1, 1), ("b", 0, 0)]);
        let obs = observations_from_counts(None, &and_node(), &never, None);
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
        assert!(one_two_sided_rule(&None, &and_node(), &never, 2).is_none());

        // The same checks on a decision that is not a tree, where
        // rule 5 is the only rule that could apply: c counted on
        // a = T's walk only (it is on both).
        let raw = counts(&[("a", 2, 1), ("b", 1, 0), ("c", 2, 0)]);
        let obs = observations_from_counts(None, &or_and_node(), &raw, None);
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
    }

    #[test]
    fn one_two_sided_rule_needs_every_condition_on_a_walk_counted() {
        // a || (b && c): c one-sided but on no walk (b is always F),
        // yet it has a counter: inconsistent, stay ambiguous.
        let or_and = ite("a", konst(true), ite("b", cond("c"), konst(false)));
        let raw = counts(&[("a", 1, 2), ("b", 0, 2), ("c", 1, 0)]);
        let obs = observations_from_counts(None, &or_and, &raw, None);
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
        assert!(one_two_sided_rule(&None, &or_and, &raw, 3).is_none());

        // Without the stray c: (T) -> T once, (F,F) -> F twice.
        let raw = counts(&[("a", 1, 2), ("b", 0, 2), ("c", 0, 0)]);
        assert_eq!(
            one_two_sided_rule(&None, &or_and, &raw, 3).map(|o| o.len()),
            Some(2)
        );
        let obs = observations_from_counts(None, &or_and, &raw, None);
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].inputs, inputs(&[("a", true)]));
        assert_eq!(obs[0].result, Some(true));
        assert_eq!(obs[1].inputs, inputs(&[("a", false), ("b", false)]));
        assert_eq!(obs[1].result, Some(false));
    }

    /// rust-base64 `alphabet::is_valid_b64_symbol`
    /// (`byte >= 32 && byte <= 126`, src/alphabet.rs:210) as Floyd
    /// decomposes it.
    fn base64_symbol() -> Node {
        ite("byte >= 32", cond("byte <= 126"), konst(false))
    }

    /// Every test that reached `is_valid_b64_symbol` in sweep
    /// `ambig` (rust-base64 5b98ee1), with its counters cut from its
    /// own `llvm-cov export` at the two condition spans (210:5-18
    /// and 210:22-36): (name, byte >= 32 (T, F), byte <= 126 (T, F)).
    /// The randomised tests' counters vary from run to run.
    /// (true, false) counters of one condition.
    type Tf = (u64, u64);

    const BASE64_TESTS: &[(&str, Tf, Tf)] = &[
        ("alphabet::tests::detects_duplicate_end", (63, 0), (63, 0)),
        ("alphabet::tests::detects_duplicate_middle", (26, 0), (26, 0)),
        ("alphabet::tests::detects_duplicate_start", (1, 0), (1, 0)),
        ("alphabet::tests::detects_padding", (64, 0), (64, 0)),
        ("alphabet::tests::detects_unprintable", (0, 1), (0, 0)),
        ("alphabet::tests::same_as_unchecked", (64, 0), (64, 0)),
        ("alphabet::tests::str_same_as_input", (64, 0), (64, 0)),
        ("alphabet::tests::symbol_matches_char_for_all_valid_symbols", (224, 32), (95, 129)),
        ("chunked_encoder::tests::chunked_encode_matches_normal_encode_random_string_sink", (6412640, 729404), (3468117, 2944523)),
        ("decode::tests::decode_into_nonempty_vec_doesnt_clobber_existing_prefix", (3211594, 366571), (1736141, 1475453)),
        ("decode::tests::decode_slice_doesnt_clobber_existing_prefix_or_suffix", (3208291, 365184), (1735965, 1472326)),
        ("decode::tests::decode_slice_unchecked_doesnt_clobber_existing_prefix_or_suffix", (3208480, 366466), (1735153, 1473327)),
        ("display::tests::display_encode_matches_normal_encode", (6417341, 731743), (3469745, 2947596)),
        ("encode::tests::encode_engine_slice_into_nonempty_buffer_doesnt_clobber_suffix", (3203310, 365183), (1734114, 1469196)),
        ("encode::tests::encode_engine_string_into_nonempty_buffer_doesnt_clobber_prefix", (3207252, 366223), (1734188, 1473064)),
        ("encode::tests::encode_to_slice_random_valid_utf8", (3205328, 365914), (1733940, 1471388)),
        ("encode::tests::encode_with_padding_random_valid_utf8", (3206515, 364542), (1734157, 1472358)),
        ("engine::tests::decode_doesnt_write_extra_bytes::case_1_general_purpose", (3208587, 366013), (1734550, 1474037)),
        ("engine::tests::decode_doesnt_write_extra_bytes::case_2_naive", (3205972, 365936), (1734049, 1471923)),
        ("engine::tests::decode_doesnt_write_extra_bytes::case_3_decoder_reader", (3203796, 364843), (1734636, 1469160)),
        ("engine::tests::decode_into_slice_fits_in_precisely_sized_slice::case_1_general_purpose", (3201209, 364589), (1733378, 1467831)),
        ("engine::tests::decode_into_slice_fits_in_precisely_sized_slice::case_2_naive", (3208900, 365566), (1734964, 1473936)),
        ("engine::tests::decode_into_slice_fits_in_precisely_sized_slice::case_3_decoder_reader", (3200273, 364573), (1732411, 1467862)),
        ("engine::tests::decode_invalid_byte_error::case_1_general_purpose", (32063031, 3652813), (17343424, 14719607)),
        ("engine::tests::decode_invalid_byte_error::case_2_naive", (32072554, 3654150), (17344420, 14728134)),
        ("engine::tests::decode_invalid_byte_error::case_3_decoder_reader", (32059533, 3652309), (17338996, 14720537)),
        ("engine::tests::encode_doesnt_write_extra_bytes::case_1_general_purpose", (3203481, 365046), (1733491, 1469990)),
        ("engine::tests::encode_doesnt_write_extra_bytes::case_2_naive", (3207082, 365326), (1734133, 1472949)),
        ("engine::tests::encode_doesnt_write_extra_bytes::case_3_decoder_reader", (3206877, 364792), (1733031, 1473846)),
        ("engine::tests::encode_engine_slice_fits_into_precisely_sized_slice::case_1_general_purpose", (3206982, 365402), (1733696, 1473286)),
        ("engine::tests::encode_engine_slice_fits_into_precisely_sized_slice::case_2_naive", (3212527, 365943), (1736081, 1476446)),
        ("engine::tests::encode_engine_slice_fits_into_precisely_sized_slice::case_3_decoder_reader", (3209270, 365210), (1735468, 1473802)),
        ("engine::tests::encode_matches_naive::case_1_general_purpose", (3202927, 365119), (1732642, 1470285)),
        ("engine::tests::encode_matches_naive::case_2_naive", (3206402, 364883), (1734476, 1471926)),
        ("engine::tests::encode_matches_naive::case_3_decoder_reader", (3206128, 364541), (1734511, 1471617)),
        ("engine::tests::roundtrip_random_alphabet::case_1_general_purpose", (3210343, 364530), (1735305, 1475038)),
        ("engine::tests::roundtrip_random_alphabet::case_2_naive", (3208731, 365341), (1735428, 1473303)),
        ("engine::tests::roundtrip_random_alphabet::case_3_decoder_reader", (3208081, 365385), (1734339, 1473742)),
        ("engine::tests::roundtrip_weird_alphabet::case_1_general_purpose", (65, 0), (65, 0)),
        ("engine::tests::roundtrip_weird_alphabet::case_2_naive", (65, 0), (65, 0)),
        ("engine::tests::roundtrip_weird_alphabet::case_3_decoder_reader", (65, 0), (65, 0)),
        ("read::decoder_tests::handles_short_read_from_delegate", (3209464, 365445), (1735881, 1473583)),
        ("read::decoder_tests::read_in_short_increments", (3207859, 366269), (1735707, 1472152)),
        ("read::decoder_tests::read_in_short_increments_with_short_delegate_reads", (3204957, 365402), (1734390, 1470567)),
        ("read::decoder_tests::reports_invalid_last_symbol_correctly", (321512, 36510), (173793, 147719)),
        ("tests::roundtrip_random_config_long", (3209439, 365716), (1734814, 1474625)),
        ("tests::roundtrip_random_config_short", (3206882, 363720), (1733894, 1472988)),
        ("write::encoder_string_writer::tests::every_possible_split_of_input", (1603006, 183039), (867076, 735930)),
        ("write::encoder_string_writer::tests::incremental_writes", (1602506, 182731), (867204, 735302)),
        ("write::encoder_tests::encode_random_config_matches_normal_encode_reasonable_input_len", (320798, 36467), (173217, 147581)),
        ("write::encoder_tests::encode_random_config_matches_normal_encode_tiny_input_len", (321050, 36741), (173311, 147739)),
        ("write::encoder_tests::every_possible_split_of_input", (1602506, 182423), (867208, 735298)),
        ("write::encoder_tests::retrying_writes_that_error_with_interrupted_works", (320473, 36480), (173576, 146897)),
        ("write::encoder_tests::writes_that_only_write_part_of_input_and_sometimes_interrupt_produce_correct_encoded_data", (320840, 36753), (173976, 146864)),
    ];

    #[test]
    fn base64_is_valid_b64_symbol_counts_resolve_by_tree_counters() {
        // symbol_matches_char_for_all_valid_symbols (256 calls):
        // byte >= 32 = [224 T, 32 F], byte <= 126 = [95 T, 129 F].
        // Both two-sided; byte <= 126's total (224) is byte >= 32's
        // true count, so the leaves carry 95, 129 and 32.
        let raw = counts(&[("byte >= 32", 224, 32), ("byte <= 126", 95, 129)]);
        let obs = observations_from_counts(
            Some("symbol_matches_char_for_all_valid_symbols".into()),
            &base64_symbol(),
            &raw,
            Some(256),
        );
        assert_eq!(
            resolved(&obs),
            [
                tree_vector(&[("byte >= 32", true), ("byte <= 126", true)], true, 95),
                tree_vector(&[("byte >= 32", true), ("byte <= 126", false)], false, 129),
                tree_vector(&[("byte >= 32", false)], false, 32),
            ]
        );
    }

    #[test]
    fn base64_every_test_resolves_and_both_conditions_are_exercised() {
        let decision = base64_symbol();
        let mut all = Vec::new();
        for (name, a, b) in BASE64_TESTS {
            let raw = counts(&[("byte >= 32", a.0, a.1), ("byte <= 126", b.0, b.1)]);
            let obs = observations_from_counts(Some(name.to_string()), &decision, &raw, None);
            assert!(!obs.is_empty(), "{name}");
            assert!(obs.iter().all(|o| !o.ambiguous), "{name}: {obs:?}");
            assert_eq!(
                obs.iter().map(|o| o.evaluations.unwrap()).sum::<u64>(),
                a.0 + a.1,
                "{name}"
            );
            all.extend(obs);
        }
        let tree = DecisionTree {
            decisions: vec![decision],
        };
        let r = crate::masking::analyze_with_runtime(&tree, &all, crate::masking::Variant::Masking);
        assert_eq!(r.ambiguous_observations, 0);
        for c in ["byte >= 32", "byte <= 126"] {
            assert!(
                matches!(
                    r.condition_status[c],
                    crate::masking::ConditionStatus::Exercised(_)
                ),
                "{c}: {:?}",
                r.condition_status[c]
            );
        }
    }

    /// anyhow `src/ensure.rs:60`, `|b| b == b' ' || b == b'\n'`, as
    /// Floyd decomposes it.
    fn anyhow_whitespace() -> Node {
        ite("b == 32", konst(true), cond("b == 10"))
    }

    #[test]
    fn anyhow_test_whitespace_counts_resolve_to_two_vectors() {
        // Counters of `test_whitespace` at the two condition spans in
        // its own llvm-cov export: 9 evaluations, b == b' ' (60:30)
        // = [1 T, 8 F], b == b'\n' (60:43) = [0 T, 8 F]. Sweep 7
        // recorded it as ambiguous and reported both conditions
        // unknown. `a || b` is a tree: (T,-) once, (F,F) 8 times.
        let raw = counts(&[("b == 32", 1, 8), ("b == 10", 0, 8)]);
        let obs = observations_from_counts(
            Some("test_whitespace".into()),
            &anyhow_whitespace(),
            &raw,
            Some(1),
        );
        assert_eq!(obs.len(), 2, "{obs:?}");
        assert!(obs.iter().all(|o| !o.ambiguous));
        assert_eq!(obs[0].evaluations, Some(1));
        assert_eq!(obs[1].evaluations, Some(8));
        assert_eq!(obs[0].inputs, inputs(&[("b == 32", true)]));
        assert_eq!(obs[0].result, Some(true));
        assert_eq!(
            obs[1].inputs,
            inputs(&[("b == 32", false), ("b == 10", false)])
        );
        assert_eq!(obs[1].result, Some(false));

        // With the 14 single-path tests' (F,F) -> F, b == 32 is
        // exercised and b == 10 (never true anywhere) unexercised.
        let tree = DecisionTree {
            decisions: vec![anyhow_whitespace()],
        };
        let r = crate::masking::analyze_with_runtime(&tree, &obs, crate::masking::Variant::Masking);
        assert!(matches!(
            r.condition_status["b == 32"],
            crate::masking::ConditionStatus::Exercised(_)
        ));
        assert!(matches!(
            r.condition_status["b == 10"],
            crate::masking::ConditionStatus::Unexercised
        ));
        assert_eq!(r.ambiguous_observations, 0);
    }

    /// chrono `NaiveTime::from_hms_nano_opt` (src/naive/time/mod.rs:429).
    fn chrono_from_hms_nano() -> Node {
        let n2 = || cond("nano >= 2000000000");
        ite(
            "hour >= 24",
            konst(true),
            ite(
                "min >= 60",
                konst(true),
                ite(
                    "sec >= 60",
                    konst(true),
                    ite(
                        "nano >= 1000000000",
                        ite("sec != 59", konst(true), n2()),
                        n2(),
                    ),
                ),
            ),
        )
    }

    #[test]
    fn chrono_test_leap_second_counts_resolve_to_two_vectors() {
        // `offset::local::tests::test_leap_second`'s counters in its
        // llvm-cov export: 2 evaluations, only `sec != 59` two-sided;
        // `nano >= 2000000000` sits behind it and counts its false
        // walk only.
        let raw = counts(&[
            ("hour >= 24", 0, 2),
            ("min >= 60", 0, 2),
            ("sec >= 60", 0, 2),
            ("nano >= 1000000000", 2, 0),
            ("sec != 59", 1, 1),
            ("nano >= 2000000000", 0, 1),
        ]);
        let decision = chrono_from_hms_nano();
        let obs = observations_from_counts(
            Some("offset::local::tests::test_leap_second".into()),
            &decision,
            &raw,
            Some(2),
        );
        assert_eq!(obs.len(), 2, "{obs:?}");
        let prefix = [
            ("hour >= 24", false),
            ("min >= 60", false),
            ("sec >= 60", false),
            ("nano >= 1000000000", true),
        ];
        let mut t = prefix.to_vec();
        t.push(("sec != 59", true));
        let mut f = prefix.to_vec();
        f.extend([("sec != 59", false), ("nano >= 2000000000", false)]);
        assert_eq!(obs[0].inputs, inputs(&t));
        assert_eq!(obs[0].result, Some(true));
        assert_eq!(obs[1].inputs, inputs(&f));
        assert_eq!(obs[1].result, Some(false));

        // `round::tests::test_round_leap_nanos` (one evaluation) is
        // the (.., n1 T, s59 F, n2 F) -> F row, and the all-false row
        // comes from single-evaluation tests: with the leap-second
        // vectors both `nano >= 1000000000` and `sec != 59` have a
        // pair.
        let mut all = obs;
        all.push(ConditionObservation::vector(
            Some("naive::time::tests::test_time_from_hms_milli".into()),
            inputs(&[
                ("hour >= 24", false),
                ("min >= 60", false),
                ("sec >= 60", false),
                ("nano >= 1000000000", false),
                ("nano >= 2000000000", false),
            ]),
            false,
        ));
        let tree = DecisionTree {
            decisions: vec![decision],
        };
        let r = crate::masking::analyze_with_runtime(&tree, &all, crate::masking::Variant::Masking);
        for c in ["nano >= 1000000000", "sec != 59"] {
            assert!(
                matches!(
                    r.condition_status[c],
                    crate::masking::ConditionStatus::Exercised(_)
                ),
                "{c}: {:?}",
                r.condition_status[c]
            );
        }
    }

    #[test]
    fn two_two_sided_conditions_stay_ambiguous_on_real_chrono_shape() {
        // Same decision, a test that hit both `sec != 59` and
        // `nano >= 1000000000` both ways: no rule applies.
        let raw = counts(&[
            ("hour >= 24", 0, 3),
            ("min >= 60", 0, 3),
            ("sec >= 60", 0, 3),
            ("nano >= 1000000000", 2, 1),
            ("sec != 59", 1, 1),
            ("nano >= 2000000000", 0, 2),
        ]);
        let obs = observations_from_counts(None, &chrono_from_hms_nano(), &raw, Some(3));
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
    }

    #[test]
    fn single_condition_decision_in_a_loop_is_credited_both_ways() {
        // `if x { true } else { false }` evaluated 5 times: 3 T, 2 F.
        let decision = ite("x", konst(true), konst(false));
        let obs = observations_from_counts(
            Some("loop".into()),
            &decision,
            &counts(&[("x", 3, 2)]),
            Some(5),
        );
        assert_eq!(obs.len(), 2);
        assert!(obs.iter().all(|o| !o.ambiguous && o.evaluations == Some(5)));
        assert_eq!(obs[0].inputs, inputs(&[("x", true)]));
        assert_eq!(obs[0].result, Some(true));
        assert_eq!(obs[1].inputs, inputs(&[("x", false)]));
        assert_eq!(obs[1].result, Some(false));

        // And the pair is demonstrated by this one test.
        let tree = DecisionTree {
            decisions: vec![decision],
        };
        let r = crate::masking::analyze_with_runtime(&tree, &obs, crate::masking::Variant::Masking);
        assert!(matches!(
            r.condition_status["x"],
            crate::masking::ConditionStatus::Exercised(_)
        ));
    }

    #[test]
    fn uniform_repeated_evaluations_yield_the_vector() {
        // (T,F) three times: every condition one-sided, so every
        // evaluation took the same path with the same values.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 3, 0), ("b", 0, 3)]),
            Some(3),
        );
        assert_eq!(obs.len(), 1);
        assert!(!obs[0].ambiguous);
        assert_eq!(obs[0].inputs, inputs(&[("a", true), ("b", false)]));
        assert_eq!(obs[0].result, Some(false));
        assert_eq!(obs[0].evaluations, Some(3));
    }

    #[test]
    fn evaluation_count_comes_from_the_root_not_the_function_count() {
        // Decision evaluated twice in ONE call (loop in the body):
        // function count 1 must not make it look single-evaluation.
        // Rule 1 would read b's two counters as one value; rule 4
        // derives (T,T) and (T,F), one evaluation each.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 2, 0), ("b", 1, 1)]),
            Some(1),
        );
        assert_eq!(obs.len(), 2);
        assert!(obs.iter().all(|o| !o.ambiguous && o.evaluations == Some(1)));
        // Two two-sided conditions in one call: rule 4 resolves the
        // three evaluations into three vectors, one each.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 2, 1), ("b", 1, 1)]),
            Some(1),
        );
        assert_eq!(obs.len(), 3);
        assert!(obs.iter().all(|o| !o.ambiguous && o.evaluations == Some(1)));
    }

    #[test]
    fn inconsistent_counters_are_ambiguous() {
        // Root says one evaluation, but b counted twice.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 1, 0), ("b", 2, 0)]),
            Some(1),
        );
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);

        // Decision evaluated but the function was never entered.
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 1, 0), ("b", 1, 0)]),
            Some(0),
        );
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
    }

    #[test]
    fn unevaluated_decision_yields_no_observation() {
        assert!(observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 0, 0), ("b", 0, 0)]),
            Some(0),
        )
        .is_empty());
        // Root has no counters at all (span did not join).
        assert!(
            observations_from_counts(None, &and_node(), &counts(&[("b", 1, 0)]), None).is_empty()
        );
    }

    #[test]
    fn merged_two_evaluation_coverage_resolves_to_both_vectors() {
        // COVERAGE_JSON is `ff` and `tt` merged into one profile:
        // two evaluations, a = [1, 1], b = [1, 0]. The old collapse
        // reported it as the single vector (T,T); rule 4 recovers
        // exactly the two evaluations, (T,T) -> T and (F,-) -> F,
        // once each.
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let cov = profile::parse(COVERAGE_JSON).expect("coverage parses");
        let tree = decision::decompose(&mir);
        let prepared = prepare_mir(&mir, &scout_paths());
        let index = CoverageIndex::build(&cov, &scout_paths());
        let obs =
            observations_from_prepared_coverage(Some("merged".into()), &prepared, 0, &index, &tree);
        assert_eq!(obs.len(), 2);
        assert!(obs.iter().all(|o| !o.ambiguous
            && o.evaluations == Some(1)
            && o.resolved_by == Some(Resolution::TreeCounters)));
        assert_eq!(obs[0].inputs, inputs(&[("a", true), ("b", true)]));
        assert_eq!(obs[1].inputs, inputs(&[("a", false)]));

        // A third evaluation (T,F) makes b two-sided too: `a && b`
        // is a tree, so rule 4 gives the three vectors, once each.
        let three = COVERAGE_JSON
            .replace("\"count\": 2", "\"count\": 3")
            .replace("[2, 5, 2, 6, 1, 1, 0, 0, 4]", "[2, 5, 2, 6, 2, 1, 0, 0, 4]")
            .replace(
                "[2, 10, 2, 11, 1, 0, 0, 0, 4]",
                "[2, 10, 2, 11, 1, 1, 0, 0, 4]",
            );
        let cov = profile::parse(&three).expect("coverage parses");
        let index = CoverageIndex::build(&cov, &scout_paths());
        let obs =
            observations_from_prepared_coverage(Some("merged".into()), &prepared, 0, &index, &tree);
        assert_eq!(obs.len(), 3, "{obs:?}");
        assert!(obs.iter().all(|o| !o.ambiguous
            && o.evaluations == Some(1)
            && o.resolved_by == Some(Resolution::TreeCounters)));
        assert_eq!(obs[0].inputs, inputs(&[("a", true), ("b", true)]));
        assert_eq!(obs[1].inputs, inputs(&[("a", true), ("b", false)]));
        assert_eq!(obs[2].inputs, inputs(&[("a", false)]));
    }

    #[test]
    fn prepared_path_uses_function_count_and_yields_vector_for_single_call() {
        // Same function, one call evaluating (T,T).
        let single = COVERAGE_JSON
            .replace("\"count\": 2", "\"count\": 1")
            .replace("[2, 5, 2, 6, 1, 1, 0, 0, 4]", "[2, 5, 2, 6, 1, 0, 0, 0, 4]");
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let cov = profile::parse(&single).expect("coverage parses");
        let tree = decision::decompose(&mir);
        let prepared = prepare_mir(&mir, &scout_paths());
        let index = CoverageIndex::build(&cov, &scout_paths());
        let obs =
            observations_from_prepared_coverage(Some("tt".into()), &prepared, 0, &index, &tree);
        assert_eq!(obs.len(), 1);
        assert!(!obs[0].ambiguous);
        assert_eq!(obs[0].inputs, inputs(&[("a", true), ("b", true)]));
        assert_eq!(obs[0].result, Some(true));
    }

    // -----------------------------------------------------------------------
    // Span join on real chrono / can-dbc MIR and coverage
    // -----------------------------------------------------------------------

    /// From chrono's lib unit-test MIR (`--emit=mir
    /// -Zmir-include-spans` in the instrumented compilation, so the
    /// coverage mappings are printed): the inherent method
    /// `RuleDay::julian_0`, the trait method `<NaiveTime as
    /// Timelike>::with_nanosecond`, and the `MIR FOR CTFE` body of
    /// the `const fn` `FixedOffset::east_opt` (whose operand set-up
    /// copy `_3 = copy _1` at 63:22-63:26 once decomposed as a
    /// spurious condition `secs`; it is not a decision).
    const CHRONO_MIR: &str = r#"
fn rule::<impl at src/offset/local/tz_info/rule.rs:460:1: 460:13>::julian_0(_1: u16) -> Result<RuleDay, tz_info::Error> {
    debug julian_day_0 => _1;            // in scope 0 at src/offset/local/tz_info/rule.rs:500:23: 500:35
    let mut _0: std::result::Result<offset::local::tz_info::rule::RuleDay, offset::local::tz_info::Error>; // return place in scope 0 at src/offset/local/tz_info/rule.rs:500:45: 500:64
    let mut _2: bool;                    // in scope 0 at src/offset/local/tz_info/rule.rs:501:12: 501:30
    let mut _3: offset::local::tz_info::Error; // in scope 0 at src/offset/local/tz_info/rule.rs:502:24: 502:76
    let mut _4: offset::local::tz_info::rule::RuleDay; // in scope 0 at src/offset/local/tz_info/rule.rs:505:12: 505:50

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/offset/local/tz_info/rule.rs:501:12: 501:30 (#0)

    coverage Code { bcb: bcb0 } => src/offset/local/tz_info/rule.rs:500:5: 500:64 (#0);
    coverage Code { bcb: bcb0 } => src/offset/local/tz_info/rule.rs:501:12: 501:30 (#0);
    coverage Code { bcb: bcb1 } => src/offset/local/tz_info/rule.rs:502:13: 502:77 (#0);
    coverage Code { bcb: bcb3 } => src/offset/local/tz_info/rule.rs:503:9: 503:10 (#0);
    coverage Code { bcb: bcb3 } => src/offset/local/tz_info/rule.rs:505:9: 505:51 (#0);
    coverage Code { bcb: bcb2 } => src/offset/local/tz_info/rule.rs:506:5: 506:6 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => src/offset/local/tz_info/rule.rs:501:12: 501:30 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/offset/local/tz_info/rule.rs:501:12: 501:30
        _2 = Gt(copy _1, const 365_u16); // scope 0 at src/offset/local/tz_info/rule.rs:501:12: 501:30
        switchInt(move _2) -> [0: bb2, otherwise: bb1]; // scope 0 at src/offset/local/tz_info/rule.rs:501:12: 501:30
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/offset/local/tz_info/rule.rs:502:76: 502:77
        _3 = tz_info::Error::TransitionRule(const "invalid rule day julian day"); // scope 0 at src/offset/local/tz_info/rule.rs:502:24: 502:76
        _0 = Result::<RuleDay, tz_info::Error>::Err(move _3); // scope 0 at src/offset/local/tz_info/rule.rs:502:20: 502:77
        goto -> bb3;                     // scope 0 at src/offset/local/tz_info/rule.rs:506:6: 506:6
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at src/offset/local/tz_info/rule.rs:506:6: 506:6
        _4 = RuleDay::Julian0WithLeap(copy _1); // scope 0 at src/offset/local/tz_info/rule.rs:505:12: 505:50
        _0 = Result::<RuleDay, tz_info::Error>::Ok(move _4); // scope 0 at src/offset/local/tz_info/rule.rs:505:9: 505:51
        goto -> bb3;                     // scope 0 at src/offset/local/tz_info/rule.rs:506:6: 506:6
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at src/offset/local/tz_info/rule.rs:506:6: 506:6
        return;                          // scope 0 at src/offset/local/tz_info/rule.rs:506:6: 506:6
    }
}

fn naive::time::<impl at src/naive/time/mod.rs:902:1: 902:28>::with_nanosecond(_1: &NaiveTime, _2: u32) -> Option<NaiveTime> {
    debug self => _1;                    // in scope 0 at src/naive/time/mod.rs:1109:24: 1109:29
    debug nano => _2;                    // in scope 0 at src/naive/time/mod.rs:1109:31: 1109:35
    let mut _0: std::option::Option<naive::time::NaiveTime>; // return place in scope 0 at src/naive/time/mod.rs:1109:45: 1109:62
    let mut _3: bool;                    // in scope 0 at src/naive/time/mod.rs:1110:12: 1110:33
    let mut _4: naive::time::NaiveTime;  // in scope 0 at src/naive/time/mod.rs:1113:14: 1113:47

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/naive/time/mod.rs:1110:12: 1110:33 (#0)

    coverage Code { bcb: bcb0 } => src/naive/time/mod.rs:1109:5: 1109:62 (#0);
    coverage Code { bcb: bcb0 } => src/naive/time/mod.rs:1110:12: 1110:33 (#0);
    coverage Code { bcb: bcb1 } => src/naive/time/mod.rs:1111:13: 1111:24 (#0);
    coverage Code { bcb: bcb3 } => src/naive/time/mod.rs:1112:9: 1112:10 (#0);
    coverage Code { bcb: bcb3 } => src/naive/time/mod.rs:1113:9: 1113:48 (#0);
    coverage Code { bcb: bcb2 } => src/naive/time/mod.rs:1114:5: 1114:6 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => src/naive/time/mod.rs:1110:12: 1110:33 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/naive/time/mod.rs:1110:12: 1110:33
        _3 = Ge(copy _2, const 2000000000_u32); // scope 0 at src/naive/time/mod.rs:1110:12: 1110:33
        switchInt(move _3) -> [0: bb2, otherwise: bb1]; // scope 0 at src/naive/time/mod.rs:1110:12: 1110:33
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/naive/time/mod.rs:1114:6: 1114:6
        _0 = Option::<NaiveTime>::None;  // scope 0 at src/naive/time/mod.rs:1111:20: 1111:24
        goto -> bb3;                     // scope 0 at src/naive/time/mod.rs:1114:6: 1114:6
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at src/naive/time/mod.rs:1114:6: 1114:6
        _4 = NaiveTime { secs: copy ((*_1).0: u32), frac: copy _2 }; // scope 0 at src/naive/time/mod.rs:1113:14: 1113:47
        _0 = Option::<NaiveTime>::Some(move _4); // scope 0 at src/naive/time/mod.rs:1113:9: 1113:48
        goto -> bb3;                     // scope 0 at src/naive/time/mod.rs:1114:6: 1114:6
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at src/naive/time/mod.rs:1114:6: 1114:6
        return;                          // scope 0 at src/naive/time/mod.rs:1114:6: 1114:6
    }
}

// MIR FOR CTFE
fn offset::fixed::<impl at src/offset/fixed.rs:34:1: 34:17>::east_opt(_1: i32) -> Option<offset::fixed::FixedOffset> {
    debug secs => _1;                    // in scope 0 at src/offset/fixed.rs:62:27: 62:31
    let mut _0: std::option::Option<offset::fixed::FixedOffset>; // return place in scope 0 at src/offset/fixed.rs:62:41: 62:60
    let mut _2: bool;                    // in scope 0 at src/offset/fixed.rs:63:12: 63:26
    let mut _3: i32;                     // in scope 0 at src/offset/fixed.rs:63:22: 63:26
    let mut _4: bool;                    // in scope 0 at src/offset/fixed.rs:63:30: 63:43
    let mut _5: i32;                     // in scope 0 at src/offset/fixed.rs:63:30: 63:34
    let mut _6: offset::fixed::FixedOffset; // in scope 0 at src/offset/fixed.rs:64:18: 64:55
    let mut _7: i32;                     // in scope 0 at src/offset/fixed.rs:64:49: 64:53

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/offset/fixed.rs:63:12: 63:26 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => src/offset/fixed.rs:63:30: 63:43 (#0)

    coverage Code { bcb: bcb0 } => src/offset/fixed.rs:62:5: 62:60 (#0);
    coverage Code { bcb: bcb0 } => src/offset/fixed.rs:63:12: 63:26 (#0);
    coverage Code { bcb: bcb1 } => src/offset/fixed.rs:63:30: 63:43 (#0);
    coverage Code { bcb: bcb2 } => src/offset/fixed.rs:64:13: 64:56 (#0);
    coverage Code { bcb: bcb5 } => src/offset/fixed.rs:66:13: 66:17 (#0);
    coverage Code { bcb: bcb3 } => src/offset/fixed.rs:68:5: 68:6 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb6 } => src/offset/fixed.rs:63:12: 63:26 (#0);
    coverage Branch { true_bcb: bcb2, false_bcb: bcb4 } => src/offset/fixed.rs:63:30: 63:43 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/offset/fixed.rs:63:12: 63:26
        StorageLive(_2);                 // scope 0 at src/offset/fixed.rs:63:12: 63:26
        StorageLive(_3);                 // scope 0 at src/offset/fixed.rs:63:22: 63:26
        _3 = copy _1;                    // scope 0 at src/offset/fixed.rs:63:22: 63:26
        _2 = Lt(const -86400_i32, move _3); // scope 0 at src/offset/fixed.rs:63:12: 63:26
        switchInt(move _2) -> [0: bb2, otherwise: bb1]; // scope 0 at src/offset/fixed.rs:63:12: 63:26
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/offset/fixed.rs:63:30: 63:43
        StorageDead(_3);                 // scope 0 at src/offset/fixed.rs:63:25: 63:26
        StorageLive(_4);                 // scope 0 at src/offset/fixed.rs:63:30: 63:43
        StorageLive(_5);                 // scope 0 at src/offset/fixed.rs:63:30: 63:34
        _5 = copy _1;                    // scope 0 at src/offset/fixed.rs:63:30: 63:34
        _4 = Lt(move _5, const 86400_i32); // scope 0 at src/offset/fixed.rs:63:30: 63:43
        switchInt(move _4) -> [0: bb4, otherwise: bb3]; // scope 0 at src/offset/fixed.rs:63:30: 63:43
    }

    bb2: {
        Coverage::VirtualCounter(bcb6);  // scope 0 at no-location
        StorageDead(_3);                 // scope 0 at src/offset/fixed.rs:63:25: 63:26
        goto -> bb5;                     // scope 0 at no-location
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at src/offset/fixed.rs:63:9: 67:10
        StorageDead(_5);                 // scope 0 at src/offset/fixed.rs:63:42: 63:43
        StorageLive(_6);                 // scope 0 at src/offset/fixed.rs:64:18: 64:55
        StorageLive(_7);                 // scope 0 at src/offset/fixed.rs:64:49: 64:53
        _7 = copy _1;                    // scope 0 at src/offset/fixed.rs:64:49: 64:53
        _6 = offset::fixed::FixedOffset { local_minus_utc: move _7 }; // scope 0 at src/offset/fixed.rs:64:18: 64:55
        StorageDead(_7);                 // scope 0 at src/offset/fixed.rs:64:54: 64:55
        _0 = Option::<offset::fixed::FixedOffset>::Some(move _6); // scope 0 at src/offset/fixed.rs:64:13: 64:56
        StorageDead(_6);                 // scope 0 at src/offset/fixed.rs:64:55: 64:56
        goto -> bb6;                     // scope 0 at src/offset/fixed.rs:63:9: 67:10
    }

    bb4: {
        Coverage::VirtualCounter(bcb4);  // scope 0 at no-location
        StorageDead(_5);                 // scope 0 at src/offset/fixed.rs:63:42: 63:43
        goto -> bb5;                     // scope 0 at no-location
    }

    bb5: {
        Coverage::VirtualCounter(bcb5);  // scope 0 at src/offset/fixed.rs:63:9: 67:10
        _0 = Option::<offset::fixed::FixedOffset>::None; // scope 0 at src/offset/fixed.rs:66:13: 66:17
        goto -> bb6;                     // scope 0 at src/offset/fixed.rs:63:9: 67:10
    }

    bb6: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at src/offset/fixed.rs:68:6: 68:6
        StorageDead(_4);                 // scope 0 at src/offset/fixed.rs:67:9: 67:10
        StorageDead(_2);                 // scope 0 at src/offset/fixed.rs:67:9: 67:10
        return;                          // scope 0 at src/offset/fixed.rs:68:6: 68:6
    }
}
"#;

    const CHRONO_ROOT: &str = "/home/ubuntu/work/verify-span/chrono/src-copy";

    fn chrono_paths() -> PathNormaliser {
        PathNormaliser::new(Some(std::path::Path::new(CHRONO_ROOT)))
    }

    /// Real whole-suite record for `RuleDay::julian_0` (8 calls, the
    /// condition always false), with the entry count and the
    /// condition's counters substitutable.
    fn julian_0_record(count: u64, cond: (u64, u64)) -> String {
        format!(
            r#"{{"name": "_RNvMs2_NtNtNtNtCseI4Ere5mNTx_6chrono6offset5local7tz_info4ruleNtB5_7RuleDay8julian_0", "count": {count}, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/offset/local/tz_info/rule.rs"], "regions": [[500, 5, 500, 64, {count}, 0, 0, 0], [501, 12, 501, 30, {count}, 0, 0, 0], [502, 13, 502, 77, {}, 0, 0, 0], [503, 9, 503, 10, {}, 0, 0, 0], [505, 9, 505, 51, {}, 0, 0, 0], [506, 5, 506, 6, {count}, 0, 0, 0]], "branches": [[501, 12, 501, 30, {}, {}, 0, 0, 4]]}}"#,
            cond.0, cond.1, cond.1, cond.0, cond.1
        )
    }

    const WITH_NANOSECOND_RECORD: &str = r#"{"name": "_RNvXs_NtNtCseI4Ere5mNTx_6chrono5naive4timeNtB4_9NaiveTimeNtNtB8_6traits8Timelike15with_nanosecondB8_", "count": 66, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/naive/time/mod.rs"], "regions": [[1109, 5, 1109, 62, 66, 0, 0, 0], [1110, 12, 1110, 33, 66, 0, 0, 0], [1111, 13, 1111, 24, 0, 0, 0, 0], [1112, 9, 1112, 10, 66, 0, 0, 0], [1113, 9, 1113, 48, 66, 0, 0, 0], [1114, 5, 1114, 6, 66, 0, 0, 0]], "branches": [[1110, 12, 1110, 33, 0, 66, 0, 0, 4]]}"#;

    fn export(functions: &[&str]) -> CoverageReport {
        let json = format!(
            r#"{{"data": [{{"files": [], "functions": [{}], "totals": {{}}}}],
                "type": "llvm.coverage.json.export", "version": "2.0.1"}}"#,
            functions.join(",")
        );
        profile::parse(&json).expect("coverage parses")
    }

    fn chrono_candidate(mir: &Mir, name_part: &str) -> (usize, DecisionTree) {
        let (idx, f) = mir
            .functions
            .iter()
            .enumerate()
            .find(|(_, f)| f.name.contains(name_part))
            .expect("function in fixture");
        let node = decision::decompose_function(f).expect("decomposes");
        (
            idx,
            DecisionTree {
                decisions: vec![node],
            },
        )
    }

    #[test]
    fn mir_counts_coverage_mappings_not_branch_markers() {
        let mir = mir::parse_text(CHRONO_MIR).expect("MIR parses");
        let counts: Vec<usize> = mir.functions.iter().map(|f| f.coverage_mappings).collect();
        assert_eq!(counts, [7, 7, 8]);
        let first = &mir.functions[0].coverage_spans[0];
        assert_eq!(first.file, "src/offset/local/tz_info/rule.rs");
        assert_eq!(
            (
                first.start_line,
                first.start_col,
                first.end_line,
                first.end_col
            ),
            (500, 5, 500, 64)
        );
    }

    #[test]
    fn impl_method_is_invoked_and_observed_through_the_span_join() {
        // MIR name `rule::<impl at src/offset/local/tz_info/rule.rs:460:1: 460:13>::julian_0`
        // never matched `<chrono::offset::local::tz_info::rule::RuleDay>::julian_0`
        // by name, so the baseline reported it never invoked.
        let mir = mir::parse_text(CHRONO_MIR).expect("MIR parses");
        let paths = chrono_paths();
        let prepared = prepare_mir(&mir, &paths);
        let (idx, tree) = chrono_candidate(&mir, "julian_0");
        let tests = vec![(
            "offset::local::tz_info::rule::tests::test_rule_day".to_string(),
            CoverageIndex::build(&export(&[&julian_0_record(1, (0, 1))]), &paths),
        )];
        let c = join_candidate(&prepared, idx, &tree, &tests);
        assert_eq!(c.status, CoverageStatus::Invoked);
        assert_eq!(
            c.display_name.as_deref(),
            Some("<chrono::offset::local::tz_info::rule::RuleDay>::julian_0")
        );
        assert_eq!(c.observations.len(), 1);
        assert!(!c.observations[0].ambiguous);
        assert_eq!(c.observations[0].evaluations, Some(1));
        assert_eq!(c.observations[0].result, Some(false));
    }

    #[test]
    fn trait_impl_method_is_invoked_and_observed_through_the_span_join() {
        let mir = mir::parse_text(CHRONO_MIR).expect("MIR parses");
        let paths = chrono_paths();
        let prepared = prepare_mir(&mir, &paths);
        let (idx, tree) = chrono_candidate(&mir, "with_nanosecond");
        let tests = vec![(
            "suite".to_string(),
            CoverageIndex::build(&export(&[WITH_NANOSECOND_RECORD]), &paths),
        )];
        let c = join_candidate(&prepared, idx, &tree, &tests);
        assert_eq!(c.status, CoverageStatus::Invoked);
        assert_eq!(
            c.display_name.as_deref(),
            Some("<chrono::naive::time::NaiveTime as chrono::traits::Timelike>::with_nanosecond")
        );
        // Single condition, 66 evaluations, always false: one vector.
        assert_eq!(c.observations.len(), 1);
        assert_eq!(c.observations[0].evaluations, Some(66));
        assert_eq!(c.observations[0].result, Some(false));
    }

    /// `julian_0_record` with the condition's region and branch
    /// (501:12-501:30) removed: a record whose regions do not cover
    /// the decision's condition span.
    const JULIAN_0_RECORD_NO_CONDITION_REGION: &str = r#"{"name": "_RNvMs2_NtNtNtNtCseI4Ere5mNTx_6chrono6offset5local7tz_info4ruleNtB5_7RuleDay8julian_0", "count": 1, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/offset/local/tz_info/rule.rs"], "regions": [[500, 5, 500, 64, 1, 0, 0, 0], [502, 13, 502, 77, 0, 0, 0, 0], [503, 9, 503, 10, 0, 0, 0, 0], [505, 9, 505, 51, 1, 0, 0, 0], [506, 5, 506, 6, 1, 0, 0, 0]], "branches": []}"#;

    #[test]
    fn mapping_spans_find_the_record_when_the_condition_span_has_none() {
        // The decision `julian_day_0 > 365` sits at 501:12-501:30,
        // which no region of this record covers. The body's coverage
        // mappings still locate julian_0's record: the function ran
        // (entered). The record has no branch region for the
        // decision, so it is not measurable (no observations).
        let mir = mir::parse_text(CHRONO_MIR).expect("MIR parses");
        let paths = chrono_paths();
        let mut prepared = prepare_mir(&mir, &paths);
        let (idx, tree) = chrono_candidate(&mir, "julian_0");
        let tests = vec![(
            "offset::local::tz_info::rule::tests::test_rule_day".to_string(),
            CoverageIndex::build(&export(&[JULIAN_0_RECORD_NO_CONDITION_REGION]), &paths),
        )];
        let c = join_candidate(&prepared, idx, &tree, &tests);
        let CoverageStatus::NotMeasurable(nm) = &c.status else {
            panic!("expected not measurable, got {:?}", c.status);
        };
        assert_eq!(nm.reason, NotMeasurableReason::NoBranchRegion);
        assert!(c.entered);
        assert_eq!(c.function_keys.len(), 1);
        assert!(c.observations.is_empty());
        assert_eq!(
            c.display_name.as_deref(),
            Some("<chrono::offset::local::tz_info::rule::RuleDay>::julian_0")
        );
        // Without the mappings the decision span alone finds nothing.
        prepared.mapping_spans[idx].clear();
        let c = join_candidate(&prepared, idx, &tree, &tests);
        assert!(matches!(
            c.status,
            CoverageStatus::NotMeasurable(NotMeasurable {
                reason: NotMeasurableReason::NoCoverageRecord,
                ..
            })
        ));
    }

    #[test]
    fn ctfe_operand_copy_is_not_a_candidate() {
        // east_opt's `_3 = copy _1` feeds the `Lt` compare in the same
        // block; it is operand set-up, not the block's value, so the
        // body yields no single-condition `secs` decision.
        let mir = mir::parse_text(CHRONO_MIR).expect("MIR parses");
        let f = mir
            .functions
            .iter()
            .find(|f| f.name.contains("east_opt"))
            .expect("function in fixture");
        // The two comparisons are its conditions (value-arm lift),
        // and `secs` is not one.
        let node = decision::decompose_function(f).expect("value arms lift");
        assert_eq!(
            crate::masking::collect_conditions(&node),
            ["-86400 < secs", "secs < 86400"]
        );
        // It is the `// MIR FOR CTFE` body, so it is declined as a
        // candidate before decomposition; the two runtime bodies
        // before it are not tagged.
        assert!(f.is_ctfe);
        assert_eq!(
            decision::try_decompose_candidate(f),
            Err(decision::DeclineReason::CtfeBody)
        );
        let tagged: Vec<bool> = mir.functions.iter().map(|f| f.is_ctfe).collect();
        assert_eq!(tagged, [false, false, true]);
    }

    #[test]
    fn invoked_and_observations_come_from_the_same_join() {
        let mir = mir::parse_text(CHRONO_MIR).expect("MIR parses");
        let paths = chrono_paths();
        let prepared = prepare_mir(&mir, &paths);
        let (idx, tree) = chrono_candidate(&mir, "julian_0");
        // t0 does not call julian_0 (record present, count 0); t1
        // calls it once; t2's binary does not link it.
        let tests = vec![
            (
                "t0".to_string(),
                CoverageIndex::build(&export(&[&julian_0_record(0, (0, 0))]), &paths),
            ),
            (
                "t1".to_string(),
                CoverageIndex::build(&export(&[&julian_0_record(1, (1, 0))]), &paths),
            ),
            (
                "t2".to_string(),
                CoverageIndex::build(&export(&[WITH_NANOSECOND_RECORD]), &paths),
            ),
        ];
        let c = join_candidate(&prepared, idx, &tree, &tests);
        assert!(c.invoked());
        assert!(c.conditions_without_branch_region.is_empty());
        let tests_with_obs: Vec<_> = c
            .observations
            .iter()
            .filter_map(|o| o.test_name.as_deref())
            .collect();
        assert_eq!(tests_with_obs, ["t1"]);
        assert_eq!(c.function_keys.len(), 1);

        // Only t0: a record exists and nobody entered it.
        let c = join_candidate(&prepared, idx, &tree, &tests[..1]);
        assert_eq!(c.status, CoverageStatus::NeverInvoked);
        assert!(c.observations.is_empty());
    }

    #[test]
    fn candidate_without_any_record_is_not_measurable_not_unreached() {
        let mir = mir::parse_text(CHRONO_MIR).expect("MIR parses");
        let paths = chrono_paths();
        let prepared = prepare_mir(&mir, &paths);
        let (idx, tree) = chrono_candidate(&mir, "julian_0");
        let tests = vec![(
            "t".to_string(),
            CoverageIndex::build(&export(&[WITH_NANOSECOND_RECORD]), &paths),
        )];
        let c = join_candidate(&prepared, idx, &tree, &tests);
        let CoverageStatus::NotMeasurable(nm) = &c.status else {
            panic!("expected not measurable, got {:?}", c.status);
        };
        assert_eq!(nm.reason, NotMeasurableReason::NoCoverageRecord);
        assert!(!c.invoked());
    }

    /// can-dbc's serde-derived `__FieldVisitor::visit_u64` for
    /// `AttrDefault` (src/ast/attr_default.rs:5, `#[derive(Deserialize)]`),
    /// from the same instrumented MIR as instrumented functions: no
    /// coverage mapping, because rustc skips
    /// `#[automatically_derived]` impls.
    const DBC_DERIVE_MIR: &str = r#"
fn attr_default::_::<impl at src/ast/attr_default.rs:5:56: 5:74>::deserialize::<impl at src/ast/attr_default.rs:5:56: 5:74>::visit_u64(_1: attr_default::_::<impl Deserialize<'de> for AttrDefault>::deserialize::__FieldVisitor, _2: u64) -> Result<attr_default::_::<impl Deserialize<'de> for AttrDefault>::deserialize::__Field, __E> {
    debug self => const attr_default::_::<impl Deserialize<'de> for AttrDefault>::deserialize::__FieldVisitor; // in scope 0 at src/ast/attr_default.rs:5:56: 5:74
    debug __value => _2;                 // in scope 0 at src/ast/attr_default.rs:5:56: 5:74
    let mut _0: std::result::Result<ast::attr_default::_::<impl serde::Deserialize<'de> for ast::attr_default::AttrDefault>::deserialize::__Field, __E>; // return place in scope 0 at src/ast/attr_default.rs:5:56: 5:74
    let mut _3: ast::attr_default::_::<impl serde::Deserialize<'de> for ast::attr_default::AttrDefault>::deserialize::__Field; // in scope 0 at src/ast/attr_default.rs:5:56: 5:74
    let mut _4: ast::attr_default::_::<impl serde::Deserialize<'de> for ast::attr_default::AttrDefault>::deserialize::__Field; // in scope 0 at src/ast/attr_default.rs:5:56: 5:74
    let mut _5: ast::attr_default::_::<impl serde::Deserialize<'de> for ast::attr_default::AttrDefault>::deserialize::__Field; // in scope 0 at src/ast/attr_default.rs:5:56: 5:74

    bb0: {
        switchInt(copy _2) -> [0: bb3, 1: bb2, otherwise: bb1]; // scope 0 at src/ast/attr_default.rs:5:56: 5:74
    }

    bb1: {
        _5 = attr_default::_::<impl Deserialize<'de> for AttrDefault>::deserialize::__Field::__ignore; // scope 0 at src/ast/attr_default.rs:5:56: 5:74
        _0 = Result::<attr_default::_::<impl Deserialize<'de> for AttrDefault>::deserialize::__Field, __E>::Ok(move _5); // scope 0 at src/ast/attr_default.rs:5:56: 5:74
        goto -> bb4;                     // scope 0 at src/ast/attr_default.rs:5:73: 5:74
    }

    bb2: {
        _4 = attr_default::_::<impl Deserialize<'de> for AttrDefault>::deserialize::__Field::__field1; // scope 0 at src/ast/attr_default.rs:5:56: 5:74
        _0 = Result::<attr_default::_::<impl Deserialize<'de> for AttrDefault>::deserialize::__Field, __E>::Ok(move _4); // scope 0 at src/ast/attr_default.rs:5:56: 5:74
        goto -> bb4;                     // scope 0 at src/ast/attr_default.rs:5:73: 5:74
    }

    bb3: {
        _3 = attr_default::_::<impl Deserialize<'de> for AttrDefault>::deserialize::__Field::__field0; // scope 0 at src/ast/attr_default.rs:5:56: 5:74
        _0 = Result::<attr_default::_::<impl Deserialize<'de> for AttrDefault>::deserialize::__Field, __E>::Ok(move _3); // scope 0 at src/ast/attr_default.rs:5:56: 5:74
        goto -> bb4;                     // scope 0 at src/ast/attr_default.rs:5:73: 5:74
    }

    bb4: {
        return;                          // scope 0 at src/ast/attr_default.rs:5:74: 5:74
    }
}
"#;

    #[test]
    fn derive_body_without_coverage_mapping_is_uninstrumented() {
        let text = format!("{CHRONO_MIR}\n{DBC_DERIVE_MIR}");
        let mir = mir::parse_text(&text).expect("MIR parses");
        let paths = chrono_paths();
        let prepared = prepare_mir(&mir, &paths);
        let derive_idx = mir
            .functions
            .iter()
            .position(|f| f.name.ends_with("::visit_u64"))
            .expect("derive body parsed");
        assert_eq!(mir.functions[derive_idx].coverage_mappings, 0);
        let instrumented = mir.functions.iter().any(|f| f.coverage_mappings > 0);
        assert!(instrumented);
        let tree = DecisionTree::default();
        let nm = mir_not_measurable(&prepared, derive_idx, &tree, instrumented, Some(&paths))
            .expect("not measurable");
        assert_eq!(nm.reason, NotMeasurableReason::Uninstrumented);
        // An instrumented body is left to the join.
        let (east, east_tree) = chrono_candidate(&mir, "julian_0");
        assert_eq!(
            mir_not_measurable(&prepared, east, &east_tree, instrumented, Some(&paths)),
            None
        );
        // MIR from an uninstrumented compilation carries no
        // mappings at all; the signal is then not used.
        assert_eq!(
            mir_not_measurable(&prepared, derive_idx, &tree, false, Some(&paths)),
            None
        );
    }

    #[test]
    fn decision_expanded_from_a_dependency_macro_is_external() {
        // An instrumented closure whose condition spans are all in a
        // registry crate's macro (shape of can-dbc's insta
        // `with_settings!` closure in tests/snapshots.rs).
        let text = r#"
fn parse_one_file::{closure#0}(_1: bool) -> bool {
    debug check => _1;                   // in scope 0 at /home/ubuntu/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/insta-1.49.0/src/macros.rs:11:19: 11:29
    let mut _0: bool;

    coverage Code { bcb: bcb0 } => tests/snapshots.rs:170:17: 170:29 (#0);

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at /home/ubuntu/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/insta-1.49.0/src/macros.rs:11:19: 11:29
    }

    bb1: {
        _0 = const true;
        goto -> bb3;
    }

    bb2: {
        _0 = const false;
        goto -> bb3;
    }

    bb3: {
        return;
    }
}
"#;
        let mir = mir::parse_text(text).expect("MIR parses");
        let paths = PathNormaliser::new(Some(std::path::Path::new(
            "/home/ubuntu/work/verify-span/dbc-parser/src-copy",
        )));
        let prepared = prepare_mir(&mir, &paths);
        let node = decision::decompose_function(&mir.functions[0]).expect("decomposes");
        let tree = DecisionTree {
            decisions: vec![node],
        };
        let nm = mir_not_measurable(&prepared, 0, &tree, true, Some(&paths)).expect("classified");
        assert_eq!(nm.reason, NotMeasurableReason::ExternalMacro);
        // Without a workspace root (single-file mode) it is left to
        // the join.
        assert_eq!(mir_not_measurable(&prepared, 0, &tree, true, None), None);
    }

    #[test]
    fn two_candidates_on_one_coverage_function_are_not_measurable() {
        let mir = mir::parse_text(CHRONO_MIR).expect("MIR parses");
        let paths = chrono_paths();
        let prepared = prepare_mir(&mir, &paths);
        let (idx, tree) = chrono_candidate(&mir, "julian_0");
        let tests = vec![(
            "t".to_string(),
            CoverageIndex::build(&export(&[&julian_0_record(1, (0, 1))]), &paths),
        )];
        let mut a = join_candidate(&prepared, idx, &tree, &tests);
        let mut b = a.clone();
        let mut same_name = a.clone();
        assert_eq!(a.status, CoverageStatus::Invoked);

        // A `const fn`'s runtime and CTFE bodies share a name: one
        // source function, still measured.
        mark_shared_coverage(&mut [("julian_0", &mut a), ("julian_0", &mut same_name)]);
        assert_eq!(a.status, CoverageStatus::Invoked);
        assert_eq!(same_name.status, CoverageStatus::Invoked);

        // Different MIR functions on one coverage function: neither
        // may be credited.
        mark_shared_coverage(&mut [("julian_0", &mut a), ("other", &mut b)]);
        for c in [&a, &b] {
            let CoverageStatus::NotMeasurable(nm) = &c.status else {
                panic!("expected not measurable, got {:?}", c.status);
            };
            assert_eq!(nm.reason, NotMeasurableReason::AmbiguousSpans);
            assert!(c.observations.is_empty());
            assert!(!c.invoked());
        }
    }

    #[test]
    fn ambiguous_resolution_drops_observations() {
        // Two distinct records at julian_0's spans with different
        // names (as macro-generated twins would have).
        let mir = mir::parse_text(CHRONO_MIR).expect("MIR parses");
        let paths = chrono_paths();
        let prepared = prepare_mir(&mir, &paths);
        let (idx, tree) = chrono_candidate(&mir, "julian_0");
        let twin = julian_0_record(1, (0, 1)).replace("8julian_0", "8julian_1");
        let tests = vec![(
            "t".to_string(),
            CoverageIndex::build(&export(&[&julian_0_record(1, (0, 1)), &twin]), &paths),
        )];
        let c = join_candidate(&prepared, idx, &tree, &tests);
        let CoverageStatus::NotMeasurable(nm) = &c.status else {
            panic!("expected not measurable, got {:?}", c.status);
        };
        assert_eq!(nm.reason, NotMeasurableReason::AmbiguousSpans);
        assert!(c.observations.is_empty());
    }

    /// BLAKE3 `src/lib.rs`, `cargo floyd test` MIR (comment-only
    /// lines stripped) for
    ///
    /// ```text
    /// impl IncrementCounter {
    ///     fn yes(&self) -> bool {
    ///         match self {
    ///             IncrementCounter::Yes => true,
    ///             IncrementCounter::No => false,
    ///         }
    ///     }
    /// }
    /// ```
    const INCREMENT_COUNTER_YES_MIR: &str = r#"
fn <impl at src/lib.rs:699:1: 699:22>::yes(_1: &IncrementCounter) -> bool {
    debug self => _1;                    // in scope 0 at src/lib.rs:701:12: 701:17
    let mut _0: bool;                    // return place in scope 0 at src/lib.rs:701:22: 701:26
    let mut _2: isize;                   // in scope 0 at src/lib.rs:703:13: 703:34

    coverage Code { bcb: bcb0 } => src/lib.rs:701:5: 701:26 (#0);
    coverage Code { bcb: bcb0 } => src/lib.rs:702:15: 702:19 (#0);
    coverage Code { bcb: bcb3 } => src/lib.rs:703:38: 703:42 (#0);
    coverage Code { bcb: bcb1 } => src/lib.rs:704:37: 704:42 (#0);
    coverage Code { bcb: bcb2 } => src/lib.rs:706:5: 706:6 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/lib.rs:702:9: 702:19
        _2 = discriminant((*_1));        // scope 0 at src/lib.rs:702:15: 702:19
        switchInt(move _2) -> [0: bb2, 1: bb3, otherwise: bb1]; // scope 0 at src/lib.rs:702:9: 702:19
    }

    bb1: {
        unreachable;                     // scope 0 at src/lib.rs:702:15: 702:19
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at src/lib.rs:703:13: 703:34
        _0 = const true;                 // scope 0 at src/lib.rs:703:38: 703:42
        goto -> bb4;                     // scope 0 at src/lib.rs:703:38: 703:42
    }

    bb3: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/lib.rs:704:37: 704:42
        _0 = const false;                // scope 0 at src/lib.rs:704:37: 704:42
        goto -> bb4;                     // scope 0 at src/lib.rs:704:37: 704:42
    }

    bb4: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at src/lib.rs:706:6: 706:6
        return;                          // scope 0 at src/lib.rs:706:6: 706:6
    }
}
"#;

    /// Its record in `avx2::test::test_hash_many`'s coverage: code
    /// regions only (the two arms 96 / 96), no branch region.
    const INCREMENT_COUNTER_YES_RECORD: &str = r#"{"name": "_RNvMsf_CsgejNOghg1Tz_6blake3NtB5_16IncrementCounter3yesB5_", "count": 192, "filenames": ["/home/ubuntu/work/validation/BLAKE3/src/lib.rs"], "regions": [[701, 5, 701, 26, 192, 0, 0, 0], [702, 15, 702, 19, 192, 0, 0, 0], [703, 38, 703, 42, 96, 0, 0, 0], [704, 37, 704, 42, 96, 0, 0, 0], [706, 5, 706, 6, 192, 0, 0, 0]], "branches": []}"#;

    #[test]
    fn enum_match_without_a_branch_region_is_not_measurable() {
        let mir = mir::parse_text(INCREMENT_COUNTER_YES_MIR).expect("MIR parses");
        let paths = PathNormaliser::new(Some(std::path::Path::new(
            "/home/ubuntu/work/validation/BLAKE3",
        )));
        let prepared = prepare_mir(&mir, &paths);
        let node = decision::try_decompose_candidate(&mir.functions[0]).expect("a candidate");
        let tree = DecisionTree {
            decisions: vec![node],
        };
        let tests = vec![(
            "avx2::test::test_hash_many".to_string(),
            CoverageIndex::build(&export(&[INCREMENT_COUNTER_YES_RECORD]), &paths),
        )];
        let c = join_candidate(&prepared, 0, &tree, &tests);
        let CoverageStatus::NotMeasurable(nm) = &c.status else {
            panic!("expected not measurable, got {:?}", c.status);
        };
        assert_eq!(nm.reason, NotMeasurableReason::NoBranchRegion);
        assert!(
            nm.detail.ends_with("the function was entered"),
            "{}",
            nm.detail
        );
        assert!(c.entered);
        assert!(!c.invoked());
        assert!(c.observations.is_empty());
        assert_eq!(
            c.conditions_without_branch_region,
            crate::masking::collect_conditions(&tree.decisions[0])
        );
        assert_eq!(
            serde_json::to_value(NotMeasurableReason::NoBranchRegion).unwrap(),
            "no_branch_region"
        );

        // Not entered by any test: still not measurable, and the
        // detail says so.
        let idle = INCREMENT_COUNTER_YES_RECORD
            .replace("192", "0")
            .replace("96", "0");
        let tests = vec![(
            "t".to_string(),
            CoverageIndex::build(&export(&[&idle]), &paths),
        )];
        let c = join_candidate(&prepared, 0, &tree, &tests);
        let CoverageStatus::NotMeasurable(nm) = &c.status else {
            panic!("expected not measurable, got {:?}", c.status);
        };
        assert_eq!(nm.reason, NotMeasurableReason::NoBranchRegion);
        assert!(nm.detail.ends_with("never entered"), "{}", nm.detail);
        assert!(!c.entered);
    }

    /// The runtime body of chrono's `const fn FixedOffset::east_opt`
    /// (`if -86_400 < secs && secs < 86_400 { Some(..) } else { None
    /// }`), from the same compilation as [`CHRONO_MIR`], comment
    /// lines stripped.
    const EAST_OPT_RUNTIME_MIR: &str = r#"
fn offset::fixed::<impl at src/offset/fixed.rs:34:1: 34:17>::east_opt(_1: i32) -> Option<offset::fixed::FixedOffset> {
    debug secs => _1;                    // in scope 0 at src/offset/fixed.rs:62:27: 62:31
    let mut _0: std::option::Option<offset::fixed::FixedOffset>; // return place in scope 0 at src/offset/fixed.rs:62:41: 62:60
    let mut _2: bool;                    // in scope 0 at src/offset/fixed.rs:63:12: 63:26
    let mut _3: bool;                    // in scope 0 at src/offset/fixed.rs:63:30: 63:43
    let mut _4: offset::fixed::FixedOffset; // in scope 0 at src/offset/fixed.rs:64:18: 64:55

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/offset/fixed.rs:63:12: 63:26 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => src/offset/fixed.rs:63:30: 63:43 (#0)

    coverage Code { bcb: bcb0 } => src/offset/fixed.rs:62:5: 62:60 (#0);
    coverage Code { bcb: bcb0 } => src/offset/fixed.rs:63:12: 63:26 (#0);
    coverage Code { bcb: bcb1 } => src/offset/fixed.rs:63:30: 63:43 (#0);
    coverage Code { bcb: bcb2 } => src/offset/fixed.rs:64:13: 64:56 (#0);
    coverage Code { bcb: bcb5 } => src/offset/fixed.rs:66:13: 66:17 (#0);
    coverage Code { bcb: bcb3 } => src/offset/fixed.rs:68:5: 68:6 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb6 } => src/offset/fixed.rs:63:12: 63:26 (#0);
    coverage Branch { true_bcb: bcb2, false_bcb: bcb4 } => src/offset/fixed.rs:63:30: 63:43 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/offset/fixed.rs:63:12: 63:26
        _2 = Lt(const -86400_i32, copy _1); // scope 0 at src/offset/fixed.rs:63:12: 63:26
        switchInt(move _2) -> [0: bb2, otherwise: bb1]; // scope 0 at src/offset/fixed.rs:63:12: 63:26
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/offset/fixed.rs:63:30: 63:43
        _3 = Lt(copy _1, const 86400_i32); // scope 0 at src/offset/fixed.rs:63:30: 63:43
        switchInt(move _3) -> [0: bb4, otherwise: bb3]; // scope 0 at src/offset/fixed.rs:63:30: 63:43
    }

    bb2: {
        Coverage::VirtualCounter(bcb6);  // scope 0 at no-location
        goto -> bb5;                     // scope 0 at no-location
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at src/offset/fixed.rs:63:9: 67:10
        _4 = offset::fixed::FixedOffset { local_minus_utc: copy _1 }; // scope 0 at src/offset/fixed.rs:64:18: 64:55
        _0 = Option::<offset::fixed::FixedOffset>::Some(move _4); // scope 0 at src/offset/fixed.rs:64:13: 64:56
        goto -> bb6;                     // scope 0 at src/offset/fixed.rs:63:9: 67:10
    }

    bb4: {
        Coverage::VirtualCounter(bcb4);  // scope 0 at no-location
        goto -> bb5;                     // scope 0 at no-location
    }

    bb5: {
        Coverage::VirtualCounter(bcb5);  // scope 0 at src/offset/fixed.rs:63:9: 67:10
        _0 = Option::<offset::fixed::FixedOffset>::None; // scope 0 at src/offset/fixed.rs:66:13: 66:17
        goto -> bb6;                     // scope 0 at src/offset/fixed.rs:63:9: 67:10
    }

    bb6: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at src/offset/fixed.rs:68:6: 68:6
        return;                          // scope 0 at src/offset/fixed.rs:68:6: 68:6
    }
}
"#;

    /// `east_opt`'s whole-suite record from chrono's `llvm-cov
    /// export`: branch regions at both condition spans.
    const EAST_OPT_SUITE_RECORD: &str = r#"{"branches": [[63, 12, 63, 26, 2338, 1, 0, 0, 4], [63, 30, 63, 43, 2337, 1, 0, 0, 4]], "count": 2339, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/offset/fixed.rs"], "mcdc_records": [], "name": "_RNvMNtNtCseI4Ere5mNTx_6chrono6offset5fixedNtB2_11FixedOffset8east_opt", "regions": [[62, 5, 62, 60, 2339, 0, 0, 0], [63, 12, 63, 26, 2339, 0, 0, 0], [63, 30, 63, 43, 2338, 0, 0, 0], [64, 13, 64, 56, 2337, 0, 0, 0], [66, 13, 66, 17, 2, 0, 0, 0], [68, 5, 68, 6, 2339, 0, 0, 0]]}"#;

    /// [`EAST_OPT_SUITE_RECORD`] for one call, with the two
    /// conditions' `(true, false)` counters substitutable.
    fn east_opt_record(a: (u64, u64), b: (u64, u64)) -> String {
        EAST_OPT_SUITE_RECORD
            .replace(
                "[[63, 12, 63, 26, 2338, 1, 0, 0, 4], [63, 30, 63, 43, 2337, 1, 0, 0, 4]]",
                &format!(
                    "[[63, 12, 63, 26, {}, {}, 0, 0, 4], [63, 30, 63, 43, {}, {}, 0, 0, 4]]",
                    a.0, a.1, b.0, b.1
                ),
            )
            .replace("2339", "1")
    }

    #[test]
    fn value_arm_chain_joins_both_conditions_by_span() {
        let mir = mir::parse_text(EAST_OPT_RUNTIME_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        assert!(!f.is_ctfe);
        let (node, lift) = decision::try_decompose_candidate_lifted(f).expect("a candidate");
        assert_eq!(lift, Some(decision::Lift::ValueArms));
        assert_eq!(
            crate::masking::collect_conditions(&node),
            ["-86400 < secs", "secs < 86400"]
        );
        let tree = DecisionTree {
            decisions: vec![node],
        };
        let paths = chrono_paths();
        let prepared = prepare_mir(&mir, &paths);
        let tests = vec![
            (
                "suite".to_string(),
                CoverageIndex::build(&export(&[EAST_OPT_SUITE_RECORD]), &paths),
            ),
            (
                "in_range".to_string(),
                CoverageIndex::build(&export(&[&east_opt_record((1, 0), (1, 0))]), &paths),
            ),
            (
                "too_low".to_string(),
                CoverageIndex::build(&export(&[&east_opt_record((0, 1), (0, 0))]), &paths),
            ),
            (
                "too_high".to_string(),
                CoverageIndex::build(&export(&[&east_opt_record((1, 0), (0, 1))]), &paths),
            ),
        ];
        let c = join_candidate(&prepared, 0, &tree, &tests);
        assert_eq!(c.status, CoverageStatus::Invoked);
        assert!(c.conditions_without_branch_region.is_empty());
        assert_eq!(
            c.display_name.as_deref(),
            Some("<chrono::offset::fixed::FixedOffset>::east_opt")
        );
        // The suite's totals mix 2339 evaluations of two conditions,
        // both two-sided; the chain is a tree, so its edge flows give
        // (T,T) x2337, (T,F) x1 and (F,-) x1. Each single-call test
        // yields its vector, and the arm taken is the outcome.
        assert_eq!(c.observations.len(), 6);
        let suite: Vec<(Option<bool>, Option<u64>)> = c.observations[..3]
            .iter()
            .map(|o| {
                assert_eq!(o.resolved_by, Some(Resolution::TreeCounters));
                (o.result, o.evaluations)
            })
            .collect();
        assert_eq!(
            suite,
            [
                (Some(true), Some(2337)),
                (Some(false), Some(1)),
                (Some(false), Some(1))
            ]
        );
        let results: Vec<Option<bool>> = c.observations[3..].iter().map(|o| o.result).collect();
        assert_eq!(results, [Some(true), Some(false), Some(false)]);
        assert_eq!(
            c.observations[5].inputs,
            BTreeMap::from([
                ("-86400 < secs".to_string(), true),
                ("secs < 86400".to_string(), false)
            ])
        );
    }
}
