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
use floyd::runner;
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

    let mut all_tests = Vec::new();
    let mut all_observations = Vec::new();
    let mut chosen_tree: Option<floyd::DecisionTree> = None;

    for (art_idx, artifact) in build.test_artifacts.iter().enumerate() {
        let mir_text = match std::fs::read_to_string(&artifact.mir_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mir = match mir::parse_text(&mir_text) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let tree = decision::decompose(&mir);
        if tree.decisions.is_empty() {
            continue;
        }
        // Phase 1 first cut: take the first non-empty decision tree as
        // THE decision under analysis. Multi-decision aggregation lands
        // in a future release.
        if chosen_tree.is_none() {
            chosen_tree = Some(tree.clone());
        }

        let tests = match runner::list_tests(&artifact.binary_path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("cargo-floyd: {e}");
                return ExitCode::from(1);
            }
        };

        for (test_idx, test_name) in tests.iter().enumerate() {
            let prof = workdir.join(format!("cov-{art_idx:03}-{test_idx:03}.profraw"));
            if let Err(e) = runner::run_test_isolated(&artifact.binary_path, test_name, &prof) {
                eprintln!("cargo-floyd: {e}");
                return ExitCode::from(1);
            }
            let coverage = match runner::profraw_to_coverage(
                &prof,
                &profdata_tool,
                &cov_tool,
                &artifact.binary_path,
                workdir,
            ) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("cargo-floyd: {e}");
                    return ExitCode::from(1);
                }
            };
            if let Some(obs) = correlate::observation_from_coverage(
                Some(test_name.clone()),
                &mir,
                &coverage,
                &tree,
            ) {
                all_observations.push(obs);
            }
            all_tests.push(test_name.clone());
        }
    }

    let tree = match chosen_tree {
        Some(t) => t,
        None => {
            eprintln!(
                "cargo-floyd: no recognised decisions in any test target. \
                 The engine currently supports `&&`, `||`, `!`, and nested \
                 combinations; `if let`, `?`, and `match` patterns are not \
                 yet implemented."
            );
            return ExitCode::from(1);
        }
    };
    let analysis = masking::analyze_with_runtime(&tree, &all_observations, Variant::Masking);

    match format {
        Format::Text => {
            println!("floyd  runtime MC/DC analysis of {}", manifest.display());
            println!();
            println!("Test targets: {}", build.test_artifacts.len());
            print_runtime_report_body(&all_tests, &all_observations, &analysis);
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
            }
            let report = Report {
                manifest: manifest.display().to_string(),
                workspace_root: build.workspace_root.display().to_string(),
                test_targets: build.test_artifacts.len(),
                tests_discovered: &all_tests,
                observations: &all_observations,
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
    let tree = decision::decompose(&mir);

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

    // 5. Run each test individually, build a ConditionObservation.
    let mut observations = Vec::new();
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
        if let Some(obs) =
            correlate::observation_from_coverage(Some(test_name.clone()), &mir, &coverage, &tree)
        {
            observations.push(obs);
        }
    }

    // 6. Combined static + runtime analysis.
    let analysis = masking::analyze_with_runtime(&tree, &observations, Variant::Masking);

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

    // One selection drives both sections. Reporting each condition's first
    // valid pair here and sizing the test set from a separate computation
    // let the two disagree: on corpus 003 the pairs printed were not the
    // pairs counted, and the count came out 5 against the pattern's pinned
    // minimum of 4.
    let minimum = floyd::masking::minimum_test_set(matrix);

    println!("Independence pairs ({:?} variant):", matrix.variant);
    for cond in &matrix.conditions {
        match minimum.chosen_pairs.get(cond) {
            Some(pair) => print_pair(cond, &matrix.conditions, pair),
            None => println!("  {cond}: no valid independence pair found"),
        }
    }
    println!();

    let label = if minimum.proven_minimal {
        "Minimum test set"
    } else {
        "Smallest test set found (search budget exhausted; upper bound)"
    };
    println!("{label}: {} test(s)", minimum.tests.len());
    for row in &minimum.tests {
        println!(
            "  ({}) -> {}",
            format_inputs(&matrix.conditions, row),
            bool_glyph(row.result)
        );
    }
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
