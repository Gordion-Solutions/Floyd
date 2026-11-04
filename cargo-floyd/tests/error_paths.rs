//! End-to-end checks of `cargo floyd test`'s error paths and
//! options, on the standalone fixtures `fixtures/failure-paths` and
//! `fixtures/proc-macro-test`:
//!
//! - a build failure prints rustc's diagnostic to stderr and, under
//!   `--format=json` / `--format=junit`, a `build_failed` document;
//! - `--features` reaches the instrumented build;
//! - a failing `--list` gives `test_discovery_failed`, a test run
//!   that leaves no coverage profile gives `test_run_failed`;
//! - a proc-macro crate's test binary (which links `libstd`
//!   dynamically) is listed and run.
//!
//! Tagged `#[ignore]`: needs the nightly toolchain with
//! `llvm-tools-preview`. Run with
//! `cargo test -p cargo-floyd --test error_paths -- --ignored`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name)
}

/// `cargo floyd test <args..> fixtures/<name>` with `env` set. Each
/// fixture gets its own target directory.
fn floyd(name: &str, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cargo-floyd"));
    cmd.args(["floyd", "test"])
        .args(args)
        .arg(fixture(name))
        .env(
            "CARGO_TARGET_DIR",
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-target")),
        )
        .env_remove("FLOYD_FIXTURE_FAIL_LIST")
        .env_remove("FLOYD_FIXTURE_NO_PROFILE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run cargo-floyd");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().expect("exit code"),
    }
}

const COMPILE_ERROR: &str = "failure-paths fixture: build with --features builds";

/// The fields every error-path JSON document carries.
fn assert_error_report(j: &Value, verdict: &str, stage: &str) {
    assert_eq!(j["verdict"], verdict, "{j:#}");
    assert!(j["summary"]["percentage"].is_null());
    assert_eq!(j["error"]["stage"], stage);
    assert!(!j["error"]["message"].as_str().unwrap().is_empty());
    assert!(j["analysis"].is_null());
    for key in [
        "tests_discovered",
        "candidates",
        "out_of_scope",
        "skipped_oversize_mirs",
        "skipped_no_harness",
    ] {
        assert!(j[key].is_array(), "{key} missing: {j:#}");
    }
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn build_failure_reports_rustc_diagnostics_in_every_format() {
    let json = floyd("failure-paths", &["--format=json"], &[]);
    assert_eq!(json.code, 1);
    let j = json.json();
    assert_error_report(&j, "build_failed", "cargo build --tests");
    assert!(j["workspace_root"].is_null());
    assert!(j["error"]["ice"].is_null());
    let diags = j["error"]["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert!(diags[0].as_str().unwrap().contains(COMPILE_ERROR));
    assert!(diags[0].as_str().unwrap().contains("src/lib.rs"));
    // The rendered diagnostic and the summary reach stderr.
    assert!(json.stderr.contains(COMPILE_ERROR), "{}", json.stderr);
    assert!(json
        .stderr
        .contains("cargo-floyd: instrumented build failed: 1 compiler error(s)"));

    let junit = floyd("failure-paths", &["--format=junit"], &[]);
    assert_eq!(junit.code, 1);
    assert!(junit.stdout.starts_with("<?xml"));
    assert!(junit.stdout.contains(r#"type="build_failed""#));
    assert!(junit.stdout.contains("<error "));
    assert!(junit.stdout.contains(COMPILE_ERROR));
    assert!(junit.stdout.trim_end().ends_with("</testsuites>"));

    let text = floyd("failure-paths", &[], &[]);
    assert_eq!(text.code, 1);
    assert!(text.stderr.contains(COMPILE_ERROR));
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn features_reach_the_instrumented_build() {
    for args in [
        &["--format=json", "--features", "builds"][..],
        &["--format=json", "--features=builds"][..],
        &["--format=json", "-F", "builds"][..],
        &["--format=json", "--all-features"][..],
    ] {
        let run = floyd("failure-paths", args, &[]);
        let j = run.json();
        assert_eq!(j["verdict"], "analysed", "{args:?}: {}", run.stderr);
        assert!(j["error"].is_null());
        assert_eq!(j["summary"]["percentage"], 100.0);
        assert_eq!(run.code, 0);
    }
    // --no-default-features is passed too: `builds` is not a default
    // feature, so the build still fails.
    let run = floyd(
        "failure-paths",
        &["--format=json", "--no-default-features"],
        &[],
    );
    assert_eq!(run.json()["verdict"], "build_failed");
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn failing_test_discovery_reports_test_discovery_failed() {
    let env = [("FLOYD_FIXTURE_FAIL_LIST", "1")];
    let run = floyd("failure-paths", &["--format=json", "-F", "builds"], &env);
    assert_eq!(run.code, 1);
    let j = run.json();
    assert_error_report(&j, "test_discovery_failed", "list_tests");
    assert!(j["workspace_root"].is_string());
    assert_eq!(j["test_targets"], 1);
    assert!(j["error"]["stderr"]
        .as_str()
        .unwrap()
        .contains("refusing to start"));
    assert!(run.stderr.contains("refusing to start"));

    let junit = floyd("failure-paths", &["--format=junit", "-F", "builds"], &env);
    assert_eq!(junit.code, 1);
    assert!(junit.stdout.contains(r#"type="test_discovery_failed""#));
    assert!(junit.stdout.contains("refusing to start"));
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn failing_test_run_reports_test_run_failed() {
    let env = [("FLOYD_FIXTURE_NO_PROFILE", "1")];
    let run = floyd("failure-paths", &["--format=json", "-F", "builds"], &env);
    assert_eq!(run.code, 1);
    let j = run.json();
    assert_error_report(&j, "test_run_failed", "run_test_isolated");
    let tests: Vec<&str> = j["tests_discovered"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        tests,
        ["tests::both", "tests::first_false", "tests::second_false"]
    );
    assert!(j["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no profraw produced"));

    let junit = floyd("failure-paths", &["--format=junit", "-F", "builds"], &env);
    assert_eq!(junit.code, 1);
    assert!(junit.stdout.contains(r#"type="test_run_failed""#));
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn proc_macro_test_binary_is_listed_and_run() {
    let run = floyd("proc-macro-test", &["--format=json"], &[]);
    let j = run.json();
    assert_eq!(j["verdict"], "analysed", "{}", run.stderr);
    assert_eq!(j["tests_discovered"].as_array().unwrap().len(), 3);
    assert_eq!(j["summary"]["percentage"], 100.0);
    assert_eq!(run.code, 0);
}
