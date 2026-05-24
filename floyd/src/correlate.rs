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
use crate::profile::CoverageReport;
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
                if let Some(branch) = cov_fn.branches.iter().find(|b| b.span == *span) {
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

/// Collect a `name -> SourceSpan` map for every named condition that
/// appears in the function's MIR.
///
/// Both `switchInt` discriminants (condition tests) and `AssignCopy`
/// sources (condition reads) are considered. First occurrence wins.
fn collect_condition_spans(f: &MirFunction) -> BTreeMap<String, SourceSpan> {
    let mut spans = BTreeMap::new();
    for block in &f.blocks {
        if let MirTerminator::SwitchInt {
            discr,
            span: Some(span),
            ..
        } = &block.terminator
        {
            if let Some(name) = f.debug_names.get(discr) {
                spans.entry(name.clone()).or_insert_with(|| span.clone());
            }
        }
        for stmt in &block.statements {
            if let MirStatement::AssignCopy {
                src,
                span: Some(span),
                ..
            } = stmt
            {
                if let Some(name) = f.debug_names.get(src) {
                    spans.entry(name.clone()).or_insert_with(|| span.clone());
                }
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
