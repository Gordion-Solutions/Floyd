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

use crate::decision::{self, DecisionTree};
use crate::masking::ConditionObservation;
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

/// Build a [`ConditionObservation`] from a single-test
/// [`CoverageReport`], a [`Mir`] tree, and a [`DecisionTree`].
///
/// For each condition the [`correlate`] join finds runtime counts
/// for, infer the boolean value the test evaluated it to: `true` if
/// `true_count > 0`, `false` if `false_count > 0`, *omitted* if both
/// are zero (the condition was short-circuited and not evaluated in
/// this test).
///
/// The decision's result is computed by evaluating the first decision
/// in `tree` under the inferred inputs via
/// [`decision::evaluate_partial`]. Returns `None` if the tree is
/// empty or the ITE evaluation needs a condition that wasn't observed
/// (which would indicate inconsistent input data).
pub fn observation_from_coverage(
    test_name: Option<String>,
    mir: &Mir,
    coverage: &CoverageReport,
    tree: &DecisionTree,
) -> Option<ConditionObservation> {
    let map = correlate(mir, coverage);
    let mut inputs = BTreeMap::new();
    for f in &map.functions {
        for (name, runtime) in &f.conditions {
            if runtime.true_count > 0 {
                inputs.insert(name.clone(), true);
            } else if runtime.false_count > 0 {
                inputs.insert(name.clone(), false);
            }
            // Both 0 -> short-circuited; omit from inputs.
        }
    }
    let decision_node = tree.decisions.first()?;
    let result = decision::evaluate_partial(decision_node, &inputs)?;
    Some(ConditionObservation {
        test_name,
        inputs,
        result,
    })
}

/// A MIR with per-function condition-span maps pre-computed.
///
/// `observation_from_coverage` and the lower-level [`correlate`] both
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

/// Build an [`ConditionObservation`] for one candidate and one
/// test's coverage, using pre-computed span data and the candidate's
/// known home-function index.
///
/// The old `observation_from_coverage` iterated EVERY function in
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
pub fn observation_from_prepared_coverage(
    test_name: Option<String>,
    prepared: &PreparedMir<'_>,
    fn_idx: usize,
    prepared_cov: &PreparedCoverage<'_>,
    tree: &DecisionTree,
) -> Option<ConditionObservation> {
    let f = prepared.mir.functions.get(fn_idx)?;
    let spans = prepared.function_spans.get(fn_idx)?;
    let cov_fn = prepared_cov.find(&f.name)?;
    let mut inputs = BTreeMap::new();
    for (name, span) in spans {
        if let Some(branch) = cov_fn.branches.iter().find(|b| spans_match(&b.span, span)) {
            if branch.true_count > 0 {
                inputs.insert(name.clone(), true);
            } else if branch.false_count > 0 {
                inputs.insert(name.clone(), false);
            }
        }
    }
    let decision_node = tree.decisions.first()?;
    let result = decision::evaluate_partial(decision_node, &inputs)?;
    Some(ConditionObservation {
        test_name,
        inputs,
        result,
    })
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
    /// O(1) analogue of [`find_matching_function`].
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

/// Deprecated: use [`observation_from_prepared_coverage`] with
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
}
