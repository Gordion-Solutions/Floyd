//! Class 7: processes other than the test itself.
//!
//! Floyd runs each test with `LLVM_PROFILE_FILE` naming that test's
//! profile, and builds the crate with its instrumentation. Neither
//! may reach processes the test or the build spawns:
//!
//! - A child process (here: this test binary rerun as a child, as
//!   test suites that spawn their own binary or a `trybuild`
//!   compiler do) must not write its counters into the test's
//!   profile, which would attribute the child's evaluations to the
//!   test, nor into the project directory.
//! - The environment Floyd sets for its instrumented build
//!   (`RUSTC_WRAPPER`, rustflags) must not reach the test, or a
//!   `cargo`/`rustc` the test spawns builds instrumented programs.
//! - The crate's build script (`build.rs`) must not be instrumented:
//!   cargo runs it in the package directory without
//!   `LLVM_PROFILE_FILE`, so it would leave `default_*.profraw`
//!   there.

/// `a && b`, evaluated (T,T) -> T by `spawned_and_spawner`, which
/// then reruns this test binary as a child that runs
/// `spawned_and_child`, (F,-) -> F.
///
/// Expected (`--only=spawned_and`): two vector observations, one per
/// test: `spawned_and_spawner` `a=T, b=T -> T` only (the child's
/// evaluation is not in its profile), and `spawned_and_child`
/// `a=F -> F`; `a` exercised, `b` not, `summary.percentage == 50`,
/// exit 2. No `*.profraw` appears in the fixture directory.
pub fn spawned_and(a: bool, b: bool) -> bool {
    a && b
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Variables through which Floyd's build would reach a `cargo` or
    /// `rustc` a test spawns, and the Floyd-specific text that marks a
    /// leaked value: the `floyd-rustc-shim` wrapper and Floyd's
    /// rustflags.
    const BUILD_VARS: &[&str] = &[
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
    ];
    const FLOYD_MARKS: &[&str] = &[
        "floyd-rustc-shim",
        "-Zmir-include-spans",
        "-Zcoverage-options",
    ];

    /// Run directly by Floyd, and as a child of `spawned_and_spawner`.
    #[test]
    fn spawned_and_child() {
        assert!(!spawned_and(false, true));
    }

    #[test]
    fn spawned_and_spawner() {
        assert!(spawned_and(true, true));
        let out = Command::new(std::env::current_exe().expect("current_exe"))
            .args(["--exact", "child_process::tests::spawned_and_child"])
            .arg("--test-threads=1")
            .output()
            .expect("spawn the test binary");
        assert!(out.status.success(), "child exited with {}", out.status);
    }

    /// Floyd ignores test outcomes, so a leak is also reported through
    /// the file named by `MCDC_CASES_LEAK_MARKER`, which
    /// `cargo-floyd/tests/fixture_cases.rs` checks.
    #[test]
    fn floyd_build_environment_is_not_inherited() {
        // Plain loops, no closures: a closure's MIR name drops the
        // `tests` module, and its decisions would be reported.
        let mut leaked = Vec::new();
        for var in BUILD_VARS {
            let Some(value) = std::env::var_os(var) else {
                continue;
            };
            let value = value.to_string_lossy();
            for mark in FLOYD_MARKS {
                if value.contains(mark) {
                    leaked.push(format!("{var}={value}"));
                    break;
                }
            }
        }
        if !leaked.is_empty() {
            if let Some(marker) = std::env::var_os("MCDC_CASES_LEAK_MARKER") {
                let _ = std::fs::write(marker, leaked.join("\n"));
            }
        }
        assert!(
            leaked.is_empty(),
            "Floyd's build environment leaked: {leaked:?}"
        );
    }
}
