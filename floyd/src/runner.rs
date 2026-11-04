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
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

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

/// The environment variable the platform's dynamic loader searches
/// for shared libraries: `DYLD_LIBRARY_PATH` on macOS, `PATH` on
/// Windows, `LD_LIBRARY_PATH` elsewhere.
pub fn dylib_path_var() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_LIBRARY_PATH"
    } else if cfg!(windows) {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// The nightly toolchain's shared-library directories: the host
/// target libdir (`rustc +nightly --print target-libdir`, which holds
/// `libstd-*.dylib`/`.so`) and `<sysroot>/lib` (which holds
/// `librustc_driver-*`). Computed once; empty when `rustc` cannot be
/// queried, in which case binaries are spawned with the inherited
/// environment.
fn toolchain_lib_dirs() -> &'static [PathBuf] {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let print = |what: &str| -> Option<PathBuf> {
            let out = Command::new("rustc")
                .args(["+nightly", "--print", what])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!s.is_empty()).then(|| PathBuf::from(s))
        };
        let mut dirs = Vec::new();
        if let Some(libdir) = print("target-libdir") {
            dirs.push(libdir);
        }
        if let Some(sysroot) = print("sysroot") {
            dirs.push(sysroot.join("lib"));
        }
        dirs.retain(|d| d.is_dir());
        dirs
    })
}

/// `prepend` followed by the entries of `existing` (an inherited
/// search-path value), joined with the platform separator. Entries
/// of `prepend` already in `existing` are not repeated.
fn join_dylib_path(prepend: &[PathBuf], existing: Option<&std::ffi::OsStr>) -> OsString {
    let inherited: Vec<PathBuf> = existing
        .map(|v| std::env::split_paths(v).collect())
        .unwrap_or_default();
    let mut all: Vec<PathBuf> = Vec::new();
    for d in prepend.iter().chain(&inherited) {
        if !d.as_os_str().is_empty() && !all.contains(d) {
            all.push(d.clone());
        }
    }
    std::env::join_paths(all).unwrap_or_else(|_| existing.map(OsString::from).unwrap_or_default())
}

/// A [`Command`] for an instrumented test binary with the toolchain's
/// shared-library directories and the binary's own directory
/// prepended to the dynamic-loader search path, as `cargo test`
/// does. Test binaries of proc-macro crates (and anything else built
/// with `-C prefer-dynamic`) link `libstd` dynamically and do not
/// start without it.
fn test_binary_command(binary: &Path) -> Command {
    let var = dylib_path_var();
    let mut dirs: Vec<PathBuf> = toolchain_lib_dirs().to_vec();
    if let Some(parent) = binary.parent() {
        dirs.push(parent.to_path_buf());
    }
    let mut cmd = Command::new(binary);
    cmd.env(
        var,
        join_dylib_path(&dirs, std::env::var_os(var).as_deref()),
    );
    cmd
}

/// Enumerate the `#[test]` functions in an instrumented test binary
/// produced by [`crate::instrument::compile_test_target`].
///
/// Invokes the binary with `--list --format=terse` and parses the
/// libtest output (one `<name>: test` line per test). Filters out
/// the summary line and any benchmark entries.
pub fn list_tests(binary: &Path) -> Result<Vec<String>, RunnerError> {
    let out = test_binary_command(binary)
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
    let out = test_binary_command(binary)
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
    let out = test_binary_command(binary)
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
        let raw = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(RunnerError {
            stage: "llvm-cov export",
            message: wrap_llvm_crash_message(&raw),
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

/// Issue #15: llvm-cov crashes with a raw LLVM stack trace that reads
/// like Floyd itself failed. Detect the crash signature and prepend a
/// human-readable summary so the user understands this is an upstream
/// LLVM issue, not a Floyd bug.
fn wrap_llvm_crash_message(raw: &str) -> String {
    let is_llvm_crash =
        raw.contains("PLEASE submit a bug report") || raw.contains("llvm::sys::PrintStackTrace");
    if is_llvm_crash {
        format!(
            "llvm-cov crashed while exporting coverage. This is an upstream LLVM bug \
             (see https://github.com/llvm/llvm-project/issues/), not a Floyd failure \
             — Floyd itself did not crash. Raw stack trace follows for a bug report:\n\n{raw}"
        )
    } else {
        raw.to_string()
    }
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

    /// Issue #15: a raw LLVM stack trace must get a human-readable
    /// prefix so users don't think Floyd crashed.
    #[test]
    fn wraps_llvm_crash_with_floyd_disclaimer() {
        let raw_stack = "PLEASE submit a bug report to https://github.com/llvm/llvm-project/issues/\n\
                         Stack dump without symbol names (ensure you have llvm-symbolizer…\n\
                         0  llvm-cov                 0x000000010f896098 llvm::sys::PrintStackTrace…";
        let wrapped = wrap_llvm_crash_message(raw_stack);
        assert!(wrapped.contains("llvm-cov crashed"));
        assert!(wrapped.contains("Floyd itself did not crash"));
        // The original raw stack is preserved for bug reports.
        assert!(wrapped.contains("PrintStackTrace"));
    }

    /// The toolchain dirs go first, inherited entries follow, and an
    /// entry already present is not repeated.
    #[test]
    fn dylib_path_prepends_without_duplicates() {
        let a = PathBuf::from("/tc/lib");
        let b = PathBuf::from("/bin/dir");
        let existing = std::env::join_paths([Path::new("/usr/lib"), Path::new("/tc/lib")]).unwrap();
        let joined = join_dylib_path(&[a.clone(), b.clone()], Some(&existing));
        let parts: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(parts, vec![a, b, PathBuf::from("/usr/lib")]);
        let fresh = join_dylib_path(&[PathBuf::from("/x")], None);
        assert_eq!(fresh, OsString::from("/x"));
    }

    /// A spawned test binary sees the toolchain's libdir (where
    /// `libstd-*` lives) on the loader search path.
    #[test]
    fn test_binary_command_sets_the_loader_path() {
        let cmd = test_binary_command(Path::new("/some/dir/bin"));
        let value = cmd
            .get_envs()
            .find(|(k, _)| *k == dylib_path_var())
            .and_then(|(_, v)| v)
            .expect("loader path set");
        let parts: Vec<PathBuf> = std::env::split_paths(value).collect();
        assert!(parts.contains(&PathBuf::from("/some/dir")));
        for d in toolchain_lib_dirs() {
            assert!(parts.contains(d), "{} missing", d.display());
        }
    }

    /// Non-LLVM-crash stderr (ordinary errors, file-not-found, etc.)
    /// passes through unchanged.
    #[test]
    fn passes_non_llvm_crash_through_unchanged() {
        let raw = "error: profile does not match binary";
        assert_eq!(wrap_llvm_crash_message(raw), raw);
    }
}
