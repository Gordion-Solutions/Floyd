//! End-to-end regression test: `cargo floyd test` on the
//! `fixtures/mcdc-cases` crate.
//!
//! The fixture has one module per engine bug class (observation
//! collapse, name-based correlation, spurious candidates, assert
//! terminators, not-measurable decisions, the value-arm lift,
//! `harness = false` targets). Each function there documents the
//! outcome Floyd must report for it; [`EXPECTED`] restates those
//! outcomes and the tests below check them against the JSON report,
//! per function: candidate or `out_of_scope` (with its `reason`),
//! `invoked`, observation count, `not_measurable.reason`, `lift`,
//! the harness-free `note`; and, per `--only=<fn>` run, observation
//! and ambiguous counts, the analysed conditions,
//! `summary.percentage` and the exit code.
//!
//! Tagged `#[ignore]` because it needs the nightly toolchain with
//! `llvm-tools-preview` (cargo-floyd builds with `cargo +nightly`).
//! Run with:
//! ```text
//! cargo test -p cargo-floyd --test fixture_cases -- --ignored
//! ```

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// One `cargo floyd test` run.
struct Run {
    /// Parsed stdout (`--format=json`), or `Value::Null` for text.
    json: Value,
    stdout: String,
    stderr: String,
    code: i32,
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/mcdc-cases")
}

/// A target directory of its own, so the nested `cargo +nightly
/// build` never waits on the lock of the `cargo test` running us.
fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("mcdc-cases-target")
}

/// Run `cargo floyd test --format=<format> [extra..] fixtures/mcdc-cases`.
///
/// Every run checks that the `harness = false` target `no_harness`
/// was not executed: the fixture's `main` writes the marker file when
/// it runs, including under `--list`.
fn floyd(format: &str, extra: &[&str]) -> Run {
    let tag = extra.join("_").replace(['=', '-', ':'], "");
    let marker = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "no-harness-ran-{format}-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker);
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-floyd"))
        .args(["floyd", "test", &format!("--format={format}")])
        .args(extra)
        .arg(fixture_dir())
        .env("CARGO_TARGET_DIR", target_dir())
        .env("MCDC_CASES_NO_HARNESS_MARKER", &marker)
        .output()
        .expect("run cargo-floyd");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !marker.exists(),
        "the harness = false target `no_harness` was executed ({format} {extra:?})"
    );
    let json = if format == "json" {
        serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!("--format=json {extra:?} printed no JSON ({e}); stderr:\n{stderr}")
        })
    } else {
        Value::Null
    };
    Run {
        json,
        stdout,
        stderr,
        code: out.status.code().expect("exit code"),
    }
}

/// The whole-crate run, shared by the per-function checks.
fn full() -> &'static Run {
    static FULL: OnceLock<Run> = OnceLock::new();
    FULL.get_or_init(|| floyd("json", &[]))
}

/// What Floyd must report for one fixture function.
enum Expect {
    /// A candidate. `observations` is the number of tests that
    /// produced one; `not_measurable` its reason when present.
    Candidate {
        invoked: bool,
        observations: u64,
        lift: Option<&'static str>,
        not_measurable: Option<&'static str>,
        note: bool,
    },
    /// Not a candidate; listed in `out_of_scope` with exactly these
    /// reasons (a `const fn` has a runtime and a CTFE body).
    OutOfScope(&'static [&'static str]),
}

const fn measured(observations: u64, lift: Option<&'static str>) -> Expect {
    Expect::Candidate {
        invoked: true,
        observations,
        lift,
        not_measurable: None,
        note: false,
    }
}

/// Fixture functions and their expected outcome, keyed by the tail
/// of the MIR function name (`<impl at ..>::` prefixes vary).
const EXPECTED: &[(&str, Expect)] = &[
    // 1. Observation collapse / ambiguity.
    ("collapse_and", measured(2, None)),
    ("tree_and", measured(1, None)),
    ("and_or_tree", measured(1, None)),
    ("dag_or_and", measured(1, Some("value_arms"))),
    ("dag_one_two_sided", measured(1, Some("value_arms"))),
    ("single_and", measured(3, None)),
    // 2. Name-based correlation.
    ("impl_or", measured(3, None)),
    ("trait_and", measured(3, None)),
    ("generic_or", measured(3, None)),
    ("closure_and::{closure#0}", measured(3, None)),
    // 3. Spurious candidates.
    ("drop_flag_loop", Expect::OutOfScope(&["unsupported_shape"])),
    ("const_entry", Expect::OutOfScope(&["unsupported_shape"])),
    ("wide_leaf", Expect::OutOfScope(&["non_bool_leaf"])),
    ("setup_copy", Expect::OutOfScope(&["unsupported_shape"])),
    (
        "ctfe_flag",
        Expect::OutOfScope(&["ctfe_body", "unsupported_shape"]),
    ),
    // Non-bool arms whose decision is the comparison alone (see
    // `non_bool_arms_are_never_conditions`).
    ("struct_leaf", measured(1, None)),
    ("usize_leaf", measured(1, Some("value_arms"))),
    ("slice_leaf", measured(1, Some("value_arms"))),
    // 4. Assert terminators.
    ("assert_after_and", measured(3, None)),
    // 5. Not measurable.
    (
        "derived_and",
        Expect::Candidate {
            invoked: false,
            observations: 0,
            lift: None,
            not_measurable: Some("uninstrumented"),
            note: false,
        },
    ),
    ("eq", Expect::OutOfScope(&["unsupported_shape"])),
    ("fmt", Expect::OutOfScope(&["unsupported_shape"])),
    (
        "enum_match",
        Expect::Candidate {
            invoked: false,
            observations: 0,
            lift: None,
            not_measurable: Some("no_branch_region"),
            note: false,
        },
    ),
    // Unexecuted in every test Floyd ran, so its record has no branch
    // region; the note names the target that would run it.
    (
        "harness_free_and",
        Expect::Candidate {
            invoked: false,
            observations: 0,
            lift: None,
            not_measurable: Some("no_branch_region"),
            note: true,
        },
    ),
    // 6. Value-arm lift.
    ("lift_const_arms", measured(2, Some("value_arms"))),
    ("lift_option_arms", measured(3, Some("value_arms"))),
    ("lift_checked_arms", measured(2, Some("call_arms"))),
    ("lift_checked_and", measured(3, Some("value_arms"))),
    (
        "decline_nested_if",
        Expect::OutOfScope(&["unsupported_shape"]),
    ),
    (
        "decline_loop_arm",
        Expect::OutOfScope(&["unsupported_shape"]),
    ),
    (
        "decline_split_locals",
        Expect::OutOfScope(&["unsupported_shape"]),
    ),
];

/// Does MIR function name `name` denote fixture function `key`?
fn names(name: &str, key: &str) -> bool {
    name == key || name.ends_with(&format!("::{key}"))
}

fn expected_key(name: &str) -> Option<&'static str> {
    EXPECTED.iter().map(|(k, _)| *k).find(|k| names(name, k))
}

fn candidates(run: &Run) -> &[Value] {
    run.json["candidates"].as_array().expect("candidates array")
}

fn out_of_scope(run: &Run) -> &[Value] {
    run.json["out_of_scope"]
        .as_array()
        .expect("out_of_scope array")
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The `--only` run's analysed condition names.
fn conditions(run: &Run) -> Vec<&str> {
    run.json["analysis"]["matrix"]["conditions"]
        .as_array()
        .expect("analysed conditions")
        .iter()
        .map(|c| c.as_str().expect("condition name"))
        .collect()
}

fn observations(run: &Run) -> &[Value] {
    run.json["observations"]
        .as_array()
        .expect("observations array")
}

fn percentage(run: &Run) -> Option<f64> {
    run.json["summary"]["percentage"].as_f64()
}

/// The JSON fields every run carries, decline paths included.
fn assert_report_shape(run: &Run, what: &str) {
    for key in [
        "verdict",
        "summary",
        "candidates",
        "out_of_scope",
        "skipped_no_harness",
    ] {
        assert!(
            run.json.get(key).is_some(),
            "{what}: JSON has no `{key}`; stdout:\n{}",
            run.stdout
        );
    }
    for key in [
        "conditions_total",
        "exercised",
        "not_exercised",
        "unknown",
        "not_measurable",
        "percentage",
    ] {
        assert!(
            run.json["summary"].get(key).is_some(),
            "{what}: summary has no `{key}`"
        );
    }
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn every_fixture_function_has_its_expected_outcome() {
    let run = full();
    assert_report_shape(run, "full run");
    assert_eq!(str_field(&run.json, "verdict"), Some("analysed"));

    // No candidate outside the table: test bodies, assertion guards,
    // derives and spurious trees must never be candidates.
    for c in candidates(run) {
        let name = str_field(c, "function_name").unwrap_or_default();
        let key =
            expected_key(name).unwrap_or_else(|| panic!("unexpected candidate `{name}`: {c}"));
        assert!(
            matches!(
                EXPECTED.iter().find(|(k, _)| *k == key).map(|(_, e)| e),
                Some(Expect::Candidate { .. })
            ),
            "`{name}` must not be a candidate: {c}"
        );
    }
    // Every declined fixture function (outside the unit tests) is in
    // the table.
    for o in out_of_scope(run) {
        let name = str_field(o, "name").unwrap_or_default();
        if name.contains("::tests::") {
            continue;
        }
        assert!(
            expected_key(name).is_some(),
            "unexpected out_of_scope entry `{name}`: {o}"
        );
    }

    for (key, expect) in EXPECTED {
        let found: Vec<&Value> = candidates(run)
            .iter()
            .filter(|c| names(str_field(c, "function_name").unwrap_or_default(), key))
            .collect();
        let declined: BTreeSet<&str> = out_of_scope(run)
            .iter()
            .filter(|o| names(str_field(o, "name").unwrap_or_default(), key))
            .filter_map(|o| str_field(o, "reason"))
            .collect();
        match expect {
            Expect::Candidate {
                invoked,
                observations,
                lift,
                not_measurable,
                note,
            } => {
                assert_eq!(found.len(), 1, "`{key}`: expected one candidate");
                assert!(
                    declined.is_empty(),
                    "`{key}`: candidate also in out_of_scope as {declined:?}"
                );
                let c = found[0];
                assert_eq!(
                    c["invoked"].as_bool(),
                    Some(*invoked),
                    "`{key}` invoked: {c}"
                );
                assert_eq!(
                    c["observations"].as_u64(),
                    Some(*observations),
                    "`{key}` observations: {c}"
                );
                assert_eq!(str_field(c, "lift"), *lift, "`{key}` lift: {c}");
                assert_eq!(
                    c.get("not_measurable")
                        .and_then(|nm| str_field(nm, "reason")),
                    *not_measurable,
                    "`{key}` not_measurable: {c}"
                );
                match str_field(c, "note") {
                    Some(text) => {
                        assert!(*note, "`{key}`: unexpected note: {text}");
                        assert!(
                            text.contains("`no_harness`") && text.contains("`mcdc-cases`"),
                            "`{key}` note: {text}"
                        );
                    }
                    None => assert!(!*note, "`{key}`: no harness-free note: {c}"),
                }
            }
            Expect::OutOfScope(reasons) => {
                assert!(
                    found.is_empty(),
                    "`{key}` must not be a candidate: {found:?}"
                );
                let want: BTreeSet<&str> = reasons.iter().copied().collect();
                assert_eq!(declined, want, "`{key}` out_of_scope reasons");
            }
        }
    }
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn harness_false_target_is_skipped_and_never_run() {
    let run = full();
    let skipped = run.json["skipped_no_harness"]
        .as_array()
        .expect("skipped_no_harness array");
    let targets: Vec<&str> = skipped
        .iter()
        .filter_map(|t| str_field(t, "target"))
        .collect();
    assert_eq!(targets, vec!["no_harness"]);
    // Not listed: none of its code counts as a discovered test.
    let tests = run.json["tests_discovered"]
        .as_array()
        .expect("tests_discovered");
    assert!(tests
        .iter()
        .all(|t| !t.as_str().unwrap_or_default().contains("no_harness")));
    // `floyd` asserted the marker file was not written.
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn multi_evaluation_tree_test_with_one_two_sided_condition_is_resolved() {
    // `collapse_and_loop`'s counters determine its two vectors; the
    // decision is a tree, so the tree rule (tried first) gives each
    // its own count.
    let run = floyd("json", &["--only=collapse_and"]);
    assert_report_shape(&run, "collapse_and");
    assert_eq!(conditions(&run), vec!["a", "b"]);
    let obs = observations(&run);
    assert_eq!(obs.len(), 3, "{obs:?}");
    assert!(
        obs.iter().all(|o| o["ambiguous"].as_bool() == Some(false)),
        "{obs:?}"
    );
    let looped: Vec<&Value> = obs
        .iter()
        .filter(|o| str_field(o, "test_name") == Some("observation::tests::collapse_and_loop"))
        .collect();
    assert_eq!(looped.len(), 2, "{obs:?}");
    assert_eq!(
        looped[0]["inputs"],
        serde_json::json!({"a": true, "b": true})
    );
    assert_eq!(looped[0]["result"].as_bool(), Some(true));
    assert_eq!(looped[1]["inputs"], serde_json::json!({"a": false}));
    assert_eq!(looped[1]["result"].as_bool(), Some(false));
    assert!(looped.iter().all(|o| o["evaluations"].as_u64() == Some(1)));
    assert!(looped
        .iter()
        .all(|o| str_field(o, "resolved_by") == Some("tree_counters")));
    assert_eq!(
        percentage(&run),
        Some(100.0),
        "summary: {}",
        run.json["summary"]
    );
    assert_eq!(run.code, 0);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn multi_evaluation_dag_test_with_one_two_sided_condition_is_resolved() {
    // `(a || b) && c` is not a tree, so the tree rule declines; only
    // `a` took both values, so the single-two-sided rule determines
    // both vectors, each recorded with the test's 3 evaluations.
    let run = floyd("json", &["--only=dag_one_two_sided"]);
    assert_report_shape(&run, "dag_one_two_sided");
    assert_eq!(conditions(&run), vec!["a", "b", "c"]);
    let obs = observations(&run);
    assert_eq!(obs.len(), 2, "{obs:?}");
    assert!(
        obs.iter().all(|o| o["ambiguous"].as_bool() == Some(false)
            && o["evaluations"].as_u64() == Some(3)
            && str_field(o, "resolved_by") == Some("single_two_sided")
            && str_field(o, "test_name") == Some("observation::tests::dag_one_two_sided_loop")),
        "{obs:?}"
    );
    assert_eq!(obs[0]["inputs"], serde_json::json!({"a": true, "c": true}));
    assert_eq!(
        obs[1]["inputs"],
        serde_json::json!({"a": false, "b": true, "c": true})
    );
    assert!(
        obs.iter().all(|o| o["result"].as_bool() == Some(true)),
        "{obs:?}"
    );
    assert_eq!(
        run.json["analysis"]["ambiguous_observations"].as_u64(),
        Some(0)
    );
}

/// The observations of `test` (a test of the `observation` module),
/// as (inputs, result), checking each is a `tree_counters` vector of
/// one evaluation.
fn tree_vectors(obs: &[Value], test: &str) -> Vec<(Value, Option<bool>)> {
    obs.iter()
        .filter(|o| str_field(o, "test_name") == Some(&format!("observation::tests::{test}")))
        .map(|o| {
            assert_eq!(o["ambiguous"].as_bool(), Some(false), "{o}");
            assert_eq!(o["evaluations"].as_u64(), Some(1), "{o}");
            assert_eq!(str_field(o, "resolved_by"), Some("tree_counters"), "{o}");
            (o["inputs"].clone(), o["result"].as_bool())
        })
        .collect()
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn multi_evaluation_tree_test_is_resolved_by_tree_counters() {
    // `tree_and_loop`: both conditions two-sided; `a && b` is a tree.
    let run = floyd("json", &["--only=tree_and"]);
    assert_report_shape(&run, "tree_and");
    assert_eq!(conditions(&run), vec!["a", "b"]);
    let obs = observations(&run);
    assert_eq!(obs.len(), 3, "{obs:?}");
    assert_eq!(
        tree_vectors(obs, "tree_and_loop"),
        [
            (serde_json::json!({"a": true, "b": true}), Some(true)),
            (serde_json::json!({"a": true, "b": false}), Some(false)),
            (serde_json::json!({"a": false}), Some(false)),
        ]
    );
    assert_eq!(
        run.json["analysis"]["ambiguous_observations"].as_u64(),
        Some(0)
    );
    assert_eq!(
        percentage(&run),
        Some(100.0),
        "summary: {}",
        run.json["summary"]
    );
    assert_eq!(run.code, 0);

    // `and_or_tree_loop`: `a && (b || c)`, every condition two-sided.
    let run = floyd("json", &["--only=and_or_tree"]);
    assert_report_shape(&run, "and_or_tree");
    assert_eq!(conditions(&run), vec!["a", "b", "c"]);
    let obs = observations(&run);
    assert_eq!(obs.len(), 4, "{obs:?}");
    assert_eq!(
        tree_vectors(obs, "and_or_tree_loop"),
        [
            (serde_json::json!({"a": true, "b": true}), Some(true)),
            (
                serde_json::json!({"a": true, "b": false, "c": true}),
                Some(true)
            ),
            (
                serde_json::json!({"a": true, "b": false, "c": false}),
                Some(false)
            ),
            (serde_json::json!({"a": false}), Some(false)),
        ]
    );
    assert_eq!(
        percentage(&run),
        Some(100.0),
        "summary: {}",
        run.json["summary"]
    );
    assert_eq!(run.code, 0);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn multi_evaluation_dag_test_is_ambiguous_and_withholds_the_percentage() {
    let run = floyd("json", &["--only=dag_or_and"]);
    assert_report_shape(&run, "dag_or_and");
    assert_eq!(str_field(&run.json, "verdict"), Some("analysed"));
    assert_eq!(conditions(&run), vec!["a", "b", "c"]);
    let obs = observations(&run);
    assert_eq!(obs.len(), 1, "{obs:?}");
    let looped = &obs[0];
    assert_eq!(looped["ambiguous"].as_bool(), Some(true));
    assert_eq!(
        str_field(looped, "test_name"),
        Some("observation::tests::dag_or_and_loop")
    );
    assert_eq!(looped["evaluations"].as_u64(), Some(2));
    assert_eq!(
        looped["inputs"],
        serde_json::json!({}),
        "no fabricated vector"
    );
    assert!(looped["result"].is_null());
    assert!(looped.get("resolved_by").is_none(), "{looped}");
    for (c, t, f) in [("a", 1, 1), ("b", 1, 0), ("c", 1, 1)] {
        assert_eq!(looped["counts"][c]["true_count"].as_u64(), Some(t), "{c}");
        assert_eq!(looped["counts"][c]["false_count"].as_u64(), Some(f), "{c}");
    }
    assert_eq!(
        run.json["analysis"]["ambiguous_observations"].as_u64(),
        Some(1)
    );
    assert_eq!(run.json["summary"]["unknown"].as_u64(), Some(1));
    assert_eq!(percentage(&run), None, "summary: {}", run.json["summary"]);
    assert_eq!(run.code, 2);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn text_report_prints_no_percentage_when_a_condition_is_unknown() {
    let run = floyd("text", &["--only=dag_or_and"]);
    assert!(run.stdout.contains("AMBIGUOUS"), "{}", run.stdout);
    assert!(run.stdout.contains("UNKNOWN"), "{}", run.stdout);
    assert!(
        run.stdout
            .contains("1 unknown of 3 conditions (no percentage"),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains('%'), "{}", run.stdout);
    assert_eq!(run.code, 2);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn outcome_no_test_took_is_unexercised_not_unknown() {
    let run = floyd("json", &["--only=dag_or_and"]);
    let status = &run.json["analysis"]["condition_status"];
    assert_eq!(status["a"], "Unexercised", "{status}");
    assert_eq!(status["b"], "Unexercised", "{status}");
    assert!(status["c"]["Ambiguous"].is_object(), "{status}");
    let why = &run.json["analysis"]["ruled_out"]["b"][0];
    assert_eq!(
        str_field(why, "test"),
        Some("observation::tests::dag_or_and_loop"),
        "{why}"
    );
    assert_eq!(str_field(why, "condition"), Some("b"));
    assert_eq!(why["value"].as_bool(), Some(false));
    assert_eq!(why["counts"]["false_count"].as_u64(), Some(0));
    assert_eq!(run.json["summary"]["not_exercised"].as_u64(), Some(2));
    assert_eq!(run.code, 2);

    let text = floyd("text", &["--only=dag_or_and"]);
    assert!(
        text.stdout.contains("b: UNEXERCISED")
            && text
                .stdout
                .contains("dag_or_and_loop (2 evaluations) never took b=F (1T/0F)"),
        "{}",
        text.stdout
    );
}

/// `--only=<name>`: the analysed conditions, the number of tests
/// observed, and the percentage and exit code of a run that
/// demonstrates MC/DC.
fn assert_full_mcdc(name: &str, want_conditions: &[&str], want_observations: usize) {
    let run = floyd("json", &[&format!("--only={name}")]);
    assert_report_shape(&run, name);
    assert_eq!(str_field(&run.json, "verdict"), Some("analysed"), "{name}");
    assert_eq!(conditions(&run), want_conditions, "{name}");
    let obs = observations(&run);
    assert_eq!(obs.len(), want_observations, "{name}: {obs:?}");
    assert!(
        obs.iter().all(|o| o["ambiguous"].as_bool() == Some(false)),
        "{name}: {obs:?}"
    );
    assert_eq!(
        percentage(&run),
        Some(100.0),
        "{name}: {}",
        run.json["summary"]
    );
    assert_eq!(run.code, 0, "{name}: stderr:\n{}", run.stderr);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn single_evaluation_tests_demonstrate_mcdc() {
    assert_full_mcdc("single_and", &["a", "b"], 3);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn generic_instantiations_are_summed() {
    // `p` is exercised by the u16 test against the u8 tests: both
    // instantiations must join to the one candidate.
    assert_full_mcdc("generic_or", &["p", "q"], 3);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn method_trait_and_closure_decisions_demonstrate_mcdc() {
    assert_full_mcdc("impl_or", &["armed", "forced"], 3);
    assert_full_mcdc("trait_and", &["left", "right"], 3);
    assert_full_mcdc("closure_and", &["enable", "v"], 3);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn value_arm_lifts_demonstrate_mcdc() {
    assert_full_mcdc("lift_option_arms", &["a", "b"], 3);
    assert_full_mcdc("lift_checked_and", &["a", "b"], 3);
    assert_full_mcdc("lift_const_arms", &["upper"], 2);
    assert_full_mcdc("assert_after_and", &["a", "b"], 3);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn non_bool_arms_are_never_conditions() {
    for (name, want) in [
        ("struct_leaf", "sh == 0"),
        ("usize_leaf", "n == 0"),
        ("slice_leaf", "first"),
    ] {
        let run = floyd("json", &[&format!("--only={name}")]);
        assert_eq!(str_field(&run.json, "verdict"), Some("analysed"), "{name}");
        assert_eq!(conditions(&run), vec![want], "{name}");
        // One test takes one arm: the condition is not exercised.
        assert_eq!(percentage(&run), Some(0.0), "{name}");
        assert_eq!(run.code, 2, "{name}");
    }
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn decline_paths_still_emit_json() {
    // Only a candidate no test Floyd ran entered.
    let run = floyd("json", &["--only=harness_free_and"]);
    assert_report_shape(&run, "all_unobserved");
    assert_eq!(str_field(&run.json, "verdict"), Some("all_unobserved"));
    assert!(run.json["analysis"].is_null());
    assert_eq!(run.json["summary"]["conditions_total"].as_u64(), Some(0));
    assert_eq!(percentage(&run), None);
    assert!(run.stderr.contains("`no_harness`"), "{}", run.stderr);
    assert_eq!(run.code, 1);

    // No candidate at all.
    let run = floyd("json", &["--only=no_such_fixture_function"]);
    assert_report_shape(&run, "no_candidates");
    assert_eq!(str_field(&run.json, "verdict"), Some("no_candidates"));
    assert!(candidates(&run).is_empty());
    assert!(
        !out_of_scope(&run).is_empty(),
        "out_of_scope is still reported"
    );
    assert_eq!(percentage(&run), None);
    assert_eq!(run.code, 1);
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn whole_crate_exit_code_follows_the_summary() {
    let run = full();
    let summary = &run.json["summary"];
    let demonstrated = summary["not_exercised"].as_u64() == Some(0)
        && summary["unknown"].as_u64() == Some(0)
        && summary["conditions_total"].as_u64() > Some(0);
    assert_eq!(run.code, if demonstrated { 0 } else { 2 }, "{summary}");
}
