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
//! 2. Look up the corresponding [`crate::profile::FunctionCoverage`]
//!    by demangling its symbol and matching the unmangled tail against
//!    the MIR function name.
//! 3. For each `(condition_name, span)` pair, find the matching
//!    [`crate::profile::Branch`] (equal `SourceSpan`) and record its
//!    `true_count` / `false_count`.
//!
//! Source-span equality is structural: same `file`, same start/end
//! line, same start/end column.
//!
//! [ADR-0002]: ../../../architecture/decisions/0002-runtime-pipeline.md

use crate::decision::{self, DecisionTree, Node};
use crate::masking::{BranchCounts, ConditionObservation};
use crate::mir::{Mir, MirFunction, MirStatement, MirTerminator, SourceSpan};
use crate::profile::{CoverageReport, FunctionCoverage};
use std::collections::BTreeMap;

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
    /// The source span where this condition appears in the program.
    pub span: SourceSpan,
    /// Number of times the condition evaluated true across the runs
    /// represented by the [`CoverageReport`].
    pub true_count: u64,
    /// Number of times the condition evaluated false.
    pub false_count: u64,
}

/// Join MIR decisions to runtime coverage data by source span.
///
/// Phase 1 scope: matches functions by unmangled name and conditions
/// by exact `SourceSpan` equality. Macro-expanded conditions, inlined
/// generics, and conditions whose source span shifts under
/// optimisation are out of scope and may yield missing entries —
/// safe failure rather than wrong data.
pub fn correlate(mir: &Mir, coverage: &CoverageReport) -> DecisionMap {
    let mut map = DecisionMap::default();
    for f in &mir.functions {
        let condition_spans = collect_condition_spans(f);
        let cov_fn = find_matching_function(&f.name, coverage);

        let mut conditions = BTreeMap::new();
        for (name, span) in &condition_spans {
            if let Some(cov_fn) = cov_fn {
                if let Some(branch) = cov_fn.branches.iter().find(|b| spans_match(&b.span, span)) {
                    conditions.insert(
                        name.clone(),
                        ConditionRuntime {
                            span: span.clone(),
                            true_count: branch.true_count,
                            false_count: branch.false_count,
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

/// Build the [`ConditionObservation`]s for one test from a
/// single-test [`CoverageReport`], a [`Mir`] tree, and a
/// [`DecisionTree`].
///
/// Joins MIR to coverage with [`correlate`] and reduces the joined
/// per-condition counters for the first decision in `tree` with
/// [`observations_from_counts`]; see there for when the counters
/// yield a condition vector and when the test is recorded as
/// ambiguous. Returns an empty `Vec` if the tree is empty or the
/// test did not evaluate the decision.
///
/// This path joins every MIR function, so it has no single
/// enclosing function whose entry count could cross-check the
/// evaluation count; it relies on the root-condition count alone.
pub fn observations_from_coverage(
    test_name: Option<String>,
    mir: &Mir,
    coverage: &CoverageReport,
    tree: &DecisionTree,
) -> Vec<ConditionObservation> {
    let Some(decision_node) = tree.decisions.first() else {
        return Vec::new();
    };
    let map = correlate(mir, coverage);
    let mut counts: BTreeMap<String, BranchCounts> = BTreeMap::new();
    for f in &map.functions {
        for (name, runtime) in &f.conditions {
            let joined = BranchCounts {
                true_count: runtime.true_count,
                false_count: runtime.false_count,
            };
            // A same-named condition in another function that this
            // test never reached must not mask one it did reach (as
            // in the pre-ambiguity join, which skipped zero counts).
            if joined.total() > 0 || !counts.contains_key(name) {
                counts.insert(name.clone(), joined);
            }
        }
    }
    observations_from_counts(test_name, decision_node, &counts, None)
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
/// ([`FunctionCoverage::count`], when known) counts entries into the
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
/// 4. Otherwise the test is **ambiguous**: recorded with its
///    evaluation count and raw counters, no vector, no result.
///
/// `N == 0` (the test never evaluated the decision) or a root with
/// no counters yields no observation at all. Condition names in
/// `counts` that the decision does not reference are ignored.
pub fn observations_from_counts(
    test_name: Option<String>,
    decision: &Node,
    counts: &BTreeMap<String, BranchCounts>,
    function_count: Option<u64>,
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

    // Rule 4.
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
/// [`observations_from_coverage`] and the lower-level [`correlate`] both
/// rebuild `collect_condition_spans` for every function on every
/// call. On a crate with hundreds of tests and a MIR that holds
/// hundreds of monomorphised functions, Pass 3 rebuilds the same
/// span map `candidates × tests` times — easily tens of millions of
/// redundant walks on a large crate. `PreparedMir` builds every
/// function's span map ONCE at Pass 1 so Pass 3 is just lookup +
/// join against runtime counts.
pub struct PreparedMir<'m> {
    /// The MIR itself. Borrowed to keep the pre-computation cost
    /// out of ownership.
    pub mir: &'m Mir,
    /// Function name → `(name → SourceSpan)` map. Function ordering
    /// matches `mir.functions` for direct indexed access when
    /// callers need it.
    pub function_spans: Vec<BTreeMap<String, SourceSpan>>,
}

/// Build a [`PreparedMir`] for the given MIR, computing each
/// function's condition-span map once up-front.
pub fn prepare_mir(mir: &Mir) -> PreparedMir<'_> {
    let function_spans = mir.functions.iter().map(collect_condition_spans).collect();
    PreparedMir {
        mir,
        function_spans,
    }
}

/// Build the [`ConditionObservation`]s for one candidate and one
/// test's coverage, using pre-computed span data and the candidate's
/// known home-function index.
///
/// The counters are reduced with [`observations_from_counts`], with
/// the home function's entry count as its consistency cross-check:
/// a vector only when the counters determine it, otherwise one
/// ambiguous observation. Empty when the test did not reach the
/// decision.
///
/// The old single-file path iterated EVERY function in
/// the MIR against every call — correct, but on a big crate with
/// hundreds of monomorphised functions and thousands of tests it
/// turned Pass 3 into billions of operations. Pass 3 only cares
/// about the decision rooted in the candidate's own function, so
/// this entry point consults just `prepared.function_spans[fn_idx]`
/// and the matching coverage function — O(branches) per call.
///
/// `prepared_cov` is a per-test coverage lookup table (`name` →
/// `&FunctionCoverage`) that lets us skip the linear
/// `find_matching_function` scan + `rustc_demangle` work on every
/// call. Built once per test's coverage via
/// [`prepare_coverage_lookup`].
pub fn observations_from_prepared_coverage(
    test_name: Option<String>,
    prepared: &PreparedMir<'_>,
    fn_idx: usize,
    prepared_cov: &PreparedCoverage<'_>,
    tree: &DecisionTree,
) -> Vec<ConditionObservation> {
    let (Some(f), Some(spans), Some(decision_node)) = (
        prepared.mir.functions.get(fn_idx),
        prepared.function_spans.get(fn_idx),
        tree.decisions.first(),
    ) else {
        return Vec::new();
    };
    let Some(cov_fn) = prepared_cov.find(&f.name) else {
        return Vec::new();
    };
    let mut counts = BTreeMap::new();
    for (name, span) in spans {
        if let Some(branch) = cov_fn.branches.iter().find(|b| spans_match(&b.span, span)) {
            counts.insert(
                name.clone(),
                BranchCounts {
                    true_count: branch.true_count,
                    false_count: branch.false_count,
                },
            );
        }
    }
    observations_from_counts(test_name, decision_node, &counts, Some(cov_fn.count))
}

/// Per-test coverage lookup accelerator.
///
/// `find_matching_function` on a bare `CoverageReport` is a linear
/// scan over every function in the coverage, demangling each name
/// on every call. On a crate with 100 coverage functions and 1000
/// tests × 10 candidates, that is 1,000,000 `rustc_demangle` calls.
/// `PreparedCoverage` pre-computes a tail-indexed lookup once per
/// test coverage so Pass 3 lookup is O(1) per call.
pub struct PreparedCoverage<'c> {
    /// The raw coverage report.
    pub coverage: &'c CoverageReport,
    /// `tail(demangled)` → index into `coverage.functions`. The tail
    /// is the final `::`-segment or the segment after a legacy
    /// `::h` hash strip; it matches a MIR function name directly.
    tail_lookup: std::collections::HashMap<String, usize>,
}

impl<'c> PreparedCoverage<'c> {
    /// O(1) analogue of a linear scan over `coverage.functions`.
    pub fn find(&self, mir_name: &str) -> Option<&FunctionCoverage> {
        // Exact name first (fast path for coverage symbols that
        // happen to match the mir name verbatim).
        if let Some(f) = self.coverage.functions.iter().find(|c| c.name == mir_name) {
            return Some(f);
        }
        let idx = *self.tail_lookup.get(mir_name)?;
        self.coverage.functions.get(idx)
    }
}

/// Pre-compute [`PreparedCoverage`] for one coverage report.
pub fn prepare_coverage_lookup(coverage: &CoverageReport) -> PreparedCoverage<'_> {
    let mut tail_lookup: std::collections::HashMap<String, usize> =
        std::collections::HashMap::with_capacity(coverage.functions.len() * 2);
    for (idx, c) in coverage.functions.iter().enumerate() {
        let demangled = rustc_demangle::demangle(&c.name).to_string();
        let trimmed = demangled
            .rsplit_once("::h")
            .map(|(prefix, _)| prefix)
            .unwrap_or(demangled.as_str())
            .to_string();
        // Full trimmed name (useful when `mir_name == trimmed`).
        tail_lookup.entry(trimmed.clone()).or_insert(idx);
        // Suffix matches: last `::`-separated segment, and tail
        // starting at any `::`. These mirror the matching strategy
        // in `find_matching_function` so lookup produces the same
        // result for every mir_name that function would accept.
        if let Some(last) = trimmed.rsplit("::").next() {
            tail_lookup.entry(last.to_string()).or_insert(idx);
        }
        // Also index each suffix `::<...>` so `mir_name` that
        // starts midway through a path (e.g. an impl bracket) still
        // lands. We walk the string and index each `::` suffix.
        let mut rest = trimmed.as_str();
        while let Some(pos) = rest.find("::") {
            rest = &rest[pos + 2..];
            tail_lookup.entry(rest.to_string()).or_insert(idx);
        }
    }
    PreparedCoverage {
        coverage,
        tail_lookup,
    }
}

/// Deprecated: use [`observations_from_prepared_coverage`] with
/// [`prepare_coverage_lookup`] for cheap Pass 3 correlation. Kept
/// because the single-source mode and the public API still rely
/// on it.
#[allow(dead_code)]
fn _unused_old_prepared_observation() {}

/// Collect a `name -> SourceSpan` map for every named condition that
/// appears in the function's MIR.
///
/// Both `switchInt` discriminants (condition tests) and `AssignCopy`
/// sources (condition reads) are considered. First occurrence wins.
///
/// Two source spans match if the line/col coordinates are identical
/// AND the file paths are compatible: equal, or one is a suffix of
/// the other. The suffix case handles the common cargo project
/// asymmetry where MIR emits relative paths (`src/lib.rs:2:5: 2:6`)
/// while `llvm-cov export` reports absolute paths
/// (`/abs/.../src/lib.rs:2:5: 2:6`).
fn spans_match(a: &SourceSpan, b: &SourceSpan) -> bool {
    if (a.start_line, a.start_col, a.end_line, a.end_col)
        != (b.start_line, b.start_col, b.end_line, b.end_col)
    {
        return false;
    }
    a.file == b.file || a.file.ends_with(&b.file) || b.file.ends_with(&a.file)
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
    }
}

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

/// Find the [`crate::profile::FunctionCoverage`] entry whose
/// (demangled) symbol name matches the given MIR function name.
///
/// Matching strategy, in order:
/// 1. Exact match on the symbol as it appears in coverage.
/// 2. Demangle via [`rustc_demangle`] and match the tail
///    (`<crate>::<...>::<name>` or `<crate>::<name>`) on either the
///    final path segment or by ending with `::<name>`.
fn find_matching_function<'a>(
    mir_name: &str,
    coverage: &'a CoverageReport,
) -> Option<&'a crate::profile::FunctionCoverage> {
    coverage
        .functions
        .iter()
        .find(|c| c.name == mir_name)
        .or_else(|| {
            coverage.functions.iter().find(|c| {
                let demangled = rustc_demangle::demangle(&c.name).to_string();
                // Strip a hash suffix (e.g. "::h0123...") if present in legacy
                // mangling output, then compare the final path segment.
                let trimmed = demangled
                    .rsplit_once("::h")
                    .map(|(prefix, _)| prefix)
                    .unwrap_or(demangled.as_str());
                trimmed == mir_name
                    || trimmed.ends_with(&format!("::{mir_name}"))
                    || trimmed.rsplit("::").next() == Some(mir_name)
            })
        })
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

    #[test]
    fn correlate_produces_one_function_entry() {
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let cov = profile::parse(COVERAGE_JSON).expect("coverage parses");
        let map = correlate(&mir, &cov);
        assert_eq!(map.functions.len(), 1);
        assert_eq!(map.functions[0].name, "decide");
    }

    #[test]
    fn correlate_joins_a_and_b_counts() {
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let cov = profile::parse(COVERAGE_JSON).expect("coverage parses");
        let map = correlate(&mir, &cov);
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
    fn correlate_matches_demangled_function_name() {
        // The fixture's coverage entry uses the Rust v0-mangled symbol.
        // correlate must demangle and match the tail "decide".
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let cov = profile::parse(COVERAGE_JSON).expect("coverage parses");
        // sanity check the mangled prefix is in the JSON
        assert!(cov.functions[0].name.starts_with("_R"));
        let map = correlate(&mir, &cov);
        assert!(!map.functions[0].conditions.is_empty());
    }

    #[test]
    fn correlate_drops_unmatched_function() {
        // Coverage references a function that isn't in the MIR.
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let coverage_with_other = COVERAGE_JSON.replace("6decide", "8different");
        let cov = profile::parse(&coverage_with_other).expect("coverage parses");
        let map = correlate(&mir, &cov);
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

    #[test]
    fn multi_evaluation_multi_condition_is_ambiguous() {
        // (T,T) then (F,-): the old collapse recorded (T,T) and lost
        // the (F,-) evaluation.
        let obs = observations_from_counts(
            Some("loop_tt_then_f".into()),
            &and_node(),
            &counts(&[("a", 1, 1), ("b", 1, 0)]),
            Some(1),
        );
        assert_eq!(obs.len(), 1);
        let o = &obs[0];
        assert!(o.ambiguous);
        assert!(o.inputs.is_empty(), "no vector may be invented");
        assert_eq!(o.result, None);
        assert_eq!(o.evaluations, Some(2));
        assert_eq!(o.counts, counts(&[("a", 1, 1), ("b", 1, 0)]));
    }

    #[test]
    fn base64_is_valid_b64_symbol_counts_are_ambiguous() {
        // rust-base64 `byte >= 32 && byte <= 126`, test
        // symbol_matches_char_for_all_valid_symbols (256 calls):
        // byte >= 32 = [224 T, 32 F], byte <= 126 = [95 T, 129 F].
        let decision = ite("byte >= 32", cond("byte <= 126"), konst(false));
        let raw = counts(&[("byte >= 32", 224, 32), ("byte <= 126", 95, 129)]);
        let obs = observations_from_counts(
            Some("symbol_matches_char_for_all_valid_symbols".into()),
            &decision,
            &raw,
            Some(256),
        );
        assert_eq!(obs.len(), 1);
        let o = &obs[0];
        assert!(o.ambiguous);
        assert!(o.inputs.is_empty());
        assert_eq!(o.result, None);
        assert_eq!(o.evaluations, Some(256));
        assert_eq!(o.counts, raw);
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
        let obs = observations_from_counts(
            None,
            &and_node(),
            &counts(&[("a", 2, 0), ("b", 1, 1)]),
            Some(1),
        );
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
        assert_eq!(obs[0].evaluations, Some(2));
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
    fn single_file_path_marks_merged_two_evaluation_coverage_ambiguous() {
        // COVERAGE_JSON is `ff` and `tt` merged into one profile:
        // two evaluations, a = [1, 1], b = [1, 0]. The old collapse
        // reported it as the single vector (T,T).
        let mir = mir::parse_text(SPANNED_AND_MIR).expect("MIR parses");
        let cov = profile::parse(COVERAGE_JSON).expect("coverage parses");
        let tree = decision::decompose(&mir);
        let obs = observations_from_coverage(Some("merged".into()), &mir, &cov, &tree);
        assert_eq!(obs.len(), 1);
        assert!(obs[0].ambiguous);
        assert_eq!(obs[0].evaluations, Some(2));
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
        let prepared = prepare_mir(&mir);
        let pcov = prepare_coverage_lookup(&cov);
        let obs =
            observations_from_prepared_coverage(Some("tt".into()), &prepared, 0, &pcov, &tree);
        assert_eq!(obs.len(), 1);
        assert!(!obs[0].ambiguous);
        assert_eq!(obs[0].inputs, inputs(&[("a", true), ("b", true)]));
        assert_eq!(obs[0].result, Some(true));
    }
}
