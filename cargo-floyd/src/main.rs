//! `cargo-floyd`: the cargo subcommand driver for the Floyd MC/DC
//! engine.
//!
//! Two modes per ADR-0003's MVP scope:
//!
//! - **Static** (default): `cargo floyd <file>` — compile the source
//!   as a library, decompose its MIR, print the truth table and
//!   per-condition independence pairs.
//! - **Runtime**: `cargo floyd test <file>` — build the source as a
//!   `#[test]`-enabled binary with coverage instrumentation, run each
//!   test individually with its own `LLVM_PROFILE_FILE`, parse the
//!   coverage per test to build per-test [`ConditionObservation`]s,
//!   and report which conditions the observed tests exercise under
//!   masking MC/DC.
//!
//! Both modes support `--format=text` (default) and `--format=json`.
//! Runtime mode additionally supports `--format=junit`, emitting one
//! JUnit `<testcase>` per condition for CIs that render JUnit XML
//! natively (Jenkins, GitLab CI, GitHub Actions, Bazel/Buck2, etc.).

use floyd::correlate;
use floyd::decision;
use floyd::instrument;
use floyd::masking::{
    self, ConditionObservation, ConditionStatus, IndependencePair, RuntimeAnalysis, TruthTableRow,
    Variant,
};
use floyd::mir;
use floyd::profile;
use floyd::runner;

mod harness;
mod selector;
use std::path::Path;
use std::process::ExitCode;

/// How runtime mode resolved candidate selection. Carried in the
/// JSON report so a consumer can tell a real verdict apart from the
/// two decline paths without parsing stderr.
#[derive(serde::Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RuntimeVerdict {
    /// A candidate was picked and `analysis` holds its verdict.
    Analysed,
    /// No decomposable boolean decision was found.
    NoCandidates,
    /// Decomposable decisions were found but no test produced a
    /// usable observation for any of them.
    AllUnobserved,
}

/// A function with a `switchInt`-shaped decision that the engine's
/// current patterns did not match.
#[derive(serde::Serialize, Clone, Debug)]
struct DeclinedFunction {
    name: String,
    source_file: Option<String>,
    line: Option<u32>,
}

/// One decomposable candidate the engine considered, with whether
/// any test reached its function at runtime.
#[derive(serde::Serialize, Clone, Debug)]
struct CandidateSummary {
    function_name: Option<String>,
    source_file: Option<String>,
    line: Option<u32>,
    /// True when at least one test's coverage shows the function
    /// was executed.
    invoked: bool,
    /// Number of tests that produced an observation (a condition
    /// vector or an ambiguous record).
    observations: usize,
}

/// A test-binary MIR file skipped by the size guard.
#[derive(serde::Serialize, Clone, Debug)]
struct SkippedMir {
    path: String,
    bytes: u64,
}

/// A test target built with `harness = false`. Floyd does not run
/// such binaries: they are plain programs that ignore libtest's
/// `--list` / `--exact` flags and would execute their real `main`.
#[derive(serde::Serialize, Clone, Debug)]
struct NoHarnessTarget {
    package: String,
    target: String,
    binary: String,
}

/// JSON document for `cargo floyd test --format=json` on a cargo
/// project. Emitted on every exit path, including the two decline
/// paths, so `out_of_scope` and `candidates` are always obtainable.
#[derive(serde::Serialize)]
struct CargoRuntimeReport<'a> {
    manifest: String,
    workspace_root: String,
    test_targets: usize,
    verdict: RuntimeVerdict,
    tests_discovered: &'a [String],
    observations: &'a [ConditionObservation],
    /// `None` unless `verdict` is `analysed`.
    analysis: Option<&'a RuntimeAnalysis>,
    /// Every decomposable candidate considered, before selection.
    candidates: &'a [CandidateSummary],
    /// Functions with a `switchInt`-shaped decision that the
    /// engine's current patterns did not match. The engine declined
    /// them rather than report a wrong verdict — honest-scope
    /// evidence for ISO 26262 / DO-178C qualification audits.
    out_of_scope: &'a [DeclinedFunction],
    /// MIR files above the size guard that were not analysed.
    skipped_oversize_mirs: &'a [SkippedMir],
    /// Test targets with `harness = false` that were not listed or
    /// run, and so contribute no tests or candidates.
    skipped_no_harness: &'a [NoHarnessTarget],
}

/// JSON document for `cargo floyd test --format=json <file.rs>`.
#[derive(serde::Serialize)]
struct SingleRuntimeReport<'a> {
    source: String,
    verdict: RuntimeVerdict,
    tests_discovered: &'a [String],
    observations: &'a [ConditionObservation],
    /// `None` unless `verdict` is `analysed`.
    analysis: Option<&'a RuntimeAnalysis>,
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), ExitCode> {
    match serde_json::to_string_pretty(value) {
        Ok(s) => {
            println!("{s}");
            Ok(())
        }
        Err(e) => {
            eprintln!("cargo-floyd: serialize JSON: {e}");
            Err(ExitCode::from(1))
        }
    }
}

#[derive(Copy, Clone)]
enum Format {
    Text,
    Json,
    Junit,
}

fn main() -> ExitCode {
    // Cargo invokes external subcommands as `cargo-foo foo ...`, so
    // the first positional argument may be the subcommand name. Strip
    // it if present so the same binary works either way.
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args: Vec<&str> = if raw.first().map(String::as_str) == Some("floyd") {
        raw.iter().skip(1).map(String::as_str).collect()
    } else {
        raw.iter().map(String::as_str).collect()
    };

    // Pull `--format=<fmt>` (or `--format <fmt>`) and
    // `--only=<pat>` (repeatable) out of the args.
    let mut format = Format::Text;
    let mut only_patterns: Vec<String> = Vec::new();
    // --max-mir-size=<N> overrides the per-MIR-file size guard.
    // The guard protects Floyd's parser from monomorphisation
    // explosions (bytes' test_bytes_vec_alloc.mir is 349 MB).
    // Users analysing const-generic libs like fixed (106 MB) can
    // raise it when they are willing to spend the parse time.
    let mut max_mir_mb: Option<u64> = None;
    let mut i = 0;
    while i < args.len() {
        if let Some(pat) = args[i].strip_prefix("--only=") {
            only_patterns.push(pat.to_string());
            args.remove(i);
            continue;
        }
        if let Some(val) = args[i].strip_prefix("--max-mir-size=") {
            match val.parse::<u64>() {
                Ok(mb) => max_mir_mb = Some(mb),
                Err(_) => {
                    eprintln!(
                        "cargo-floyd: --max-mir-size requires a positive \
                         integer in megabytes (e.g. --max-mir-size=200)"
                    );
                    return ExitCode::from(2);
                }
            }
            args.remove(i);
            continue;
        }
        if args[i] == "--max-mir-size" {
            args.remove(i);
            match args.get(i).copied().and_then(|v| v.parse::<u64>().ok()) {
                Some(mb) => {
                    max_mir_mb = Some(mb);
                    args.remove(i);
                }
                None => {
                    eprintln!(
                        "cargo-floyd: --max-mir-size requires a positive \
                         integer in megabytes (e.g. --max-mir-size 200)"
                    );
                    return ExitCode::from(2);
                }
            }
            continue;
        }
        if args[i] == "--only" {
            args.remove(i);
            match args.get(i).copied() {
                Some(v) => {
                    only_patterns.push(v.to_string());
                    args.remove(i);
                }
                None => {
                    eprintln!("cargo-floyd: --only requires a value");
                    return ExitCode::from(2);
                }
            }
            continue;
        }
        if let Some(rest) = args[i].strip_prefix("--format=") {
            format = match parse_format(rest) {
                Ok(f) => f,
                Err(msg) => {
                    eprintln!("cargo-floyd: {msg}");
                    return ExitCode::from(2);
                }
            };
            args.remove(i);
        } else if args[i] == "--format" {
            args.remove(i);
            let value = args.get(i).copied();
            format = match value {
                Some(v) => match parse_format(v) {
                    Ok(f) => f,
                    Err(msg) => {
                        eprintln!("cargo-floyd: {msg}");
                        return ExitCode::from(2);
                    }
                },
                None => {
                    eprintln!("cargo-floyd: --format requires a value");
                    return ExitCode::from(2);
                }
            };
            args.remove(i);
        } else {
            i += 1;
        }
    }

    // Detect the optional `test` subcommand.
    let is_test_mode = args.first().copied() == Some("test");
    if is_test_mode {
        args.remove(0);
    }

    // Detect what `source` actually refers to:
    //   - missing -> default to current dir's Cargo.toml (cargo subcommand idiom)
    //   - a directory -> assume it contains Cargo.toml
    //   - Cargo.toml -> cargo project
    //   - <something>.rs -> single source file
    let arg_path = args.first().map(|s| Path::new(*s));
    let source = match resolve_source(arg_path) {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("cargo-floyd: {msg}");
            eprintln!();
            print_usage();
            return ExitCode::from(2);
        }
    };

    let workdir = std::env::temp_dir().join(format!("cargo-floyd-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&workdir) {
        eprintln!("cargo-floyd: cannot create workdir: {e}");
        return ExitCode::from(1);
    }

    match (is_test_mode, source) {
        (true, Source::Cargo(p)) => {
            run_test_mode_cargo(&p, &workdir, format, &only_patterns, max_mir_mb)
        }
        (true, Source::SingleFile(p)) => run_test_mode_single(&p, &workdir, format),
        (false, Source::Cargo(_)) => {
            eprintln!("cargo-floyd: static analysis on a cargo project is not supported yet;");
            eprintln!("  pass a single .rs file, or use `cargo floyd test` for runtime analysis.");
            ExitCode::from(2)
        }
        (false, Source::SingleFile(p)) => run_static_mode(&p, &workdir, format),
    }
}

enum Source {
    SingleFile(std::path::PathBuf),
    Cargo(std::path::PathBuf),
}

fn resolve_source(arg: Option<&Path>) -> Result<Source, String> {
    let path = match arg {
        None => {
            let cwd = std::env::current_dir()
                .map_err(|e| format!("cannot read current directory: {e}"))?;
            let manifest = cwd.join("Cargo.toml");
            if !manifest.exists() {
                return Err(format!(
                    "no Cargo.toml in {} and no path argument given; specify a source file or run from a cargo project",
                    cwd.display()
                ));
            }
            return Ok(Source::Cargo(manifest));
        }
        Some(p) => p,
    };
    if !path.exists() {
        return Err(format!("source not found: {}", path.display()));
    }
    if path.is_dir() {
        let manifest = path.join("Cargo.toml");
        if manifest.exists() {
            return Ok(Source::Cargo(manifest));
        }
        return Err(format!("directory {} has no Cargo.toml", path.display()));
    }
    if path.file_name() == Some(std::ffi::OsStr::new("Cargo.toml")) {
        return Ok(Source::Cargo(path.to_path_buf()));
    }
    if path.extension() == Some(std::ffi::OsStr::new("rs")) {
        return Ok(Source::SingleFile(path.to_path_buf()));
    }
    Err(format!(
        "unsupported source: {} (expected a .rs file, a Cargo.toml, or a directory containing one)",
        path.display()
    ))
}

fn run_static_mode(source: &Path, workdir: &Path, format: Format) -> ExitCode {
    let mir_path = match instrument::emit_mir(source, workdir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
    };
    let mir_text = match std::fs::read_to_string(&mir_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cargo-floyd: read MIR: {e}");
            return ExitCode::from(1);
        }
    };
    let parsed = match mir::parse_text(&mir_text) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cargo-floyd: parse MIR: {e}");
            return ExitCode::from(1);
        }
    };

    let tree = decision::decompose(&parsed);
    let matrix = masking::analyze(&tree);

    match format {
        Format::Text => print_static_report(source, &tree, &matrix),
        Format::Json => match serde_json::to_string_pretty(&matrix) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("cargo-floyd: serialize JSON: {e}");
                return ExitCode::from(1);
            }
        },
        Format::Junit => {
            eprintln!(
                "cargo-floyd: --format=junit is only meaningful in runtime mode \
                 (use `cargo floyd test`). Static analysis has no test verdicts \
                 to render."
            );
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

fn run_test_mode_cargo(
    manifest: &Path,
    workdir: &Path,
    format: Format,
    only_patterns: &[String],
    max_mir_mb: Option<u64>,
) -> ExitCode {
    // Scope Floyd's `--emit=mir,link -Cinstrument-coverage
    // -Zcoverage-options=branch,condition -Zmir-include-spans` to
    // the primary package only, via a RUSTC_WRAPPER shim that
    // checks CARGO_PRIMARY_PACKAGE. Without this, every dep crate
    // also dumps MIR and gets instrumented — a heapless build
    // produced 336 GB virtual memory before rustc OOM'd on a
    // 30 GB-RAM runner. Finding and setting the shim here (early)
    // means the cargo build invoked inside `floyd::instrument`
    // inherits the env var and routes rustc through our shim.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let shim = parent.join("floyd-rustc-shim");
            if shim.exists() {
                std::env::set_var("RUSTC_WRAPPER", &shim);
            }
        }
    }

    // Build all test artifacts in the cargo project.
    let build = match instrument::compile_cargo_project(manifest) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
    };

    let profdata_tool = match runner::llvm_tool("llvm-profdata") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
    };
    let cov_tool = match runner::llvm_tool("llvm-cov") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
    };

    // Phase 4 cross-artifact correlation (Issue #13 follow-on):
    //
    // Candidates live in the MIR of ONE artifact (typically the lib's,
    // for library functions). The tests exercising those candidates
    // live in OTHER artifacts (the integration-test binaries). So we
    // collect candidates per artifact, run tests per artifact, then
    // correlate every candidate against every test's coverage —
    // regardless of which artifact each came from. Without this,
    // lib candidates always scored 0 observations because the lib
    // artifact itself has no unit tests (e.g. semver's `ptr_as_str`).

    struct ArtifactInfo {
        mir: floyd::Mir,
        binary_path: std::path::PathBuf,
        tests: Vec<String>,
    }

    struct RawCandidate {
        tree: floyd::DecisionTree,
        prov: floyd::DecisionProvenance,
        artifact_idx: usize,
        fn_idx: usize,
    }

    let mut artifacts: Vec<ArtifactInfo> = Vec::new();
    let mut raw_candidates: Vec<RawCandidate> = Vec::new();
    let mut all_tests: Vec<String> = Vec::new();
    let mut declined: Vec<DeclinedFunction> = Vec::new();
    // Pragmatic scalability guard: rustc's `--emit=mir` on a test
    // binary that pulls in deep generic chains can produce MIR
    // files hundreds of megabytes in size (bytes'
    // `test_bytes_vec_alloc` is 349 MB — 4.2 million lines).
    // Parsing those in Floyd's text parser blocks progress on the
    // whole crate for 10+ minutes. We skip them above a size
    // threshold and surface the skip in the honest-scope report,
    // so the user sees exactly which artifacts were elided and
    // why.
    let max_mir_bytes: u64 = max_mir_mb.unwrap_or(50) * 1024 * 1024;
    let mut skipped_oversize_mirs: Vec<(std::path::PathBuf, u64)> = Vec::new();
    let mut skipped_no_harness: Vec<NoHarnessTarget> = Vec::new();

    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] starting Pass 1 (parse MIR + enumerate tests)");
    }
    // Pass 1: parse MIR, enumerate tests, collect candidates per artifact.
    for artifact in build.test_artifacts.iter() {
        // `harness = false` targets are programs with their own
        // `main`; `--list` would run them. Decide before invoking.
        let uses_harness = artifact
            .manifest_path
            .as_deref()
            .and_then(|mp| {
                let text = std::fs::read_to_string(mp).ok()?;
                Some(harness::target_uses_harness(
                    &text,
                    mp.parent().unwrap_or(Path::new(".")),
                    &artifact.target_name,
                    &artifact.target_kinds,
                    &artifact.src_path,
                ))
            })
            .unwrap_or(true);
        if !uses_harness {
            skipped_no_harness.push(NoHarnessTarget {
                package: artifact.package_name.clone(),
                target: artifact.target_name.clone(),
                binary: artifact.binary_path.display().to_string(),
            });
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&artifact.mir_path) {
            if meta.len() > max_mir_bytes {
                skipped_oversize_mirs.push((artifact.mir_path.clone(), meta.len()));
                continue;
            }
        }
        let mir_text = match std::fs::read_to_string(&artifact.mir_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mir = match mir::parse_text(&mir_text) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let tests = match runner::list_tests(&artifact.binary_path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("cargo-floyd: {e}");
                return ExitCode::from(1);
            }
        };
        for t in &tests {
            all_tests.push(t.clone());
        }
        let art_idx = artifacts.len();
        for (fn_idx, f) in mir.functions.iter().enumerate() {
            // Require BOTH a successful decomposition AND the
            // presence of at least one `switchInt` in the function.
            // Without that `switchInt` gate, boolean-passthrough
            // helpers like `fn f(x: bool) -> bool { x }` register
            // as candidates — the decomposer produces a
            // `Node::Condition { name: "x" }`, but the function has
            // no actual decision in it (no branch), so llvm-cov
            // emits zero branch regions for it, correlation can't
            // produce an observation, and the candidate shows up
            // under the "reached-but-unobserved" bucket as a
            // false-positive correlation bug. It's not a bug —
            // there is no decision to measure in the first place.
            // These passthroughs are not out-of-scope either; they
            // are "no boolean decision at all", so they do not
            // belong in the OOS report.
            if decision::has_switchint(f) {
                if let Some(node) = decision::decompose_function(f) {
                    let prov = correlate::provenance_for(f);
                    // `--only=<pattern>` scopes Floyd to the
                    // functions the user named. On real embedded
                    // crates the whole-crate scan is both slow and
                    // noisy — a qualification engineer typically
                    // has a specific list of safety-critical gates
                    // they need to prove MC/DC on. If any --only
                    // pattern is present and this function's name
                    // doesn't contain any of them, drop it before
                    // scoring; it will not appear in the verdict
                    // or in the honest-scope report.
                    if !only_patterns.is_empty() {
                        let fn_name = prov.function_name.as_deref().unwrap_or(&f.name);
                        let hit = only_patterns
                            .iter()
                            .any(|pat| fn_name.contains(pat.as_str()));
                        if !hit {
                            continue;
                        }
                    }
                    // Reject candidates whose decision spans come
                    // from inlined-stdlib macro expansions.
                    // Rustc paths of the form `/rustc/<hash>/library/
                    // core/src/macros/mod.rs` are the hygiene
                    // bindings of `assert_eq!`, `debug_assert!`,
                    // `panic!`, and friends — not user-written
                    // decisions. On blake3 and other
                    // test-heavy crates, every `#[test] fn x() {
                    // assert_eq!(a, b); }` was being flagged as a
                    // candidate whose "conditions" `left_val` and
                    // `right_val` are the macro's own locals,
                    // producing reached-but-unobserved false
                    // positives in the AllUnobserved verdict.
                    if is_stdlib_macro_span(prov.source_file.as_deref()) {
                        continue;
                    }
                    raw_candidates.push(RawCandidate {
                        tree: floyd::DecisionTree {
                            decisions: vec![node],
                        },
                        prov,
                        artifact_idx: art_idx,
                        fn_idx,
                    });
                    continue;
                }
            }
            if decision::has_switchint(f) {
                // Has a switchInt (so some kind of decision) but
                // the decomposer did not match any supported
                // pattern. Honest-scope report entry.
                let prov = correlate::provenance_for(f);
                declined.push(DeclinedFunction {
                    name: prov.function_name.unwrap_or_default(),
                    source_file: prov.source_file,
                    line: prov.line,
                });
            }
        }
        artifacts.push(ArtifactInfo {
            mir,
            binary_path: artifact.binary_path.clone(),
            tests,
        });
    }

    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] Pass 1 complete: {} candidates, {} declined, {} artifacts, {} skipped-oversize",
        raw_candidates.len(), declined.len(), artifacts.len(), skipped_oversize_mirs.len());
    }
    // Pass 2: run every test across every artifact once; keep each
    // test's parsed coverage. We intentionally do NOT dedupe test
    // names across artifacts — two artifacts can share a test name.
    //
    // Parallelised across tests via rayon. Each per-test job is
    // self-contained (unique profraw file per `(art_idx, test_idx)`,
    // no shared state), and the real wall-clock work is in three
    // sequential subprocess calls per job (test invocation,
    // `llvm-profdata merge`, `llvm-cov export`). Serialising those
    // across every test in every artifact was the scalability
    // blocker surfaced on real crates: `bytes`, `uuid`, `blake3`,
    // `base64`, `chrono`, `url` all exceeded 10 minutes before
    // this pass. On N logical cores, rayon runs min(N, jobs) jobs
    // in parallel.
    #[derive(Clone)]
    struct TestJob {
        art_idx: usize,
        test_idx: usize,
        test_name: String,
        binary_path: std::path::PathBuf,
    }

    let jobs: Vec<TestJob> = artifacts
        .iter()
        .enumerate()
        .flat_map(|(art_idx, info)| {
            let bp = info.binary_path.clone();
            info.tests
                .iter()
                .enumerate()
                .map(move |(test_idx, test_name)| TestJob {
                    art_idx,
                    test_idx,
                    test_name: test_name.clone(),
                    binary_path: bp.clone(),
                })
                .collect::<Vec<_>>()
        })
        .collect();

    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!(
            "[progress] starting Pass 2: {} tests across {} artifacts",
            jobs.len(),
            artifacts.len()
        );
    }
    let progress_counter = std::sync::atomic::AtomicUsize::new(0);
    let progress_total = jobs.len();

    use rayon::prelude::*;
    let results: Result<Vec<(String, profile::CoverageReport)>, String> = jobs
        .par_iter()
        .map(|job| -> Result<(String, profile::CoverageReport), String> {
            let prof = workdir.join(format!(
                "cov-{:03}-{:03}.profraw",
                job.art_idx, job.test_idx
            ));
            runner::run_test_isolated(&job.binary_path, &job.test_name, &prof)
                .map_err(|e| e.to_string())?;
            let coverage = runner::profraw_to_coverage(
                &prof,
                &profdata_tool,
                &cov_tool,
                &job.binary_path,
                workdir,
            )
            .map_err(|e| e.to_string())?;
            let n = progress_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if (n.is_multiple_of(10) || n == progress_total) && std::env::var("FLOYD_DIAG").is_ok()
            {
                eprintln!("[progress]   {n}/{progress_total}");
            }
            Ok((job.test_name.clone(), coverage))
        })
        .collect();
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] Pass 2 complete");
    }

    let all_coverage = match results {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
    };

    // Pre-compute condition span maps per artifact's MIR. Done
    // once; reused across every (candidate, test) pair in Pass 3.
    // This turns Pass 3 from O(candidates × tests × functions ×
    // blocks) into O(functions × blocks) + O(candidates × tests).
    // On `bytes` (10 candidates, 1053 tests, ~500 functions per
    // artifact MIR), that is a factor-of-thousands speedup.
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] starting Pass 3 prep (precompute span maps)");
    }
    let prepared: Vec<correlate::PreparedMir<'_>> = artifacts
        .iter()
        .map(|a| correlate::prepare_mir(&a.mir))
        .collect();
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] Pass 3 prep complete");
    }

    // Pre-compute per-test coverage lookups (one `HashMap` per
    // test so Pass 3's `find_matching_function` scan becomes O(1)).
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] prepping per-test coverage lookups");
    }
    let prepared_covs: Vec<(String, correlate::PreparedCoverage<'_>)> = all_coverage
        .par_iter()
        .map(|(name, cov)| (name.clone(), correlate::prepare_coverage_lookup(cov)))
        .collect();
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] coverage lookups ready: {}", prepared_covs.len());
    }

    // Pass 3: for each candidate, correlate against every test's
    // coverage using the pre-computed span maps and coverage
    // lookups. Parallelised across candidates — each is
    // independent.
    let candidates: Vec<selector::Candidate> = raw_candidates
        .into_par_iter()
        .map(|raw| {
            let prep = &prepared[raw.artifact_idx];
            let obs: Vec<_> = prepared_covs
                .iter()
                .flat_map(|(test_name, prepared_cov)| {
                    correlate::observations_from_prepared_coverage(
                        Some(test_name.clone()),
                        prep,
                        raw.fn_idx,
                        prepared_cov,
                        &raw.tree,
                    )
                })
                .collect();
            selector::Candidate {
                tree: raw.tree,
                provenance: raw.prov,
                observations: obs,
            }
        })
        .collect();
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!(
            "[progress] Pass 3 complete: {} candidates",
            candidates.len()
        );
    }

    // Classify every candidate by whether its own function was
    // actually reached at runtime by any test. Lets us split the
    // AllUnobserved message into (a) candidates the test suite
    // genuinely never invoked and (b) candidates the suite DID
    // run through but Floyd's span-match correlation missed —
    // the latter is a Floyd bug, the former a user gap, and
    // conflating them hides evidence that matters to a
    // qualification audit.
    let candidate_reach: Vec<bool> = candidates
        .iter()
        .map(|c| {
            let fn_name = c.provenance.function_name.as_deref().unwrap_or("");
            prepared_covs
                .iter()
                .any(|(_, pcov)| pcov.find(fn_name).map(|f| f.count > 0).unwrap_or(false))
        })
        .collect();

    // FLOYD_DIAG span-mismatch probe: for every candidate that
    // WAS reached at runtime but produced zero observations,
    // dump its MIR condition spans alongside the first
    // reaching test's coverage branch regions. Lets a human
    // diff the two and see what offset / file-path shift the
    // span-equality join is tripping on.
    if std::env::var("FLOYD_DIAG").is_ok() {
        for (c, reached) in candidates.iter().zip(candidate_reach.iter()) {
            if !*reached || !c.observations.is_empty() {
                continue;
            }
            let fn_name = c.provenance.function_name.as_deref().unwrap_or("");
            eprintln!("[span-diag] candidate fn={fn_name}");
            // MIR condition spans — rebuild for this function via
            // the correlate helper (prepared.function_spans is
            // keyed by fn_idx, not reachable from this scope
            // cleanly; collecting the one function's spans here
            // is cheap).
            for (art_idx, info) in artifacts.iter().enumerate() {
                if let Some((fidx, _)) = info
                    .mir
                    .functions
                    .iter()
                    .enumerate()
                    .find(|(_, f)| f.name == fn_name)
                {
                    eprintln!("[span-diag]   artifact {art_idx} function_idx={fidx}");
                    let spans = &prepared[art_idx].function_spans[fidx];
                    for (name, span) in spans {
                        eprintln!("[span-diag]     MIR cond '{name}' @ {span:?}");
                    }
                    break;
                }
            }
            // Coverage branches from the first reaching test.
            for (test_name, pcov) in prepared_covs.iter() {
                if let Some(fc) = pcov.find(fn_name) {
                    if fc.count > 0 {
                        eprintln!(
                            "[span-diag]   test '{test_name}' has {} branches, count={}",
                            fc.branches.len(),
                            fc.count
                        );
                        for b in fc.branches.iter().take(10) {
                            eprintln!(
                                "[span-diag]     COV branch @ {:?} true={} false={}",
                                b.span, b.true_count, b.false_count
                            );
                        }
                        break;
                    }
                }
            }
        }
    }
    let reached_count = candidate_reach.iter().filter(|r| **r).count();
    let unreached_count = candidate_reach.len() - reached_count;
    let candidate_summaries: Vec<CandidateSummary> = candidates
        .iter()
        .zip(candidate_reach.iter())
        .map(|(c, reached)| CandidateSummary {
            function_name: c.provenance.function_name.clone(),
            source_file: c.provenance.source_file.clone(),
            line: c.provenance.line,
            invoked: *reached,
            observations: selector::observed_tests(c),
        })
        .collect();
    let skipped_mirs: Vec<SkippedMir> = skipped_oversize_mirs
        .iter()
        .map(|(path, bytes)| SkippedMir {
            path: path.display().to_string(),
            bytes: *bytes,
        })
        .collect();
    let manifest_str = manifest.display().to_string();
    let workspace_root_str = build.workspace_root.display().to_string();
    let test_targets = build.test_artifacts.len();
    let report = |verdict, observations, analysis| CargoRuntimeReport {
        manifest: manifest_str.clone(),
        workspace_root: workspace_root_str.clone(),
        test_targets,
        verdict,
        tests_discovered: &all_tests,
        observations,
        analysis,
        candidates: &candidate_summaries,
        out_of_scope: &declined,
        skipped_oversize_mirs: &skipped_mirs,
        skipped_no_harness: &skipped_no_harness,
    };
    // Decline paths: the human-readable explanation goes to stderr
    // in every format; stdout carries only the format's document.
    let emit_decline = |verdict| -> ExitCode {
        match format {
            Format::Text => print_out_of_scope_report(
                &mut std::io::stdout(),
                &declined,
                &skipped_oversize_mirs,
                &skipped_no_harness,
            ),
            Format::Json => {
                if let Err(code) = print_json(&report(verdict, &[], None)) {
                    return code;
                }
            }
            Format::Junit => print_out_of_scope_report(
                &mut std::io::stderr(),
                &declined,
                &skipped_oversize_mirs,
                &skipped_no_harness,
            ),
        }
        ExitCode::from(1)
    };

    let chosen = match selector::pick_best(candidates) {
        selector::PickResult::Picked(c) => c,
        selector::PickResult::AllUnobserved => {
            eprintln!(
                "cargo-floyd: Floyd found decomposable boolean \
                 decisions in your code, but no test produced a \
                 usable observation for any of them."
            );
            if reached_count > 0 {
                eprintln!(
                    "  {reached_count} candidate(s) WERE invoked at \
                     runtime, but Floyd's span-match could not join \
                     them to a coverage branch region. This is most \
                     likely a Floyd correlation bug (rustc span \
                     drift under optimisation); please file an \
                     issue with --format=json's provenance block."
                );
            }
            if unreached_count > 0 {
                eprintln!(
                    "  {unreached_count} candidate(s) were never \
                     invoked by any test — add a test that calls \
                     one of your top-level `pub fn` decisions, or \
                     verify which candidates were considered via \
                     --format=json's `candidates` list."
                );
            }
            return emit_decline(RuntimeVerdict::AllUnobserved);
        }
        selector::PickResult::NoCandidates => {
            eprintln!(
                "cargo-floyd: no recognised decisions in any test target. \
                 The engine currently supports `&&`, `||`, `!`, and nested \
                 combinations; `if let`, `?`, and `match` patterns are \
                 partially supported (2-arm const-returning shapes); \
                 non-boolean decisions and arms that end in a function \
                 call are out of scope."
            );
            return emit_decline(RuntimeVerdict::NoCandidates);
        }
    };
    let all_observations = chosen.observations.clone();
    let mut analysis =
        masking::analyze_with_runtime(&chosen.tree, &all_observations, Variant::Masking);
    analysis.provenance = Some(chosen.provenance);

    match format {
        Format::Text => {
            println!("floyd  runtime MC/DC analysis of {}", manifest.display());
            println!();
            println!("Test targets: {}", build.test_artifacts.len());
            print_runtime_report_body(&all_tests, &all_observations, &analysis);
            print_out_of_scope_report(
                &mut std::io::stdout(),
                &declined,
                &skipped_oversize_mirs,
                &skipped_no_harness,
            );
        }
        Format::Json => {
            if let Err(code) = print_json(&report(
                RuntimeVerdict::Analysed,
                &all_observations,
                Some(&analysis),
            )) {
                return code;
            }
        }
        Format::Junit => {
            print_runtime_junit(&manifest.display().to_string(), &analysis);
        }
    }
    ExitCode::SUCCESS
}

fn run_test_mode_single(source: &Path, workdir: &Path, format: Format) -> ExitCode {
    // 1. Build the file as a #[test]-enabled instrumented binary.
    let compiled = match instrument::compile_test_target(source, workdir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
    };

    // 2. Locate llvm-tools we need to ingest coverage per test.
    let profdata_tool = match runner::llvm_tool("llvm-profdata") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
    };
    let cov_tool = match runner::llvm_tool("llvm-cov") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
    };

    // 3. Parse MIR once + decompose.
    let mir_text = match std::fs::read_to_string(&compiled.mir_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cargo-floyd: read MIR: {e}");
            return ExitCode::from(1);
        }
    };
    let mir = match mir::parse_text(&mir_text) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cargo-floyd: parse MIR: {e}");
            return ExitCode::from(1);
        }
    };
    // 4. Enumerate the binary's tests.
    let tests = match runner::list_tests(&compiled.binary_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
    };
    if tests.is_empty() {
        eprintln!(
            "cargo-floyd: no #[test] functions found in {}",
            source.display()
        );
        return ExitCode::from(1);
    }

    // 4b. Collect per-function candidates (smarter selector) and
    //     pre-compute coverage per test — same shape as the cargo-mode path.
    let mut artifact_candidates: Vec<(floyd::DecisionTree, floyd::DecisionProvenance)> = Vec::new();
    for f in &mir.functions {
        if let Some(node) = decision::decompose_function(f) {
            artifact_candidates.push((
                floyd::DecisionTree {
                    decisions: vec![node],
                },
                correlate::provenance_for(f),
            ));
        }
    }

    // 5. Run each test individually.
    let mut per_test_coverage: Vec<(String, profile::CoverageReport)> =
        Vec::with_capacity(tests.len());
    for (idx, test_name) in tests.iter().enumerate() {
        let prof = workdir.join(format!("cov-{idx:03}.profraw"));
        if let Err(e) = runner::run_test_isolated(&compiled.binary_path, test_name, &prof) {
            eprintln!("cargo-floyd: {e}");
            return ExitCode::from(1);
        }
        let coverage = match runner::profraw_to_coverage(
            &prof,
            &profdata_tool,
            &cov_tool,
            &compiled.binary_path,
            workdir,
        ) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cargo-floyd: {e}");
                return ExitCode::from(1);
            }
        };
        per_test_coverage.push((test_name.clone(), coverage));
    }

    // Build candidates with per-tree observations.
    let mut candidates: Vec<selector::Candidate> = Vec::new();
    for (tree, prov) in artifact_candidates {
        let mut obs = Vec::new();
        for (test_name, coverage) in &per_test_coverage {
            obs.extend(correlate::observations_from_coverage(
                Some(test_name.clone()),
                &mir,
                coverage,
                &tree,
            ));
        }
        candidates.push(selector::Candidate {
            tree,
            provenance: prov,
            observations: obs,
        });
    }

    // 6. Pick the best candidate, run the analysis.
    let report = |verdict, observations, analysis| SingleRuntimeReport {
        source: source.display().to_string(),
        verdict,
        tests_discovered: &tests,
        observations,
        analysis,
    };
    let emit_decline = |verdict| -> ExitCode {
        if let Format::Json = format {
            if let Err(code) = print_json(&report(verdict, &[], None)) {
                return code;
            }
        }
        ExitCode::from(1)
    };
    let chosen = match selector::pick_best(candidates) {
        selector::PickResult::Picked(c) => c,
        selector::PickResult::AllUnobserved => {
            eprintln!(
                "cargo-floyd: Floyd found decomposable boolean decisions in \
                 your code, but no test exercised any of them. \
                 The decisions it recognised live in closures \
                 or test-adjacent helpers that the test suite \
                 does not call directly. Add a test that calls \
                 one of your top-level `pub fn` decisions, or \
                 verify which candidates Floyd considered via \
                 `--format=json`'s provenance block."
            );
            return emit_decline(RuntimeVerdict::AllUnobserved);
        }
        selector::PickResult::NoCandidates => {
            eprintln!(
                "cargo-floyd: no recognised decisions in {}",
                source.display()
            );
            return emit_decline(RuntimeVerdict::NoCandidates);
        }
    };
    let observations = chosen.observations.clone();
    let mut analysis = masking::analyze_with_runtime(&chosen.tree, &observations, Variant::Masking);
    analysis.provenance = Some(chosen.provenance);

    match format {
        Format::Text => print_runtime_report(source, &tests, &observations, &analysis),
        Format::Json => {
            if let Err(code) = print_json(&report(
                RuntimeVerdict::Analysed,
                &observations,
                Some(&analysis),
            )) {
                return code;
            }
        }
        Format::Junit => {
            print_runtime_junit(&source.display().to_string(), &analysis);
        }
    }
    ExitCode::SUCCESS
}

fn parse_format(s: &str) -> Result<Format, String> {
    match s {
        "text" => Ok(Format::Text),
        "json" => Ok(Format::Json),
        "junit" => Ok(Format::Junit),
        other => Err(format!(
            "unknown --format value: {other} (expected text, json, or junit)"
        )),
    }
}

fn print_usage() {
    eprintln!("cargo-floyd  MC/DC coverage analysis for Rust");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("    cargo floyd test [--format=text|json|junit] [<path>]");
    eprintln!("        Runtime MC/DC analysis: builds the project as a test");
    eprintln!("        crate with coverage instrumentation, runs each #[test]");
    eprintln!("        function individually, and reports which conditions the");
    eprintln!("        test suite exercises under masking MC/DC.");
    eprintln!();
    eprintln!("        <path> defaults to the current directory (cargo project).");
    eprintln!("        Pass an explicit Cargo.toml, a directory, or a single .rs");
    eprintln!("        file to override.");
    eprintln!();
    eprintln!("        --format=junit emits a JUnit XML report (one testcase");
    eprintln!("        per condition; pass=exercised, failure=unexercised) for");
    eprintln!("        CIs that render JUnit natively (Jenkins, GitLab CI,");
    eprintln!("        GitHub Actions, Bazel/Buck2, etc.).");
    eprintln!();
    eprintln!("    cargo floyd [--format=text|json] <path/to/source.rs>");
    eprintln!("        Static MC/DC analysis: prints the recovered truth table");
    eprintln!("        and per-condition independence pairs for one source file.");
    eprintln!("        --format=junit is not valid in static mode (no test");
    eprintln!("        verdicts to render).");
    eprintln!();
    eprintln!("Requires nightly Rust + the llvm-tools-preview component:");
    eprintln!("    rustup component add llvm-tools-preview --toolchain nightly");
}

// ---------------------------------------------------------------------------
// Text report rendering
// ---------------------------------------------------------------------------

fn print_static_report(
    source: &Path,
    tree: &floyd::DecisionTree,
    matrix: &floyd::IndependenceMatrix,
) {
    println!("floyd  static MC/DC analysis of {}", source.display());
    println!();
    println!("Decisions recovered: {}", tree.decisions.len());
    if !matrix.conditions.is_empty() {
        println!("Conditions:          {}", matrix.conditions.join(", "));
    }
    println!();

    println!("Truth table ({} rows):", matrix.truth_table.len());
    print_truth_table_header(&matrix.conditions);
    for row in &matrix.truth_table {
        print_truth_table_row(&matrix.conditions, row);
    }
    println!();

    println!("Independence pairs ({:?} variant):", matrix.variant);
    for cond in &matrix.conditions {
        if let Some(pairs) = matrix.independence_pairs.get(cond) {
            if let Some(p) = pairs.first() {
                print_pair(cond, &matrix.conditions, p);
            } else {
                println!("  {cond}: no valid independence pair found");
            }
        }
    }
    println!();

    let min_set: std::collections::BTreeSet<&_> = matrix
        .independence_pairs
        .values()
        .flat_map(|pairs| pairs.first())
        .flat_map(|p| vec![&p.test_1.inputs, &p.test_2.inputs])
        .collect();
    println!(
        "Minimum test set (one valid choice): {} test(s)",
        min_set.len()
    );
}

fn print_runtime_report(
    source: &Path,
    tests: &[String],
    observations: &[ConditionObservation],
    analysis: &RuntimeAnalysis,
) {
    println!("floyd  runtime MC/DC analysis of {}", source.display());
    println!();
    print_runtime_report_body(tests, observations, analysis);
}

fn print_runtime_report_body(
    tests: &[String],
    observations: &[ConditionObservation],
    analysis: &RuntimeAnalysis,
) {
    let ambiguous = observations.iter().filter(|o| o.ambiguous).count();
    println!("Tests discovered: {}", tests.len());
    println!(
        "Observations:     {} ({} used, {ambiguous} ambiguous)",
        observations.len(),
        observations.len() - ambiguous,
    );
    println!(
        "Conditions:       {}",
        if analysis.matrix.conditions.is_empty() {
            "(none — no decision recovered)".to_string()
        } else {
            analysis.matrix.conditions.join(", ")
        }
    );
    println!();

    // Issue #14: 0 observations against a non-empty decision tree is
    // misleading. Users read "UNEXERCISED" as "my suite is weak" when
    // the actual cause is Floyd analysed a function the suite does
    // not touch at all. Warn clearly.
    if observations.is_empty() && !analysis.matrix.conditions.is_empty() {
        let func = analysis
            .provenance
            .as_ref()
            .and_then(|p| p.function_name.as_deref())
            .unwrap_or("<unknown>");
        println!("WARNING: the analysed decision in `{func}` was not");
        println!("         exercised by any observed test.  Floyd may have selected a");
        println!("         function the test suite does not call; the 'UNEXERCISED'");
        println!("         status below reflects that, not a weak test suite.  Consider");
        println!("         adding a #[test] that calls `{func}`, or verify which");
        println!("         function Floyd analysed via --format=json's provenance block.");
        println!();
    }

    if !observations.is_empty() {
        println!("Per-test observations:");
        for obs in observations {
            let label = obs.test_name.as_deref().unwrap_or("?");
            if obs.ambiguous {
                println!(
                    "  {label:30}  AMBIGUOUS  {}",
                    format_ambiguous_counts(&analysis.matrix.conditions, obs)
                );
                continue;
            }
            let mut parts: Vec<String> = analysis
                .matrix
                .conditions
                .iter()
                .map(|c| match obs.inputs.get(c) {
                    Some(true) => format!("{c}=T"),
                    Some(false) => format!("{c}=F"),
                    None => format!("{c}=-"),
                })
                .collect();
            parts.push(format!("result={}", obs.result.map_or("?", bool_glyph)));
            if let Some(n) = obs.evaluations.filter(|n| *n > 1) {
                parts.push(format!("({n} evaluations)"));
            }
            println!("  {label:30}  {}", parts.join("  "));
        }
        println!();
    }

    println!("Per-condition MC/DC status:");
    let mut exercised = 0usize;
    for cond in &analysis.matrix.conditions {
        match analysis.condition_status.get(cond) {
            Some(ConditionStatus::Exercised(pair)) => {
                exercised += 1;
                let t1 = format_inputs(&analysis.matrix.conditions, &pair.test_1);
                let t2 = format_inputs(&analysis.matrix.conditions, &pair.test_2);
                println!(
                    "  ✓ {cond}: EXERCISED  via ({t1}) -> {}   vs   ({t2}) -> {}",
                    bool_glyph(pair.test_1.result),
                    bool_glyph(pair.test_2.result),
                );
            }
            Some(ConditionStatus::Ambiguous { tests }) => {
                println!(
                    "  ? {cond}: unknown (ambiguous) — evaluated only in tests that \
                     evaluated the decision more than once: {}",
                    format_ambiguous_tests(tests, observations)
                );
            }
            Some(ConditionStatus::Unexercised) => {
                let missing = analysis
                    .matrix
                    .independence_pairs
                    .get(cond)
                    .and_then(|pairs| pairs.first());
                if let Some(p) = missing {
                    let t1 = format_inputs(&analysis.matrix.conditions, &p.test_1);
                    let t2 = format_inputs(&analysis.matrix.conditions, &p.test_2);
                    println!("  ✗ {cond}: UNEXERCISED — needs a test exercising ({t1}) or ({t2})",);
                } else {
                    println!("  ✗ {cond}: UNEXERCISED (no valid pair available)");
                }
            }
            None => {
                println!("  ? {cond}: unknown");
            }
        }
    }
    let total = analysis.matrix.conditions.len();
    if total > 0 {
        let pct = (exercised as f64 / total as f64) * 100.0;
        println!();
        if analysis.ambiguous_observations == 0 {
            println!("MC/DC coverage: {exercised} of {total} conditions exercised ({pct:.0}%)",);
        } else {
            let unknown = analysis
                .condition_status
                .values()
                .filter(|s| matches!(s, ConditionStatus::Ambiguous { .. }))
                .count();
            println!(
                "MC/DC coverage: {exercised} of {total} conditions exercised ({pct:.0}%; \
                 {unknown} unknown, {} ambiguous observation(s) excluded; add \
                 single-evaluation tests or see the README section \
                 \"Tests that evaluate a decision more than once\")",
                analysis.ambiguous_observations
            );
        }
    }
}

/// `evaluated 256x: c1=224T/32F  c2=95T/129F` for an ambiguous
/// observation, in the decision's condition order.
fn format_ambiguous_counts(conditions: &[String], obs: &ConditionObservation) -> String {
    let counts: Vec<String> = conditions
        .iter()
        .map(|c| match obs.counts.get(c) {
            Some(n) => format!("{c}={}T/{}F", n.true_count, n.false_count),
            None => format!("{c}=-"),
        })
        .collect();
    let evals = obs
        .evaluations
        .map_or_else(|| "?".to_string(), |n| n.to_string());
    format!("evaluated {evals}x: {}", counts.join("  "))
}

/// How many ambiguous tests a condition's status line names before
/// summarising the rest (all of them are in the per-test listing).
const AMBIGUOUS_TESTS_SHOWN: usize = 3;

/// `t1 (256 evaluations), t2 (3 evaluations)` for the tests behind a
/// [`ConditionStatus::Ambiguous`], naming at most
/// [`AMBIGUOUS_TESTS_SHOWN`] of them.
fn format_ambiguous_tests(tests: &[String], observations: &[ConditionObservation]) -> String {
    let mut shown: Vec<String> = tests
        .iter()
        .take(AMBIGUOUS_TESTS_SHOWN)
        .map(|t| {
            let evals = observations
                .iter()
                .find(|o| o.ambiguous && o.test_name.as_deref() == Some(t.as_str()))
                .and_then(|o| o.evaluations);
            match evals {
                Some(n) => format!("{t} ({n} evaluations)"),
                None => t.clone(),
            }
        })
        .collect();
    if tests.len() > AMBIGUOUS_TESTS_SHOWN {
        shown.push(format!(
            "and {} more (see per-test observations)",
            tests.len() - AMBIGUOUS_TESTS_SHOWN
        ));
    }
    shown.join(", ")
}

fn print_truth_table_header(conditions: &[String]) {
    let header: Vec<String> = conditions.iter().map(|c| format!("{c:>3}")).collect();
    println!("    {}  | result", header.join(" "));
    let sep_line: Vec<String> = conditions.iter().map(|_| "---".to_string()).collect();
    println!("    {}--+-------", sep_line.join("-"));
}

fn print_truth_table_row(conditions: &[String], row: &TruthTableRow) {
    let cells: Vec<String> = conditions
        .iter()
        .map(|c| format!("{:>3}", bool_glyph(row.inputs[c])))
        .collect();
    println!("    {}  |   {}", cells.join(" "), bool_glyph(row.result));
}

fn print_pair(cond: &str, conditions: &[String], pair: &IndependencePair) {
    let t1 = format_inputs(conditions, &pair.test_1);
    let t2 = format_inputs(conditions, &pair.test_2);
    println!(
        "  {cond}: ({t1}) -> {}   vs   ({t2}) -> {}",
        bool_glyph(pair.test_1.result),
        bool_glyph(pair.test_2.result),
    );
}

fn format_inputs(conditions: &[String], row: &TruthTableRow) -> String {
    conditions
        .iter()
        .map(|c| format!("{c}={}", bool_glyph(row.inputs[c])))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Does this source-file path belong to the standard-library
/// macro expansion namespace that rustc stamps at compile time?
///
/// The canonical shape is `/rustc/<hash>/library/<crate>/src/...`
/// where `<crate>` is `core`, `std`, `alloc`, `proc_macro`, or a
/// test-harness crate. Also catches cargo-registry proc-macro
/// expansions whose spans point back to the macro definition
/// site rather than the user call site. Candidates whose
/// decision lives in those paths are macro hygiene bindings
/// (`left_val` / `right_val` from `assert_eq!`, etc.), not
/// user-written decisions, and should not be surfaced as MC/DC
/// candidates.
fn is_stdlib_macro_span(file: Option<&str>) -> bool {
    let Some(f) = file else {
        return false;
    };
    f.starts_with("/rustc/")
        || f.contains("/library/core/")
        || f.contains("/library/std/")
        || f.contains("/library/alloc/")
}

fn print_out_of_scope_report(
    out: &mut dyn std::io::Write,
    declined: &[impl std::fmt::Debug],
    skipped_oversize_mirs: &[(std::path::PathBuf, u64)],
    skipped_no_harness: &[NoHarnessTarget],
) {
    if !skipped_no_harness.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Skipped {} test target(s) built with `harness = false` \
             (custom `main`, no libtest `--list`); Floyd did not run them:",
            skipped_no_harness.len()
        );
        for t in skipped_no_harness {
            let _ = writeln!(out, "  {} ({})", t.target, t.package);
        }
    }
    if !declined.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Out of scope: {} function(s) have a decision in a shape \
             the engine's current patterns do not recognise. The engine \
             declined them rather than report a wrong verdict. See \
             --format=json's `out_of_scope` for the full list.",
            declined.len()
        );
    }
    if !skipped_oversize_mirs.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Skipped {} MIR file(s) above the 50 MB scalability \
             threshold (monomorphisation explosion in test \
             binaries). These artifacts were not analysed — \
             candidates and tests they would have contributed are \
             absent from the verdict:",
            skipped_oversize_mirs.len()
        );
        for (path, size) in skipped_oversize_mirs.iter().take(5) {
            let _ = writeln!(
                out,
                "  {}  ({:.0} MB)",
                path.display(),
                (*size as f64) / (1024.0 * 1024.0)
            );
        }
        if skipped_oversize_mirs.len() > 5 {
            let _ = writeln!(out, "  ... and {} more", skipped_oversize_mirs.len() - 5);
        }
    }
}

fn bool_glyph(v: bool) -> &'static str {
    if v {
        "T"
    } else {
        "F"
    }
}

// ---------------------------------------------------------------------------
// JUnit XML rendering
// ---------------------------------------------------------------------------

/// Emit a JUnit XML report for a runtime analysis. One
/// `<testsuite>` per analysis context (typically one per
/// invocation today), one `<testcase>` per recovered condition.
/// Exercised conditions pass (empty body); unexercised conditions
/// emit a `<failure type="UnexercisedCondition">` with the required
/// independence-pair inputs in the message; conditions whose only
/// evidence is ambiguous emit a `<failure type="AmbiguousCondition">`
/// naming the multi-evaluation tests (not demonstrated, so not a
/// pass).
///
/// The format is the de-facto JUnit XML that Jenkins, GitLab CI,
/// GitHub Actions, Bazel/Buck2, and the other CIs the automotive
/// industry already deploys render natively.
fn print_runtime_junit(source: &str, analysis: &RuntimeAnalysis) {
    let conditions = &analysis.matrix.conditions;
    let total = conditions.len();
    let failures = conditions
        .iter()
        .filter(|c| {
            !matches!(
                analysis.condition_status.get(c.as_str()),
                Some(ConditionStatus::Exercised(_))
            )
        })
        .count();

    println!(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    println!(r#"<testsuites name="floyd" tests="{total}" failures="{failures}">"#);
    println!(
        r#"  <testsuite name="{}" tests="{total}" failures="{failures}">"#,
        xml_escape(source)
    );
    for cond in conditions {
        let case_name = xml_escape(cond);
        match analysis.condition_status.get(cond.as_str()) {
            Some(ConditionStatus::Exercised(_)) => {
                println!(r#"    <testcase name="{case_name}" classname="floyd.mcdc"/>"#);
            }
            Some(ConditionStatus::Ambiguous { tests }) => {
                let message = format!(
                    "unknown (ambiguous): evaluated only in tests that evaluated the \
                     decision more than once ({}); aggregate counters cannot attribute \
                     a condition vector to them. Add single-evaluation tests.",
                    tests.join(", ")
                );
                println!(r#"    <testcase name="{case_name}" classname="floyd.mcdc">"#);
                println!(
                    r#"      <failure message="{}" type="AmbiguousCondition"/>"#,
                    xml_escape(&message)
                );
                println!(r#"    </testcase>"#);
            }
            Some(ConditionStatus::Unexercised) | None => {
                let required = analysis
                    .matrix
                    .independence_pairs
                    .get(cond.as_str())
                    .and_then(|pairs| pairs.first())
                    .map(|p| {
                        let t1 = format_inputs(conditions, &p.test_1);
                        let t2 = format_inputs(conditions, &p.test_2);
                        format!(
                            "needs a test exercising ({t1}) -> {} or ({t2}) -> {}",
                            bool_glyph(p.test_1.result),
                            bool_glyph(p.test_2.result),
                        )
                    })
                    .unwrap_or_else(|| {
                        "no valid independence pair available for this condition".to_string()
                    });
                println!(r#"    <testcase name="{case_name}" classname="floyd.mcdc">"#);
                println!(
                    r#"      <failure message="{}" type="UnexercisedCondition"/>"#,
                    xml_escape(&required)
                );
                println!(r#"    </testcase>"#);
            }
        }
    }
    println!(r#"  </testsuite>"#);
    println!(r#"</testsuites>"#);
}

/// Escape the five XML special characters so condition names like
/// `speed > 50` survive intact in attribute and text positions.
/// The `&` replacement runs first so the subsequent escapes don't
/// double-escape the ampersands they introduce.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_handles_all_five_specials() {
        assert_eq!(xml_escape("a < b"), "a &lt; b");
        assert_eq!(xml_escape("a > b"), "a &gt; b");
        assert_eq!(xml_escape(r#"q="x""#), "q=&quot;x&quot;");
        assert_eq!(xml_escape("don't"), "don&apos;t");
        // Ampersand must run first so `&lt;` doesn't become `&amp;lt;`.
        assert_eq!(xml_escape("a < b & c"), "a &lt; b &amp; c");
    }

    #[test]
    fn decline_json_report_carries_out_of_scope_and_candidates() {
        let declined = vec![DeclinedFunction {
            name: "crate::f".to_string(),
            source_file: Some("src/lib.rs".to_string()),
            line: Some(7),
        }];
        let candidates = vec![CandidateSummary {
            function_name: Some("crate::g".to_string()),
            source_file: Some("src/lib.rs".to_string()),
            line: Some(12),
            invoked: false,
            observations: 0,
        }];
        let skipped = vec![SkippedMir {
            path: "target/x.mir".to_string(),
            bytes: 1,
        }];
        let report = CargoRuntimeReport {
            manifest: "Cargo.toml".to_string(),
            workspace_root: ".".to_string(),
            test_targets: 1,
            verdict: RuntimeVerdict::AllUnobserved,
            tests_discovered: &[],
            observations: &[],
            analysis: None,
            candidates: &candidates,
            out_of_scope: &declined,
            skipped_oversize_mirs: &skipped,
            skipped_no_harness: &[],
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&report).unwrap()).unwrap();
        assert_eq!(v["verdict"], "all_unobserved");
        assert!(v["analysis"].is_null());
        assert_eq!(v["observations"].as_array().unwrap().len(), 0);
        assert_eq!(v["out_of_scope"][0]["name"], "crate::f");
        assert_eq!(v["out_of_scope"][0]["line"], 7);
        assert_eq!(v["candidates"][0]["function_name"], "crate::g");
        assert_eq!(v["candidates"][0]["invoked"], false);
        assert_eq!(v["skipped_oversize_mirs"][0]["bytes"], 1);
    }

    #[test]
    fn json_observations_carry_ambiguity_evaluations_and_raw_counts() {
        use floyd::BranchCounts;
        let mut vector = ConditionObservation::vector(
            Some("detects_unprintable".to_string()),
            [("byte >= 32".to_string(), false)].into_iter().collect(),
            false,
        );
        vector.evaluations = Some(1);
        let ambiguous = ConditionObservation::ambiguous(
            Some("symbol_matches_char_for_all_valid_symbols".to_string()),
            256,
            [
                (
                    "byte >= 32".to_string(),
                    BranchCounts {
                        true_count: 224,
                        false_count: 32,
                    },
                ),
                (
                    "byte <= 126".to_string(),
                    BranchCounts {
                        true_count: 95,
                        false_count: 129,
                    },
                ),
            ]
            .into_iter()
            .collect(),
        );
        let observations = [vector, ambiguous];
        let report = SingleRuntimeReport {
            source: "lib.rs".to_string(),
            verdict: RuntimeVerdict::Analysed,
            tests_discovered: &[],
            observations: &observations,
            analysis: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        let o = &v["observations"];
        assert_eq!(o[0]["ambiguous"], false);
        assert_eq!(o[0]["evaluations"], 1);
        assert_eq!(o[0]["result"], false);
        assert!(o[0].get("counts").is_none());
        assert_eq!(o[1]["ambiguous"], true);
        assert_eq!(o[1]["evaluations"], 256);
        assert!(o[1]["result"].is_null());
        assert_eq!(o[1]["counts"]["byte >= 32"]["true_count"], 224);
        assert_eq!(o[1]["counts"]["byte >= 32"]["false_count"], 32);
        assert_eq!(o[1]["counts"]["byte <= 126"]["true_count"], 95);
        assert_eq!(o[1]["counts"]["byte <= 126"]["false_count"], 129);

        let conditions = ["byte <= 126".to_string(), "byte >= 32".to_string()];
        assert_eq!(
            format_ambiguous_counts(&conditions, &observations[1]),
            "evaluated 256x: byte <= 126=95T/129F  byte >= 32=224T/32F"
        );
        assert_eq!(
            format_ambiguous_tests(
                &["symbol_matches_char_for_all_valid_symbols".to_string()],
                &observations
            ),
            "symbol_matches_char_for_all_valid_symbols (256 evaluations)"
        );
        let many: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        assert_eq!(
            format_ambiguous_tests(&many, &[]),
            "t0, t1, t2, and 2 more (see per-test observations)"
        );
    }

    #[test]
    fn parse_format_accepts_three_values() {
        assert!(matches!(parse_format("text"), Ok(Format::Text)));
        assert!(matches!(parse_format("json"), Ok(Format::Json)));
        assert!(matches!(parse_format("junit"), Ok(Format::Junit)));
        assert!(parse_format("xml").is_err());
        assert!(parse_format("").is_err());
    }
}
