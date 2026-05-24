//! Compile Rust sources with coverage + MIR instrumentation.
//!
//! Per ADR-0002, Floyd uses nightly rustc with:
//! - `--emit=mir -Zmir-include-spans` to obtain span-annotated MIR for
//!   the static decision recovery.
//! - `-Cinstrument-coverage -Zcoverage-options=branch,condition` to
//!   embed the LLVM coverage map that the runner side reads through
//!   `llvm-profdata` + `llvm-cov`.
//!
//! Phase 1 scope: single-file rustc invocations sufficient for the
//! corpus patterns. Cargo-aware compilation (workspace discovery,
//! test target enumeration, dependency build) lands as a separate
//! `compile_cargo_project` entry point in a follow-up commit.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Result of a successful [`compile_with_coverage`] invocation.
#[derive(Debug, Clone)]
pub struct InstrumentResult {
    /// Path to the emitted MIR text dump (`-Zmir-include-spans` form).
    pub mir_path: PathBuf,
    /// Path to the instrumented executable.
    pub binary_path: PathBuf,
}

/// Error returned when one of the rustc invocations fails.
#[derive(Debug, Clone)]
pub struct InstrumentError {
    /// Which compilation step produced the failure.
    pub stage: &'static str,
    /// Captured `stderr` (or a message describing the exec failure).
    pub stderr: String,
}

impl std::fmt::Display for InstrumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "instrument error ({}):\n{}", self.stage, self.stderr)
    }
}

impl std::error::Error for InstrumentError {}

/// Emit MIR for a single `.rs` source file as a `lib` crate.
///
/// Useful when you want static decision analysis without producing
/// or running an instrumented binary — typical for the corpus
/// patterns (no `fn main`). Uses `-Zmir-include-spans` so source
/// spans are available for any future correlate step.
pub fn emit_mir(source: &Path, workdir: &Path) -> Result<PathBuf, InstrumentError> {
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let mir_path = workdir.join(format!("{stem}.mir"));
    rustc_nightly(
        "rustc --emit=mir (lib)",
        &[
            "--edition",
            "2021",
            "--crate-type",
            "lib",
            "--emit=mir",
            "-Zmir-include-spans",
        ],
        &mir_path,
        source,
    )?;
    Ok(mir_path)
}

/// Compile a single `.rs` source file as a *test* crate with both
/// MIR emission and runtime coverage instrumentation.
///
/// The `--test` flag wraps the source in libtest's test runner so
/// `#[test]` functions become individually invocable via
/// `--exact <name>`. Used by `cargo-floyd test` (runtime mode) to
/// drive per-test profraw collection.
pub fn compile_test_target(
    source: &Path,
    workdir: &Path,
) -> Result<InstrumentResult, InstrumentError> {
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let mir_path = workdir.join(format!("{stem}.mir"));
    let binary_path = workdir.join(stem);

    rustc_nightly(
        "rustc --test --emit=mir",
        &[
            "--edition",
            "2021",
            "--crate-type",
            "bin",
            "--test",
            "--emit=mir",
            "-Zmir-include-spans",
        ],
        &mir_path,
        source,
    )?;

    rustc_nightly(
        "rustc --test -Cinstrument-coverage",
        &[
            "--edition",
            "2021",
            "--crate-type",
            "bin",
            "--test",
            "-Cinstrument-coverage",
            "-Zcoverage-options=branch,condition",
        ],
        &binary_path,
        source,
    )?;

    Ok(InstrumentResult {
        mir_path,
        binary_path,
    })
}

/// Compile a single `.rs` source file with both MIR emission and
/// runtime coverage instrumentation enabled.
///
/// Runs two rustc invocations:
/// 1. `rustc +nightly --emit=mir -Zmir-include-spans` — writes the
///    MIR text into `workdir/<stem>.mir`.
/// 2. `rustc +nightly -Cinstrument-coverage -Zcoverage-options=branch,condition`
///    — produces the executable at `workdir/<stem>`.
///
/// Returns both paths so the caller can hand them to
/// [`crate::mir::parse_text`] and [`crate::runner`] respectively.
///
/// Requires a nightly toolchain reachable via `rustc +nightly`.
pub fn compile_with_coverage(
    source: &Path,
    workdir: &Path,
) -> Result<InstrumentResult, InstrumentError> {
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let mir_path = workdir.join(format!("{stem}.mir"));
    let binary_path = workdir.join(stem);

    rustc_nightly(
        "rustc --emit=mir",
        &[
            "--edition",
            "2021",
            "--crate-type",
            "bin",
            "--emit=mir",
            "-Zmir-include-spans",
        ],
        &mir_path,
        source,
    )?;

    rustc_nightly(
        "rustc -Cinstrument-coverage",
        &[
            "--edition",
            "2021",
            "--crate-type",
            "bin",
            "-Cinstrument-coverage",
            "-Zcoverage-options=branch,condition",
        ],
        &binary_path,
        source,
    )?;

    Ok(InstrumentResult {
        mir_path,
        binary_path,
    })
}

fn rustc_nightly(
    stage: &'static str,
    flags: &[&str],
    output: &Path,
    source: &Path,
) -> Result<(), InstrumentError> {
    let mut cmd = Command::new("rustc");
    cmd.arg("+nightly");
    for f in flags {
        cmd.arg(f);
    }
    cmd.arg("-o").arg(output).arg(source);
    let out = cmd.output().map_err(|e| InstrumentError {
        stage,
        stderr: format!("could not exec rustc: {e}"),
    })?;
    if !out.status.success() {
        return Err(InstrumentError {
            stage,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_includes_stage_and_stderr() {
        let e = InstrumentError {
            stage: "rustc --emit=mir",
            stderr: "boom".to_string(),
        };
        let s = format!("{e}");
        assert!(s.contains("rustc --emit=mir"));
        assert!(s.contains("boom"));
    }
}
