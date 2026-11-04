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
    /// Captured `stdout` of the failing command, empty when it was
    /// not run or printed nothing. For the cargo build this is the
    /// `--message-format=json` stream, which carries rustc's
    /// diagnostics (`compiler-message` entries).
    pub stdout: String,
}

impl InstrumentError {
    fn new(stage: &'static str, stderr: impl Into<String>) -> Self {
        InstrumentError {
            stage,
            stderr: stderr.into(),
            stdout: String::new(),
        }
    }
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

/// One test artifact produced by [`compile_cargo_project`].
#[derive(Debug, Clone)]
pub struct TestArtifact {
    /// Path to the instrumented `#[test]` binary that responds to
    /// `--list` and `--exact <name>`.
    pub binary_path: PathBuf,
    /// Path to the span-annotated MIR text dump emitted alongside the
    /// binary. Sits at `<binary>.mir` per rustc's `--emit=mir,link`.
    pub mir_path: PathBuf,
    /// Cargo package name the artifact came from (for diagnostics).
    pub package_name: String,
    /// Cargo target name (`target.name` in cargo's artifact message).
    pub target_name: String,
    /// Cargo target kinds (`target.kind`, e.g. `["lib"]`, `["test"]`).
    pub target_kinds: Vec<String>,
    /// Root source file of the target (`target.src_path`).
    pub src_path: PathBuf,
    /// `Cargo.toml` of the package that owns the target, from
    /// `cargo metadata`. `None` if the package id was not listed.
    /// Callers read target settings cargo does not report in its JSON
    /// (notably `harness = false`) from this manifest.
    pub manifest_path: Option<PathBuf>,
}

/// Result of a successful [`compile_cargo_project`] invocation.
#[derive(Debug, Clone)]
pub struct CargoBuildResult {
    /// Workspace root from `cargo metadata`.
    pub workspace_root: PathBuf,
    /// One [`TestArtifact`] per cargo test target that produced both
    /// a binary and a MIR file.
    pub test_artifacts: Vec<TestArtifact>,
}

/// Build a cargo project's test targets with MIR emission and coverage
/// instrumentation enabled.
///
/// Internally:
/// 1. Probes `cargo metadata` to discover the workspace root.
/// 2. Runs `cargo +nightly build --tests --message-format=json` with
///    `RUSTFLAGS=-Cinstrument-coverage -Zcoverage-options=branch,condition
///                -Zmir-include-spans --emit=mir,link`.
/// 3. Parses the JSON message stream for `compiler-artifact`
///    entries flagged as test targets and pairs each binary with its
///    sibling `.mir` file.
///
/// `manifest_path` may be a `Cargo.toml` path or a directory
/// containing one; it gets passed through to cargo as
/// `--manifest-path`. Requires the nightly toolchain.
pub fn compile_cargo_project(manifest_path: &Path) -> Result<CargoBuildResult, InstrumentError> {
    compile_cargo_project_with_args(manifest_path, &[])
}

/// [`compile_cargo_project`] with extra arguments appended to the
/// `cargo build --tests` command line, such as `--features=a,b`,
/// `--all-features` or `--no-default-features`.
///
/// On a build failure the returned error's `stdout` holds cargo's
/// JSON message stream, so the caller can render rustc's
/// diagnostics.
pub fn compile_cargo_project_with_args(
    manifest_path: &Path,
    cargo_args: &[String],
) -> Result<CargoBuildResult, InstrumentError> {
    let manifest = if manifest_path.is_dir() {
        manifest_path.join("Cargo.toml")
    } else {
        manifest_path.to_path_buf()
    };
    if !manifest.exists() {
        return Err(InstrumentError::new(
            "cargo project",
            format!("Cargo.toml not found at {}", manifest.display()),
        ));
    }

    // Step 1: cargo metadata to discover workspace.
    let metadata_out = Command::new("cargo")
        .args([
            "+nightly",
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .map_err(|e| {
            InstrumentError::new("cargo metadata", format!("could not exec cargo: {e}"))
        })?;
    if !metadata_out.status.success() {
        return Err(InstrumentError::new(
            "cargo metadata",
            String::from_utf8_lossy(&metadata_out.stderr).into_owned(),
        ));
    }
    let metadata: Option<serde_json::Value> = serde_json::from_slice(&metadata_out.stdout).ok();
    let package_manifests: std::collections::HashMap<String, PathBuf> = metadata
        .as_ref()
        .and_then(|v| v.get("packages"))
        .and_then(|p| p.as_array())
        .map(|pkgs| {
            pkgs.iter()
                .filter_map(|p| {
                    let id = p.get("id")?.as_str()?.to_string();
                    let mp = p.get("manifest_path")?.as_str()?;
                    Some((id, PathBuf::from(mp)))
                })
                .collect()
        })
        .unwrap_or_default();
    let workspace_root: PathBuf = metadata
        .as_ref()
        .and_then(|v| {
            v.get("workspace_root")
                .and_then(|w| w.as_str())
                .map(PathBuf::from)
        })
        .ok_or_else(|| {
            InstrumentError::new(
                "cargo metadata",
                "could not parse workspace_root".to_string(),
            )
        })?;

    // Step 2: cargo build --tests. The caller sets
    // `RUSTC_WRAPPER` to the `floyd-rustc-shim`, which appends
    // Floyd's instrumentation flags ONLY for the primary package
    // (via `CARGO_PRIMARY_PACKAGE`). Dep crates compile without
    // `--emit=mir` and without coverage instrumentation — critical
    // on real embedded crates like `heapless` where the generic
    // dep tree would otherwise produce hundreds of gigabytes of
    // MIR text.
    let build_out = Command::new("cargo")
        .args([
            "+nightly",
            "build",
            "--tests",
            "--message-format=json",
            "--manifest-path",
        ])
        .arg(&manifest)
        .args(cargo_args)
        .output()
        .map_err(|e| {
            InstrumentError::new("cargo build --tests", format!("could not exec cargo: {e}"))
        })?;
    if !build_out.status.success() {
        return Err(InstrumentError {
            stage: "cargo build --tests",
            stderr: String::from_utf8_lossy(&build_out.stderr).into_owned(),
            stdout: String::from_utf8_lossy(&build_out.stdout).into_owned(),
        });
    }

    // Step 3: parse JSON stream for test artifacts.
    let mut test_artifacts = Vec::new();
    for line in build_out.stdout.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let target = msg.get("target");
        let is_test = target.and_then(|t| t.get("test")).and_then(|b| b.as_bool()) == Some(true);
        if !is_test {
            continue;
        }
        let executable = msg
            .get("executable")
            .and_then(|e| e.as_str())
            .map(PathBuf::from);
        let package_name = msg
            .get("package_id")
            .and_then(|p| p.as_str())
            .unwrap_or("?")
            .to_string();
        if let Some(bin) = executable {
            let mir = bin.with_extension("mir");
            if !mir.exists() {
                // Some artifacts (e.g. proc-macros, build scripts) may
                // not produce a sibling .mir. Skip without erroring;
                // the test target wasn't compiled with our flags.
                continue;
            }
            let target_name = target
                .and_then(|t| t.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or_default()
                .to_string();
            let target_kinds = target
                .and_then(|t| t.get("kind"))
                .and_then(|k| k.as_array())
                .map(|ks| {
                    ks.iter()
                        .filter_map(|k| k.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let src_path = target
                .and_then(|t| t.get("src_path"))
                .and_then(|p| p.as_str())
                .map(PathBuf::from)
                .unwrap_or_default();
            let manifest_path = package_manifests.get(&package_name).cloned();
            test_artifacts.push(TestArtifact {
                binary_path: bin,
                mir_path: mir,
                package_name,
                target_name,
                target_kinds,
                src_path,
                manifest_path,
            });
        }
    }

    if test_artifacts.is_empty() {
        return Err(InstrumentError::new(
            "cargo build --tests",
            "no test artifacts produced; does the project have any #[test] functions?".to_string(),
        ));
    }

    Ok(CargoBuildResult {
        workspace_root,
        test_artifacts,
    })
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
    let out = cmd
        .output()
        .map_err(|e| InstrumentError::new(stage, format!("could not exec rustc: {e}")))?;
    if !out.status.success() {
        return Err(InstrumentError::new(
            stage,
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_includes_stage_and_stderr() {
        let e = InstrumentError::new("rustc --emit=mir", "boom".to_string());
        let s = format!("{e}");
        assert!(s.contains("rustc --emit=mir"));
        assert!(s.contains("boom"));
    }
}
