//! End-to-end checks of `cargo floyd test`'s error paths and
//! options, on the standalone fixtures `fixtures/failure-paths` and
//! `fixtures/proc-macro-test`:
//!
//! - a build failure prints rustc's diagnostic to stderr and, under
//!   `--format=json` / `--format=junit`, a `build_failed` document;
//! - `--features` reaches the instrumented build;
//! - a failing `--list` gives `test_discovery_failed`, a test run
//!   that leaves no coverage profile gives `test_run_failed`;
//! - MIR files skipped by `--max-mir-size` are named on stderr and in
//!   the JSON, and the `no_candidates` message says they are why;
//! - a proc-macro crate's test binary (which links `libstd`
//!   dynamically) is listed and run;
//! - the instrumented build goes into `<target>/floyd`, so plain
//!   `cargo +nightly test` binaries in the target directory are not
//!   mistaken for it, and a second run reuses it.
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
fn oversize_mir_skip_is_reported() {
    let run = floyd(
        "failure-paths",
        &["--format=json", "-F", "builds", "--max-mir-size=0"],
        &[],
    );
    assert_eq!(run.code, 1);
    let j = run.json();
    assert_eq!(j["verdict"], "no_candidates");
    assert_eq!(j["max_mir_size_mb"], 0);
    assert_eq!(j["tests_discovered"].as_array().unwrap().len(), 0);
    let skipped = j["skipped_oversize_mirs"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{j:#}");
    let path = skipped[0]["path"].as_str().unwrap();
    assert!(path.ends_with(".mir"));
    assert!(skipped[0]["bytes"].as_u64().unwrap() > 0);
    // stderr names the file, the limit and the flag, and the decline
    // message gives the skip as the reason.
    assert!(run.stderr.contains(path), "{}", run.stderr);
    assert!(run.stderr.contains("--max-mir-size limit of 0 MB"));
    assert!(run.stderr.contains("--max-mir-size=50"));
    assert!(run.stderr.contains(
        "no recognised decisions: the MIR of all 1 test target(s) exceeded --max-mir-size=0"
    ));

    let text = floyd("failure-paths", &["-F", "builds", "--max-mir-size=0"], &[]);
    assert!(text.stdout.contains("above the 0 MB scalability threshold"));
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

/// Files under `dir` with extension `ext`, with their modification
/// times.
fn files_with_mtimes(dir: &Path, ext: &str) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == ext) {
                let mtime = e.metadata().and_then(|m| m.modified()).expect("mtime");
                out.push((p, mtime));
            }
        }
    }
    out.sort();
    out
}

/// A fresh, empty target directory under `CARGO_TARGET_TMPDIR`.
fn fresh_target(name: &str) -> PathBuf {
    let t = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&t);
    t
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn a_plain_nightly_test_build_does_not_hide_the_instrumented_one() {
    // `cargo +nightly test` first leaves uninstrumented test binaries
    // (no `.mir`, no coverage map) in the target directory. Cargo
    // does not fingerprint what the rustc shim adds, so building in
    // the same directory reused them and Floyd found no test
    // artifacts. It builds in `<target>/floyd` instead.
    let target = fresh_target("nightly-first-target");
    let manifest = fixture("failure-paths").join("Cargo.toml");
    let plain = Command::new("cargo")
        .args([
            "+nightly",
            "test",
            "--features",
            "builds",
            "--manifest-path",
        ])
        .arg(&manifest)
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .output()
        .expect("run cargo +nightly test");
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(files_with_mtimes(&target, "mir").is_empty());

    let run = floyd_in_target(
        "failure-paths",
        &["--format=json", "--features=builds"],
        &target,
    );
    let j = run.json();
    assert_eq!(j["verdict"], "analysed", "{}", run.stderr);
    assert_eq!(j["summary"]["percentage"], 100.0);
    assert_eq!(run.code, 0);
    let mirs = files_with_mtimes(&target.join("floyd"), "mir");
    assert!(!mirs.is_empty(), "no MIR under {}/floyd", target.display());
    assert_eq!(
        files_with_mtimes(&target, "mir"),
        mirs,
        "MIR outside <target>/floyd"
    );
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn a_second_run_reuses_the_instrumented_build() {
    let target = fresh_target("second-run-target");
    let args = ["--format=json", "--features=builds"];
    let first = floyd_in_target("failure-paths", &args, &target);
    assert_eq!(first.json()["verdict"], "analysed", "{}", first.stderr);
    let built = files_with_mtimes(&target.join("floyd"), "mir");
    assert!(!built.is_empty());

    let second = floyd_in_target("failure-paths", &args, &target);
    assert_eq!(second.json()["verdict"], "analysed", "{}", second.stderr);
    assert_eq!(second.json()["summary"], first.json()["summary"]);
    assert_eq!(
        files_with_mtimes(&target.join("floyd"), "mir"),
        built,
        "the second run recompiled the fixture"
    );
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn target_dir_option_overrides_cargo_target_dir() {
    let target = fresh_target("target-dir-option-target");
    let env_target = fresh_target("target-dir-option-env-target");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cargo-floyd"));
    cmd.args([
        "floyd",
        "test",
        "--format=json",
        "--features=builds",
        "--target-dir",
    ])
    .arg(&target)
    .arg(fixture("failure-paths"))
    .env("CARGO_TARGET_DIR", &env_target);
    let out = cmd.output().expect("run cargo-floyd");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let j: Value = serde_json::from_str(&stdout).expect("JSON");
    assert_eq!(
        j["verdict"],
        "analysed",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!files_with_mtimes(&target.join("floyd"), "mir").is_empty());
    assert!(!env_target.exists());
}

/// `cargo floyd test <args..> fixtures/<name>` with the project
/// target directory `target` (`CARGO_TARGET_DIR`), run in the
/// directory `<target>.cwd`, which must be left without coverage
/// profiles.
fn floyd_in_target(name: &str, args: &[&str], target: &Path) -> Run {
    let cwd = target.with_extension("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let out = Command::new(env!("CARGO_BIN_EXE_cargo-floyd"))
        .current_dir(&cwd)
        .args(["floyd", "test"])
        .args(args)
        .arg(fixture(name))
        .env("CARGO_TARGET_DIR", target)
        .env_remove("FLOYD_FIXTURE_FAIL_LIST")
        .env_remove("FLOYD_FIXTURE_NO_PROFILE")
        .output()
        .expect("run cargo-floyd");
    assert_eq!(
        files_with_mtimes(&cwd, "profraw"),
        vec![],
        "coverage profiles left in the working directory"
    );
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().expect("exit code"),
    }
}

#[test]
fn all_candidates_is_rejected_outside_cargo_test_mode() {
    // Checked before anything is built, so no toolchain is needed.
    let src = fixture("mcdc-cases").join("src/lib.rs");
    for args in [
        vec!["floyd", "test", "--all-candidates"],
        vec!["floyd", "--all-candidates"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_cargo-floyd"))
            .args(&args)
            .arg(&src)
            .output()
            .expect("run cargo-floyd");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr
                .contains("--all-candidates applies to `cargo floyd test` on cargo projects only"),
            "{args:?}: {stderr}"
        );
    }
}
