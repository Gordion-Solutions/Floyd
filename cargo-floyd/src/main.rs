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
//! Both modes support `--format=text` (default) or `--format=json`.

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
            format = match rest {
                "json" => Format::Json,
                "text" => Format::Text,
                other => {
                    eprintln!("cargo-floyd: unknown --format value: {other}");
                    return ExitCode::from(2);
                }
            };
            args.remove(i);
        } else if args[i] == "--format" {
            args.remove(i);
            let value = args.get(i).copied();
            format = match value {
                Some("json") => Format::Json,
                Some("text") => Format::Text,
                Some(other) => {
                    eprintln!("cargo-floyd: unknown --format value: {other}");
                    return ExitCode::from(2);
                }
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

    let source = match args.first() {
        Some(p) => Path::new(*p),
        None => {
            print_usage();
            return ExitCode::from(2);
        }
    };
    if !source.exists() {
        eprintln!("cargo-floyd: source not found: {}", source.display());
        return ExitCode::from(2);
    }

    let workdir = std::env::temp_dir().join(format!("cargo-floyd-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&workdir) {
        eprintln!("cargo-floyd: cannot create workdir: {e}");
        return ExitCode::from(1);
    }

    if is_test_mode {
        run_test_mode(source, &workdir, format)
    } else {
        run_static_mode(source, &workdir, format)
    }
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
    }
    ExitCode::SUCCESS
}

fn run_test_mode(source: &Path, workdir: &Path, format: Format) -> ExitCode {
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
    }
    ExitCode::SUCCESS
}

fn print_usage() {
    eprintln!("cargo-floyd  MC/DC coverage analysis for Rust");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("    cargo floyd [--format=text|json] <path/to/source.rs>");
    eprintln!("        Static MC/DC analysis: prints the recovered truth table");
    eprintln!("        and per-condition independence pairs.");
    eprintln!();
    eprintln!("    cargo floyd test [--format=text|json] <path/to/source.rs>");
    eprintln!("        Runtime MC/DC analysis: compiles the source as a test");
    eprintln!("        crate, runs each #[test] function individually with");
    eprintln!("        coverage instrumentation, and reports which conditions");
    eprintln!("        the test suite exercises under masking MC/DC.");
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
