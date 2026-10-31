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

mod selector;
use std::path::Path;
use std::process::ExitCode;

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

    // Pull `--format=<fmt>` (or `--format <fmt>`) out of the args.
    let mut format = Format::Text;
    let mut i = 0;
    while i < args.len() {
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
        (true, Source::Cargo(p)) => run_test_mode_cargo(&p, &workdir, format),
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

fn run_test_mode_cargo(manifest: &Path, workdir: &Path, format: Format) -> ExitCode {
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

    #[derive(serde::Serialize, Clone, Debug)]
    struct DeclinedFunction {
        name: String,
        source_file: Option<String>,
        line: Option<u32>,
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
    const MAX_MIR_BYTES: u64 = 50 * 1024 * 1024; // 50 MB
    let mut skipped_oversize_mirs: Vec<(std::path::PathBuf, u64)> = Vec::new();

    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] starting Pass 1 (parse MIR + enumerate tests)");
    }
    // Pass 1: parse MIR, enumerate tests, collect candidates per artifact.
    for artifact in build.test_artifacts.iter() {
        if let Ok(meta) = std::fs::metadata(&artifact.mir_path) {
            if meta.len() > MAX_MIR_BYTES {
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
            if let Some(node) = decision::decompose_function(f) {
                raw_candidates.push(RawCandidate {
                    tree: floyd::DecisionTree {
                        decisions: vec![node],
                    },
                    prov: correlate::provenance_for(f),
                    artifact_idx: art_idx,
                    fn_idx,
                });
            } else if decision::has_switchint(f) {
                // The function has at least one switchInt — i.e. a
                // decision in the source — but the decomposer did
                // not match any supported pattern. Record it for
                // the honest-scope report so a reader can see what
                // the engine declined, instead of silently ignoring
                // it. Pure straight-line functions and pure Call
                // delegations fall out here (no switchInt), so
                // they do not pollute the list.
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
                .filter_map(|(test_name, prepared_cov)| {
                    correlate::observation_from_prepared_coverage(
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
            print_out_of_scope_report(&declined, &skipped_oversize_mirs);
            return ExitCode::from(1);
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
            print_out_of_scope_report(&declined, &skipped_oversize_mirs);
            return ExitCode::from(1);
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
            print_out_of_scope_report(&declined, &skipped_oversize_mirs);
        }
        Format::Json => {
            #[derive(serde::Serialize)]
            struct Report<'a> {
                manifest: String,
                workspace_root: String,
                test_targets: usize,
                tests_discovered: &'a [String],
                observations: &'a [ConditionObservation],
                analysis: &'a RuntimeAnalysis,
                /// Functions with a `switchInt`-shaped decision that
                /// the engine's current patterns did not match. The
                /// engine declined them rather than report a wrong
                /// verdict — honest-scope evidence for ISO 26262 /
                /// DO-178C qualification audits.
                out_of_scope: &'a [DeclinedFunction],
            }
            let report = Report {
                manifest: manifest.display().to_string(),
                workspace_root: build.workspace_root.display().to_string(),
                test_targets: build.test_artifacts.len(),
                tests_discovered: &all_tests,
                observations: &all_observations,
                analysis: &analysis,
                out_of_scope: &declined,
            };
            match serde_json::to_string_pretty(&report) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("cargo-floyd: serialize JSON: {e}");
                    return ExitCode::from(1);
                }
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
            if let Some(o) =
                correlate::observation_from_coverage(Some(test_name.clone()), &mir, coverage, &tree)
            {
                obs.push(o);
            }
        }
        candidates.push(selector::Candidate {
            tree,
            provenance: prov,
            observations: obs,
        });
    }

    // 6. Pick the best candidate, run the analysis.
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
            return ExitCode::from(1);
        }
        selector::PickResult::NoCandidates => {
            eprintln!(
                "cargo-floyd: no recognised decisions in {}",
                source.display()
            );
            return ExitCode::from(1);
        }
    };
    let observations = chosen.observations.clone();
    let mut analysis = masking::analyze_with_runtime(&chosen.tree, &observations, Variant::Masking);
    analysis.provenance = Some(chosen.provenance);

    match format {
        Format::Text => print_runtime_report(source, &tests, &observations, &analysis),
        Format::Json => {
            #[derive(serde::Serialize)]
            struct Report<'a> {
                source: String,
                tests_discovered: &'a [String],
                observations: &'a [ConditionObservation],
                analysis: &'a RuntimeAnalysis,
            }
            let report = Report {
                source: source.display().to_string(),
                tests_discovered: &tests,
                observations: &observations,
                analysis: &analysis,
            };
            match serde_json::to_string_pretty(&report) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("cargo-floyd: serialize JSON: {e}");
                    return ExitCode::from(1);
                }
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
    println!("Tests discovered: {}", tests.len());
    println!("Observations:     {}", observations.len());
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
            parts.push(format!("result={}", bool_glyph(obs.result)));
            let label = obs.test_name.as_deref().unwrap_or("?");
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
        println!("MC/DC coverage: {exercised} of {total} conditions exercised ({pct:.0}%)",);
    }
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

fn print_out_of_scope_report(
    declined: &[impl std::fmt::Debug],
    skipped_oversize_mirs: &[(std::path::PathBuf, u64)],
) {
    if !declined.is_empty() {
        println!();
        println!(
            "Out of scope: {} function(s) have a decision in a shape \
             the engine's current patterns do not recognise. The engine \
             declined them rather than report a wrong verdict. See \
             --format=json's `out_of_scope` for the full list.",
            declined.len()
        );
    }
    if !skipped_oversize_mirs.is_empty() {
        println!();
        println!(
            "Skipped {} MIR file(s) above the 50 MB scalability \
             threshold (monomorphisation explosion in test \
             binaries). These artifacts were not analysed — \
             candidates and tests they would have contributed are \
             absent from the verdict:",
            skipped_oversize_mirs.len()
        );
        for (path, size) in skipped_oversize_mirs.iter().take(5) {
            println!(
                "  {}  ({:.0} MB)",
                path.display(),
                (*size as f64) / (1024.0 * 1024.0)
            );
        }
        if skipped_oversize_mirs.len() > 5 {
            println!("  ... and {} more", skipped_oversize_mirs.len() - 5);
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
/// emit a `<failure>` with the required independence-pair inputs
/// in the message.
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
            matches!(
                analysis.condition_status.get(c.as_str()),
                Some(ConditionStatus::Unexercised) | None
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
    fn parse_format_accepts_three_values() {
        assert!(matches!(parse_format("text"), Ok(Format::Text)));
        assert!(matches!(parse_format("json"), Ok(Format::Json)));
        assert!(matches!(parse_format("junit"), Ok(Format::Junit)));
        assert!(parse_format("xml").is_err());
        assert!(parse_format("").is_err());
    }
}
