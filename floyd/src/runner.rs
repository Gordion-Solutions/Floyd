//! Execute instrumented binaries and ingest the resulting profraw data.
//!
//! Per ADR-0002 (Q4 resolution), MC/DC analysis needs per-test
//! observations of condition values. This module:
//!
//! - Locates the `llvm-profdata` / `llvm-cov` binaries that ship in
//!   the `llvm-tools-preview` rustup component.
//! - Runs an instrumented binary with a unique `LLVM_PROFILE_FILE`
//!   per invocation, so each invocation produces a separate
//!   `.profraw` containing only that run's counter values.
//! - Merges and exports each `.profraw` to JSON via subprocess and
//!   parses the result through [`crate::profile::parse`].
//!
//! For Phase 1 the API is intentionally low-level — `cargo-floyd`
//! orchestrates many runs via these primitives. A higher-level
//! cargo-aware driver lands as part of the cargo project support
//! follow-up.

use crate::profile;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Error from any runner operation.
#[derive(Debug, Clone)]
pub struct RunnerError {
    /// Which step produced the failure.
    pub stage: &'static str,
    /// Human-readable message.
    pub message: String,
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runner error ({}): {}", self.stage, self.message)
    }
}

impl std::error::Error for RunnerError {}

/// Locate a binary in the active nightly toolchain's
/// `llvm-tools-preview` component.
///
/// Probes `rustc +nightly --print=sysroot` and walks
/// `<sysroot>/lib/rustlib/<triple>/bin/<name>`. Returns an error if
/// no candidate exists (typically because `llvm-tools-preview` is
/// not installed: `rustup component add llvm-tools-preview --toolchain nightly`).
pub fn llvm_tool(name: &str) -> Result<PathBuf, RunnerError> {
    let out = Command::new("rustc")
        .args(["+nightly", "--print=sysroot"])
        .output()
        .map_err(|e| RunnerError {
            stage: "rustc --print=sysroot",
            message: format!("could not exec: {e}"),
        })?;
    if !out.status.success() {
        return Err(RunnerError {
            stage: "rustc --print=sysroot",
            message: format!("exit status {}", out.status),
        });
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let rustlib = Path::new(&sysroot).join("lib").join("rustlib");
    let entries = std::fs::read_dir(&rustlib).map_err(|e| RunnerError {
        stage: "find_llvm_tool",
        message: format!("read {}: {e}", rustlib.display()),
    })?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("bin").join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(RunnerError {
        stage: "find_llvm_tool",
        message: format!(
            "{name} not found under {}; install with: rustup component add llvm-tools-preview --toolchain nightly",
            rustlib.display()
        ),
    })
}

/// Enumerate the `#[test]` functions in an instrumented test binary
/// produced by [`crate::instrument::compile_test_target`].
///
/// Invokes the binary with `--list --format=terse` and parses the
/// libtest output (one `<name>: test` line per test). Filters out
/// the summary line and any benchmark entries.
pub fn list_tests(binary: &Path) -> Result<Vec<String>, RunnerError> {
    let out = Command::new(binary)
        .args(["--list", "--format=terse"])
        .output()
        .map_err(|e| RunnerError {
            stage: "list_tests",
            message: format!("could not exec {}: {e}", binary.display()),
        })?;
    if !out.status.success() {
        return Err(RunnerError {
            stage: "list_tests",
            message: format!(
                "binary exited with status {}; stderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            ),
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut tests = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_suffix(": test") {
            tests.push(name.to_string());
        }
    }
    Ok(tests)
}

/// Run a single test from an instrumented test binary, capturing its
/// coverage to `profraw`. Equivalent to
/// `<binary> --exact <test_name>` with `LLVM_PROFILE_FILE` set.
///
/// Like [`run`], does not check the test's exit status — a failing
/// `#[test]` still produces a valid `profraw`.
pub fn run_test_isolated(
    binary: &Path,
    test_name: &str,
    profraw: &Path,
) -> Result<(), RunnerError> {
    let out = Command::new(binary)
        .args(["--exact", test_name])
        .env("LLVM_PROFILE_FILE", profraw)
        .output()
        .map_err(|e| RunnerError {
            stage: "run_test_isolated",
            message: format!("could not exec {}: {e}", binary.display()),
        })?;
    if !profraw.exists() {
        return Err(RunnerError {
            stage: "run_test_isolated",
            message: format!(
                "no profraw produced at {} for test {test_name}; binary stderr:\n{}",
                profraw.display(),
                String::from_utf8_lossy(&out.stderr)
            ),
        });
    }
    Ok(())
}

/// Run an instrumented binary once, writing coverage counters to
/// `profraw`. Other arguments are forwarded to the binary as `argv`.
///
/// Does not check the binary's exit status — instrumented tests may
/// legitimately exit non-zero on assertion failure while still
/// producing a valid `profraw`. Errors out only if `profraw` is not
/// created.
pub fn run(binary: &Path, argv: &[&str], profraw: &Path) -> Result<(), RunnerError> {
    let out = Command::new(binary)
        .args(argv)
        .env("LLVM_PROFILE_FILE", profraw)
        .output()
        .map_err(|e| RunnerError {
            stage: "run binary",
            message: format!("could not exec {}: {e}", binary.display()),
        })?;
    if !profraw.exists() {
        return Err(RunnerError {
            stage: "run binary",
            message: format!(
                "no profraw produced at {}; binary stderr:\n{}",
                profraw.display(),
                String::from_utf8_lossy(&out.stderr)
            ),
        });
    }
    Ok(())
}

/// Merge a `.profraw` to `.profdata` and export the per-function
/// coverage JSON, returning a parsed [`profile::CoverageReport`].
///
/// `workdir` is used as the scratch directory for the intermediate
/// `.profdata` file.
pub fn profraw_to_coverage(
    profraw: &Path,
    profdata_tool: &Path,
    cov_tool: &Path,
    binary: &Path,
    workdir: &Path,
) -> Result<profile::CoverageReport, RunnerError> {
    let pdata_name = profraw
        .file_stem()
        .map(|s| {
            let mut o = std::ffi::OsString::from(s);
            o.push(".profdata");
            o
        })
        .unwrap_or_else(|| "merged.profdata".into());
    let pdata = workdir.join(&pdata_name);

    let out = Command::new(profdata_tool)
        .args(["merge", "-sparse"])
        .arg(profraw)
        .arg("-o")
        .arg(&pdata)
        .output()
        .map_err(|e| RunnerError {
            stage: "llvm-profdata merge",
            message: format!("could not exec: {e}"),
        })?;
    if !out.status.success() {
        return Err(RunnerError {
            stage: "llvm-profdata merge",
            message: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let out = Command::new(cov_tool)
        .args(["export", "--format=text", "--instr-profile"])
        .arg(&pdata)
        .arg(binary)
        .output()
        .map_err(|e| RunnerError {
            stage: "llvm-cov export",
            message: format!("could not exec: {e}"),
        })?;
    if !out.status.success() {
        return Err(RunnerError {
            stage: "llvm-cov export",
            message: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let cov_text = String::from_utf8(out.stdout).map_err(|e| RunnerError {
        stage: "llvm-cov export",
        message: format!("utf8: {e}"),
    })?;
    profile::parse(&cov_text).map_err(|e| RunnerError {
        stage: "parse coverage",
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_includes_stage_and_message() {
        let e = RunnerError {
            stage: "llvm-cov export",
            message: "boom".to_string(),
        };
        let s = format!("{e}");
        assert!(s.contains("llvm-cov export"));
        assert!(s.contains("boom"));
    }
}
