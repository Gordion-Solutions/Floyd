//! End-to-end pipeline test on corpus pattern `001-simple-and`.
//!
//! Compiles a fixture identical to corpus 001 plus a `main` that
//! exercises each input combination in a separate process, then runs
//! the full Floyd analytical pipeline. All external tool plumbing
//! (nightly rustc invocation, llvm-tools-preview lookup, per-run
//! profraw collection, llvm-profdata + llvm-cov subprocesses, JSON
//! parsing) goes through the [`floyd::instrument`] and
//! [`floyd::runner`] modules.
//!
//! Tagged `#[ignore]` because the toolchain prerequisites
//! (`nightly` + `llvm-tools-preview`) aren't universally available.
//! Run locally with:
//! ```text
//! cargo test --test end_to_end_001 -- --ignored --nocapture
//! ```

use floyd::masking::{ConditionObservation, ConditionStatus, Variant};
use floyd::{correlate, decision, instrument, masking, mir, profile, runner, Mir};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Fixture source: corpus pattern 001 plus a `main` that takes two
/// "0"/"1" arguments and exercises `decide`.
const FIXTURE_SOURCE: &str = r#"
pub fn decide(a: bool, b: bool) -> bool {
    a && b
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let a = args.get(1).map(|s| s == "1").unwrap_or(false);
    let b = args.get(2).map(|s| s == "1").unwrap_or(false);
    std::process::exit(if decide(a, b) { 0 } else { 1 });
}
"#;

/// Sanity-check that a single-test [`profile::CoverageReport`] reports
/// the runtime counts we'd predict from the source. Validates the
/// MIR ↔ coverage span join through [`correlate`]; not used to build
/// the observation itself.
///
/// In an integration test we *know* the input boolean values because
/// we set them on the command line — that is the correct level for
/// MC/DC analysis (the masking pair table is defined over input value
/// assignments, not over runtime-observed evaluations).
fn assert_runtime_consistent(
    test_label: &str,
    mir: &Mir,
    coverage: &profile::CoverageReport,
    expected_evaluated: &BTreeMap<String, bool>,
) {
    let map = correlate::correlate(mir, coverage);
    for f in &map.functions {
        for (name, runtime) in &f.conditions {
            match expected_evaluated.get(name) {
                Some(&true) => assert_eq!(
                    (runtime.true_count, runtime.false_count),
                    (1, 0),
                    "{test_label}: {name} expected eval=true, got {runtime:?}"
                ),
                Some(&false) => assert_eq!(
                    (runtime.true_count, runtime.false_count),
                    (0, 1),
                    "{test_label}: {name} expected eval=false, got {runtime:?}"
                ),
                None => assert_eq!(
                    (runtime.true_count, runtime.false_count),
                    (0, 0),
                    "{test_label}: {name} expected unevaluated, got {runtime:?}"
                ),
            }
        }
    }
}

/// Build a fresh workdir + write the fixture source + compile it via
/// [`floyd::instrument`].
///
/// `tag` makes the workdir name unique per test so cargo's parallel
/// test execution doesn't collide.
fn setup(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let workdir = std::env::temp_dir().join(format!("floyd-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workdir);
    std::fs::create_dir_all(&workdir).expect("create workdir");
    let src = workdir.join("fixture.rs");
    std::fs::write(&src, FIXTURE_SOURCE).expect("write source");
    let result = instrument::compile_with_coverage(&src, &workdir).expect("instrument");
    (workdir, result.mir_path, result.binary_path)
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn pipeline_on_001_simple_and() {
    let (workdir, mir_path, bin) = setup("full");
    let profdata_tool = runner::llvm_tool("llvm-profdata").expect("llvm-profdata");
    let cov_tool = runner::llvm_tool("llvm-cov").expect("llvm-cov");
    let mir =
        mir::parse_text(&std::fs::read_to_string(&mir_path).expect("read mir")).expect("parse mir");

    struct Case {
        a_str: &'static str,
        b_str: &'static str,
        a_val: bool,
        b_val: bool,
        result: bool,
        expected_evaluated: &'static [(&'static str, bool)],
    }
    let cases = [
        Case {
            a_str: "0",
            b_str: "0",
            a_val: false,
            b_val: false,
            result: false,
            expected_evaluated: &[("a", false)], // b short-circuited
        },
        Case {
            a_str: "0",
            b_str: "1",
            a_val: false,
            b_val: true,
            result: false,
            expected_evaluated: &[("a", false)], // b short-circuited
        },
        Case {
            a_str: "1",
            b_str: "0",
            a_val: true,
            b_val: false,
            result: false,
            expected_evaluated: &[("a", true), ("b", false)],
        },
        Case {
            a_str: "1",
            b_str: "1",
            a_val: true,
            b_val: true,
            result: true,
            expected_evaluated: &[("a", true), ("b", true)],
        },
    ];

    let mut observations = Vec::new();
    for Case {
        a_str,
        b_str,
        a_val,
        b_val,
        result,
        expected_evaluated,
    } in cases
    {
        let prof = workdir.join(format!("cov-{a_str}-{b_str}.profraw"));
        runner::run(&bin, &[a_str, b_str], &prof).expect("run binary");
        let coverage =
            runner::profraw_to_coverage(&prof, &profdata_tool, &cov_tool, &bin, &workdir)
                .expect("profraw -> coverage");

        // Cross-check the runtime ingestion pipeline against the
        // source-level short-circuit prediction. Independently
        // validates the MIR ↔ coverage join.
        let expected_map: BTreeMap<String, bool> = expected_evaluated
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect();
        assert_runtime_consistent(
            &format!("a={a_str},b={b_str}"),
            &mir,
            &coverage,
            &expected_map,
        );

        let mut inputs = BTreeMap::new();
        inputs.insert("a".to_string(), a_val);
        inputs.insert("b".to_string(), b_val);
        observations.push(ConditionObservation {
            test_name: Some(format!("a={a_str},b={b_str}")),
            inputs,
            result,
        });
    }

    let tree = decision::decompose(&mir);
    assert!(!tree.decisions.is_empty(), "expected at least one decision");

    let analysis = masking::analyze_with_runtime(&tree, &observations, Variant::Masking);
    assert!(
        matches!(
            analysis.condition_status["a"],
            ConditionStatus::Exercised(_)
        ),
        "a status was {:?}",
        analysis.condition_status["a"]
    );
    assert!(
        matches!(
            analysis.condition_status["b"],
            ConditionStatus::Exercised(_)
        ),
        "b status was {:?}",
        analysis.condition_status["b"]
    );
    assert_eq!(analysis.matrix.conditions, vec!["a", "b"]);
    assert_eq!(analysis.matrix.truth_table.len(), 4);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn pipeline_detects_partial_coverage_on_001() {
    // Same fixture as above, but only run inputs (0,0) and (1,1).
    // Corpus 001's expected pairs are {a: (F,T) vs (T,T), b: (T,F) vs
    // (T,T)} — neither has both of its members observed, so both
    // conditions should come out Unexercised.
    let (workdir, mir_path, bin) = setup("partial");
    let profdata_tool = runner::llvm_tool("llvm-profdata").expect("llvm-profdata");
    let cov_tool = runner::llvm_tool("llvm-cov").expect("llvm-cov");
    let mir = mir::parse_text(&std::fs::read_to_string(&mir_path).unwrap()).unwrap();

    struct Case {
        a_str: &'static str,
        b_str: &'static str,
        a_val: bool,
        b_val: bool,
        result: bool,
    }
    let cases = [
        Case {
            a_str: "0",
            b_str: "0",
            a_val: false,
            b_val: false,
            result: false,
        },
        Case {
            a_str: "1",
            b_str: "1",
            a_val: true,
            b_val: true,
            result: true,
        },
    ];
    let mut observations = Vec::new();
    for Case {
        a_str,
        b_str,
        a_val,
        b_val,
        result,
    } in cases
    {
        let prof = workdir.join(format!("cov-{a_str}-{b_str}.profraw"));
        runner::run(&bin, &[a_str, b_str], &prof).unwrap();
        // Just exercise the coverage pipeline; we don't need the
        // result here.
        let _ =
            runner::profraw_to_coverage(&prof, &profdata_tool, &cov_tool, &bin, &workdir).unwrap();
        let mut inputs = BTreeMap::new();
        inputs.insert("a".to_string(), a_val);
        inputs.insert("b".to_string(), b_val);
        observations.push(ConditionObservation {
            test_name: Some(format!("a={a_str},b={b_str}")),
            inputs,
            result,
        });
    }

    let tree = decision::decompose(&mir);
    let analysis = masking::analyze_with_runtime(&tree, &observations, Variant::Masking);
    assert!(matches!(
        analysis.condition_status["a"],
        ConditionStatus::Unexercised
    ));
    assert!(matches!(
        analysis.condition_status["b"],
        ConditionStatus::Unexercised
    ));
}
