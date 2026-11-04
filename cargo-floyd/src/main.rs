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
use floyd::coverage_index::{CoverageIndex, PathNormaliser};
use floyd::decision;
use floyd::instrument;
use floyd::masking::{
    self, ConditionObservation, ConditionStatus, IndependencePair, RuntimeAnalysis, TruthTableRow,
    Variant,
};
use floyd::mir;
use floyd::runner;

mod failure;
mod harness;
mod selector;
mod summary;
use std::path::Path;
use std::process::ExitCode;
use summary::CoverageSummary;

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
    /// The instrumented `cargo build --tests` failed (`error` carries
    /// rustc's diagnostics, or the internal compiler error).
    BuildFailed,
    /// A required tool (`llvm-profdata`, `llvm-cov`) was not found.
    SetupFailed,
    /// A test binary's `--list` failed, so its tests are unknown.
    TestDiscoveryFailed,
    /// Running a test, or exporting its coverage, failed.
    TestRunFailed,
}

impl RuntimeVerdict {
    /// The verdict as it appears in the JSON (`build_failed`, ...).
    fn as_str(self) -> &'static str {
        match self {
            RuntimeVerdict::Analysed => "analysed",
            RuntimeVerdict::NoCandidates => "no_candidates",
            RuntimeVerdict::AllUnobserved => "all_unobserved",
            RuntimeVerdict::BuildFailed => "build_failed",
            RuntimeVerdict::SetupFailed => "setup_failed",
            RuntimeVerdict::TestDiscoveryFailed => "test_discovery_failed",
            RuntimeVerdict::TestRunFailed => "test_run_failed",
        }
    }
}

/// A function with a `switchInt`-shaped decision that the engine's
/// current patterns did not match.
#[derive(serde::Serialize, Clone, Debug)]
struct DeclinedFunction {
    name: String,
    source_file: Option<String>,
    line: Option<u32>,
    /// Why the decomposer declined it: `unsupported_shape`,
    /// `non_bool_leaf` when an arm's value is a named local that is
    /// not `bool`, `no_conditions` when the tree tests no condition,
    /// or `ctfe_body` for a `const fn`'s `MIR FOR CTFE` body.
    reason: decision::DeclineReason,
}

/// One decomposable candidate the engine considered, with whether
/// any test reached its function at runtime.
#[derive(serde::Serialize, Clone, Debug)]
struct CandidateSummary {
    /// The function's name as rustc printed it in the MIR header
    /// (impl methods read `<impl at FILE:L:C: L:C>::name`). Kept
    /// verbatim for traceability back to the MIR.
    function_name: Option<String>,
    /// Report name from the coverage symbol the span join found
    /// (demangled, no crate hashes, generic arguments removed);
    /// the MIR name when the join found none.
    display_name: Option<String>,
    source_file: Option<String>,
    line: Option<u32>,
    /// True when at least one test's coverage shows the function
    /// was executed. Determined by the same source-span join as
    /// `observations`. False for a `not_measurable` candidate.
    invoked: bool,
    /// Number of tests that produced an observation (a condition
    /// vector or an ambiguous record).
    observations: usize,
    /// Present when coverage cannot tell whether the function ran
    /// (rustc did not instrument it, its decision comes from a
    /// dependency's macro, no coverage record has its spans, or the
    /// spans are ambiguous). Such a candidate is neither invoked nor
    /// never-invoked.
    #[serde(skip_serializing_if = "Option::is_none")]
    not_measurable: Option<correlate::NotMeasurable>,
    /// Present when no test Floyd ran entered the function and its
    /// package has `harness = false` test targets Floyd skipped
    /// (see `skipped_no_harness`): names those targets, which may
    /// be what exercises it.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    /// Conditions of the decision with no branch region in the
    /// coverage record the join found (their span has none, or they
    /// have no span). Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conditions_without_branch_region: Vec<String>,
    /// The lift that made the decision a candidate, when one did:
    /// `call_arms` (both arms end in non-panic calls) or
    /// `value_arms` (both arms write one non-`bool` value and
    /// join). Omitted for a decision over Boolean leaves.
    #[serde(skip_serializing_if = "Option::is_none")]
    lift: Option<decision::Lift>,
}

impl CandidateSummary {
    fn new(prov: &floyd::DecisionProvenance, join: &correlate::CandidateCoverage) -> Self {
        CandidateSummary {
            function_name: prov.function_name.clone(),
            display_name: join
                .display_name
                .clone()
                .or_else(|| prov.function_name.clone()),
            source_file: prov.source_file.clone(),
            line: prov.line,
            invoked: join.invoked(),
            observations: selector::observed_test_count(&join.observations),
            not_measurable: match &join.status {
                correlate::CoverageStatus::NotMeasurable(nm) => Some(nm.clone()),
                _ => None,
            },
            note: None,
            conditions_without_branch_region: join.conditions_without_branch_region.clone(),
            lift: prov.lift,
        }
    }

    /// `display_name (file:line)` for report lines.
    fn label(&self) -> String {
        let name = self
            .display_name
            .as_deref()
            .or(self.function_name.as_deref())
            .unwrap_or("<unknown>");
        match (&self.source_file, self.line) {
            (Some(file), Some(line)) => format!("`{name}` ({file}:{line})"),
            _ => format!("`{name}`"),
        }
    }
}

/// The stderr explanation of an all-unobserved verdict, one line per
/// element: what each group of candidates is and why it produced no
/// observation.
///
/// An invoked candidate is blamed on Floyd's span correlation only
/// when its record has branch regions for some of the decision's
/// conditions and none at the span of another
/// (`conditions_without_branch_region` non-empty). A record without
/// any branch region for the decision is `no_branch_region`, not
/// measurable, and is explained as such; an invoked candidate whose
/// condition spans all have branch regions was entered but its
/// decision never evaluated.
fn all_unobserved_explanation(candidates: &[CandidateSummary]) -> Vec<String> {
    let mut lines = vec![
        "cargo-floyd: Floyd found decomposable boolean decisions in your code, but no \
         test produced a usable observation for any of them."
            .to_string(),
    ];
    let notes = |lines: &mut Vec<String>, group: &[&CandidateSummary]| {
        for c in group {
            if let Some(note) = &c.note {
                lines.push(format!("    {}: {note}", c.label()));
            }
        }
    };
    let (drift, unevaluated): (Vec<&CandidateSummary>, Vec<&CandidateSummary>) = candidates
        .iter()
        .filter(|c| c.invoked)
        .partition(|c| !c.conditions_without_branch_region.is_empty());
    if !drift.is_empty() {
        lines.push(format!(
            "  {} candidate(s) WERE invoked at runtime, and their coverage record has \
             branch regions for some of the decision's conditions but none at the span of \
             others. This is most likely a Floyd correlation bug (rustc span drift); please \
             file an issue with --format=json's `candidates` list.",
            drift.len()
        ));
        for c in &drift {
            let conds: Vec<String> = c
                .conditions_without_branch_region
                .iter()
                .map(|n| format!("`{n}`"))
                .collect();
            lines.push(format!(
                "    {}: no branch region for {}",
                c.label(),
                conds.join(", ")
            ));
        }
    }
    if !unevaluated.is_empty() {
        lines.push(format!(
            "  {} candidate(s) WERE invoked at runtime, and coverage has a branch region \
             for every condition of their decision, but no test that entered the function \
             evaluated the decision (its branch counters are zero), or the counters are \
             inconsistent with the decision.",
            unevaluated.len()
        ));
        for c in &unevaluated {
            lines.push(format!("    {}", c.label()));
        }
    }
    let never: Vec<&CandidateSummary> = candidates
        .iter()
        .filter(|c| !c.invoked && c.not_measurable.is_none())
        .collect();
    if !never.is_empty() {
        lines.push(format!(
            "  {} candidate(s) were never invoked by any test — add a test that calls one \
             of your top-level `pub fn` decisions, or verify which candidates were \
             considered via --format=json's `candidates` list.",
            never.len()
        ));
        notes(&mut lines, &never);
    }
    let not_measurable: Vec<&CandidateSummary> = candidates
        .iter()
        .filter(|c| c.not_measurable.is_some())
        .collect();
    if !not_measurable.is_empty() {
        let mut reasons: std::collections::BTreeMap<&str, usize> = Default::default();
        for nm in not_measurable
            .iter()
            .filter_map(|c| c.not_measurable.as_ref())
        {
            *reasons
                .entry(summary::reason_label(&nm.reason))
                .or_default() += 1;
        }
        let reasons: Vec<String> = reasons.iter().map(|(r, n)| format!("{r}: {n}")).collect();
        lines.push(format!(
            "  {} candidate(s) are not measurable by coverage ({}): code rustc does not \
             instrument (such as derive output), code expanded from a dependency's macro, \
             or decisions rustc emits no branch region for (such as a `match` on an enum). \
             Whether their decision was evaluated is unknown, not a test gap. See \
             `not_measurable` in --format=json's `candidates` list.",
            not_measurable.len(),
            reasons.join(", ")
        ));
        notes(&mut lines, &not_measurable);
    }
    lines
}

/// The note for a candidate that no test Floyd ran entered, when
/// `package` (the package whose MIR the candidate came from) has
/// `harness = false` test targets that Floyd skipped: such a target
/// may be the only code that exercises it (rust-url's `valid_value`
/// runs only from `data-url/tests/wpt.rs`).
///
/// Applies to a never-invoked candidate and to a not-measurable one
/// whose record was not found or has no branch region, provided no
/// test entered the function. `None` otherwise.
fn harness_free_note(
    join: &correlate::CandidateCoverage,
    package: &str,
    skipped_no_harness: &[NoHarnessTarget],
) -> Option<String> {
    use correlate::{CoverageStatus, NotMeasurableReason};
    if join.entered {
        return None;
    }
    let eligible = match &join.status {
        CoverageStatus::NeverInvoked => true,
        CoverageStatus::NotMeasurable(nm) => matches!(
            nm.reason,
            NotMeasurableReason::NoBranchRegion | NotMeasurableReason::NoCoverageRecord
        ),
        CoverageStatus::Invoked => false,
    };
    if !eligible {
        return None;
    }
    let mut targets: Vec<String> = skipped_no_harness
        .iter()
        .filter(|t| t.package == package)
        .map(|t| format!("`{}`", t.target))
        .collect();
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return None;
    }
    Some(format!(
        "no test Floyd ran entered this function; it may be exercised by the \
         `harness = false` test target(s) {} of package `{}`, which Floyd \
         does not run",
        targets.join(", "),
        package_display_name(package)
    ))
}

/// The package name in a cargo package id: `data-url` for
/// `path+file:///w/data-url#0.3.2`, `serde` for
/// `registry+https://..#serde@1.0.0`, `foo` for the legacy
/// `foo 0.1.0 (path+file:///w)`. Anything else is returned as is.
fn package_display_name(id: &str) -> &str {
    if let Some((name, _)) = id.split_once(' ') {
        return name;
    }
    let Some((url, fragment)) = id.rsplit_once('#') else {
        return id;
    };
    match fragment.split_once('@') {
        Some((name, _)) => name,
        None => url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(id),
    }
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
/// project. Emitted on every exit path: the analysed verdict, the two
/// decline paths, and the error paths (`build_failed`,
/// `setup_failed`, `test_discovery_failed`, `test_run_failed`), which
/// carry `error` and whatever was collected before the failure.
#[derive(serde::Serialize)]
struct CargoRuntimeReport<'a> {
    manifest: String,
    /// `null` when the build failed before cargo reported it.
    workspace_root: Option<String>,
    test_targets: usize,
    verdict: RuntimeVerdict,
    /// Why the run stopped; `null` unless `verdict` is an error.
    error: Option<&'a failure::RunError>,
    tests_discovered: &'a [String],
    observations: &'a [ConditionObservation],
    /// `None` unless `verdict` is `analysed`.
    analysis: Option<&'a RuntimeAnalysis>,
    /// Condition counts for the analysed decision and the
    /// percentage, `null` whenever a condition is unknown.
    summary: CoverageSummary,
    /// Every decomposable candidate considered, before selection.
    candidates: &'a [CandidateSummary],
    /// Functions with a `switchInt`-shaped decision that the
    /// engine's current patterns did not match. The engine declined
    /// them rather than report a wrong verdict — honest-scope
    /// evidence for ISO 26262 / DO-178C qualification audits.
    out_of_scope: &'a [DeclinedFunction],
    /// MIR files above the size guard that were not analysed.
    skipped_oversize_mirs: &'a [SkippedMir],
    /// The size guard in effect (`--max-mir-size`, default 50).
    max_mir_size_mb: u64,
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
    /// Condition counts for the analysed decision and the
    /// percentage, `null` whenever a condition is unknown.
    summary: CoverageSummary,
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
    // Cargo feature selection, passed through to the instrumented
    // `cargo build --tests`.
    let mut cargo_args: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--all-features" || args[i] == "--no-default-features" {
            cargo_args.push(args[i].to_string());
            args.remove(i);
            continue;
        }
        if let Some(val) = args[i]
            .strip_prefix("--features=")
            .or_else(|| args[i].strip_prefix("-F="))
        {
            cargo_args.push(format!("--features={val}"));
            args.remove(i);
            continue;
        }
        if args[i] == "--features" || args[i] == "-F" {
            let flag = args.remove(i);
            match args.get(i).copied() {
                Some(v) if !v.starts_with('-') => {
                    cargo_args.push(format!("--features={v}"));
                    args.remove(i);
                }
                _ => {
                    eprintln!("cargo-floyd: {flag} requires a value (e.g. {flag} serde,std)");
                    return ExitCode::from(2);
                }
            }
            continue;
        }
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

    if !cargo_args.is_empty() && !matches!(source, Source::Cargo(_)) {
        eprintln!(
            "cargo-floyd: --features, --all-features and --no-default-features \
             apply to cargo projects only"
        );
        return ExitCode::from(2);
    }

    match (is_test_mode, source) {
        (true, Source::Cargo(p)) => run_test_mode_cargo(
            &p,
            &workdir,
            format,
            &only_patterns,
            max_mir_mb,
            &cargo_args,
        ),
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
    cargo_args: &[String],
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

    let max_mir_mb = max_mir_mb.unwrap_or(DEFAULT_MAX_MIR_MB);
    let partial = |workspace_root| Partial {
        manifest,
        workspace_root,
        test_targets: 0,
        tests_discovered: &[],
        out_of_scope: &[],
        skipped_oversize_mirs: &[],
        skipped_no_harness: &[],
        max_mir_mb,
    };

    // Build all test artifacts in the cargo project.
    let build_started = std::time::SystemTime::now();
    let build = match instrument::compile_cargo_project_with_args(manifest, cargo_args) {
        Ok(b) => b,
        Err(e) => {
            let error = build_error(manifest, &e, build_started);
            return emit_run_error(format, &partial(None), RuntimeVerdict::BuildFailed, &error);
        }
    };

    let tools = runner::llvm_tool("llvm-profdata")
        .and_then(|p| runner::llvm_tool("llvm-cov").map(|c| (p, c)));
    let (profdata_tool, cov_tool) = match tools {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            let error = runner_error(&e);
            return emit_run_error(
                format,
                &Partial {
                    test_targets: build.test_artifacts.len(),
                    ..partial(Some(&build.workspace_root))
                },
                RuntimeVerdict::SetupFailed,
                &error,
            );
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
        package: String,
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
    let max_mir_bytes: u64 = max_mir_mb * 1024 * 1024;
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
                return emit_run_error(
                    format,
                    &Partial {
                        workspace_root: Some(&build.workspace_root),
                        test_targets: build.test_artifacts.len(),
                        tests_discovered: &all_tests,
                        out_of_scope: &declined,
                        skipped_oversize_mirs: &skipped_oversize_mirs,
                        skipped_no_harness: &skipped_no_harness,
                        ..partial(None)
                    },
                    RuntimeVerdict::TestDiscoveryFailed,
                    &runner_error(&e),
                );
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
                let (node, lift) = match decision::try_decompose_candidate_lifted(f) {
                    Ok(decomposed) => decomposed,
                    Err(reason) => {
                        // Has a switchInt (so some kind of decision)
                        // but the decomposer did not match any
                        // supported pattern. Honest-scope report entry.
                        let prov = correlate::provenance_for(f);
                        declined.push(DeclinedFunction {
                            name: prov.function_name.unwrap_or_default(),
                            source_file: prov.source_file,
                            line: prov.line,
                            reason,
                        });
                        continue;
                    }
                };
                let prov = floyd::DecisionProvenance {
                    lift,
                    ..correlate::provenance_for(f)
                };
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
            }
        }
        artifacts.push(ArtifactInfo {
            mir,
            binary_path: artifact.binary_path.clone(),
            tests,
            package: artifact.package_name.clone(),
        });
    }

    if !skipped_oversize_mirs.is_empty() {
        print_oversize_notice(&mut std::io::stderr(), &skipped_oversize_mirs, max_mir_mb);
    }
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] Pass 1 complete: {} candidates, {} declined, {} artifacts, {} skipped-oversize",
        raw_candidates.len(), declined.len(), artifacts.len(), skipped_oversize_mirs.len());
    }
    // Source spans are compared in one canonical form: relative to
    // the workspace root rustc ran in (see `PathNormaliser`).
    let paths = PathNormaliser::new(Some(&build.workspace_root));
    // Pre-compute normalised condition span maps per artifact's MIR.
    // Done once; reused across every (candidate, test) pair in
    // Pass 3. This turns Pass 3 from O(candidates × tests ×
    // functions × blocks) into O(functions × blocks) +
    // O(candidates × tests).
    let prepared: Vec<correlate::PreparedMir<'_>> = artifacts
        .iter()
        .map(|a| correlate::prepare_mir(&a.mir, &paths))
        .collect();
    // Every span that locates a candidate's coverage record (its
    // decision spans and its body's coverage-mapping spans). Each
    // test's coverage index keeps only the records containing one
    // of these, so Pass 2 holds a few records per test instead of
    // every record of every test.
    let wanted: std::collections::HashSet<floyd::mir::SourceSpan> = raw_candidates
        .iter()
        .flat_map(|raw| prepared[raw.artifact_idx].lookup_spans(raw.fn_idx, &raw.tree))
        .collect();

    // Pass 2: run every test across every artifact once; keep each
    // test's coverage, indexed by source span. We intentionally do
    // NOT dedupe test names across artifacts — two artifacts can
    // share a test name.
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
    let results: Result<Vec<(String, CoverageIndex)>, runner::RunnerError> = jobs
        .par_iter()
        .map(
            |job| -> Result<(String, CoverageIndex), runner::RunnerError> {
                let prof = workdir.join(format!(
                    "cov-{:03}-{:03}.profraw",
                    job.art_idx, job.test_idx
                ));
                runner::run_test_isolated(&job.binary_path, &job.test_name, &prof)?;
                let coverage = runner::profraw_to_coverage(
                    &prof,
                    &profdata_tool,
                    &cov_tool,
                    &job.binary_path,
                    workdir,
                )?;
                let n = progress_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if (n.is_multiple_of(10) || n == progress_total)
                    && std::env::var("FLOYD_DIAG").is_ok()
                {
                    eprintln!("[progress]   {n}/{progress_total}");
                }
                Ok((
                    job.test_name.clone(),
                    CoverageIndex::build_for(&coverage, &paths, &wanted),
                ))
            },
        )
        .collect();
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] Pass 2 complete");
    }

    let test_indexes = match results {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cargo-floyd: {e}");
            return emit_run_error(
                format,
                &Partial {
                    workspace_root: Some(&build.workspace_root),
                    test_targets: build.test_artifacts.len(),
                    tests_discovered: &all_tests,
                    out_of_scope: &declined,
                    skipped_oversize_mirs: &skipped_oversize_mirs,
                    skipped_no_harness: &skipped_no_harness,
                    ..partial(None)
                },
                RuntimeVerdict::TestRunFailed,
                &runner_error(&e),
            );
        }
    };

    // Pass 3: join every candidate to every test's coverage index
    // by source span. Candidates rustc did not instrument, or whose
    // decision comes from a dependency's macro, are classified from
    // their MIR without a join. The same per-test join gives both
    // the observations and the `invoked` status. Parallelised
    // across candidates — each is independent.
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] starting Pass 3");
    }
    let mir_instrumented: Vec<bool> = artifacts
        .iter()
        .map(|a| a.mir.functions.iter().any(|f| f.coverage_mappings > 0))
        .collect();
    let mut joins: Vec<correlate::CandidateCoverage> = raw_candidates
        .par_iter()
        .map(|raw| {
            let prep = &prepared[raw.artifact_idx];
            match correlate::mir_not_measurable(
                prep,
                raw.fn_idx,
                &raw.tree,
                mir_instrumented[raw.artifact_idx],
                Some(&paths),
            ) {
                Some(nm) => correlate::CandidateCoverage::not_measurable(nm.reason, nm.detail),
                None => correlate::join_candidate(prep, raw.fn_idx, &raw.tree, &test_indexes),
            }
        })
        .collect();
    {
        let mut named: Vec<(&str, &mut correlate::CandidateCoverage)> = raw_candidates
            .iter()
            .map(|raw| {
                prepared[raw.artifact_idx].mir.functions[raw.fn_idx]
                    .name
                    .as_str()
            })
            .zip(joins.iter_mut())
            .collect();
        correlate::mark_shared_coverage(&mut named);
    }
    if std::env::var("FLOYD_DIAG").is_ok() {
        eprintln!("[progress] Pass 3 complete: {} candidates", joins.len());
    }

    // FLOYD_DIAG span-mismatch probe: for every candidate that WAS
    // reached at runtime but produced zero observations, dump its
    // decision spans alongside the branch regions of its coverage
    // function in the first test that entered it. Lets a human diff
    // the two and see which span the condition join missed.
    if std::env::var("FLOYD_DIAG").is_ok() {
        for (raw, join) in raw_candidates.iter().zip(&joins) {
            if !join.invoked() || !join.observations.is_empty() {
                continue;
            }
            let prep = &prepared[raw.artifact_idx];
            eprintln!(
                "[span-diag] candidate fn={}",
                prep.mir.functions[raw.fn_idx].name
            );
            for (name, span) in &prep.function_spans[raw.fn_idx] {
                eprintln!("[span-diag]     MIR cond '{name}' @ {span:?}");
            }
            let spans = prep.lookup_spans(raw.fn_idx, &raw.tree);
            for (test_name, index) in &test_indexes {
                if let floyd::coverage_index::FunctionMatch::Found(fc) =
                    index.resolve(&spans, Some(&prep.mir.functions[raw.fn_idx].name))
                {
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

    let candidate_summaries: Vec<CandidateSummary> = raw_candidates
        .iter()
        .zip(&joins)
        .map(|(raw, join)| CandidateSummary {
            note: harness_free_note(
                join,
                &artifacts[raw.artifact_idx].package,
                &skipped_no_harness,
            ),
            ..CandidateSummary::new(&raw.prov, join)
        })
        .collect();
    let candidates: Vec<selector::Candidate> = raw_candidates
        .into_iter()
        .zip(joins)
        .map(|(raw, join)| {
            let mut provenance = raw.prov;
            provenance.display_name = join.display_name;
            selector::Candidate {
                tree: raw.tree,
                provenance,
                observations: join.observations,
            }
        })
        .collect();
    let not_measurable: Vec<&correlate::NotMeasurable> = candidate_summaries
        .iter()
        .filter_map(|c| c.not_measurable.as_ref())
        .collect();
    let not_measurable_count = not_measurable.len();
    let skipped_mirs = skipped_mir_entries(&skipped_oversize_mirs);
    let manifest_str = manifest.display().to_string();
    let workspace_root_str = build.workspace_root.display().to_string();
    let test_targets = build.test_artifacts.len();
    let report = |verdict, observations, analysis| CargoRuntimeReport {
        manifest: manifest_str.clone(),
        workspace_root: Some(workspace_root_str.clone()),
        test_targets,
        verdict,
        error: None,
        tests_discovered: &all_tests,
        observations,
        analysis,
        summary: CoverageSummary::new(analysis, not_measurable_count),
        candidates: &candidate_summaries,
        out_of_scope: &declined,
        skipped_oversize_mirs: &skipped_mirs,
        max_mir_size_mb: max_mir_mb,
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
                max_mir_mb,
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
                max_mir_mb,
                &skipped_no_harness,
            ),
        }
        ExitCode::from(1)
    };

    let chosen = match selector::pick_best(candidates) {
        selector::PickResult::Picked(c) => c,
        selector::PickResult::AllUnobserved => {
            for line in all_unobserved_explanation(&candidate_summaries) {
                eprintln!("{line}");
            }
            return emit_decline(RuntimeVerdict::AllUnobserved);
        }
        selector::PickResult::NoCandidates => {
            eprintln!(
                "{}",
                no_candidates_line(
                    &skipped_oversize_mirs,
                    build.test_artifacts.len(),
                    max_mir_mb
                )
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
            print_runtime_report_body(&all_tests, &all_observations, &analysis, &not_measurable);
            print_out_of_scope_report(
                &mut std::io::stdout(),
                &declined,
                &skipped_oversize_mirs,
                max_mir_mb,
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
            print!(
                "{}",
                render_runtime_junit(
                    &manifest.display().to_string(),
                    &analysis,
                    not_measurable_count
                )
            );
        }
    }
    ExitCode::from(CoverageSummary::new(Some(&analysis), not_measurable_count).exit_code())
}

/// Default `--max-mir-size`, in megabytes.
const DEFAULT_MAX_MIR_MB: u64 = 50;

/// What a runtime run on a cargo project had collected when it
/// stopped with an error.
struct Partial<'a> {
    manifest: &'a Path,
    workspace_root: Option<&'a Path>,
    test_targets: usize,
    tests_discovered: &'a [String],
    out_of_scope: &'a [DeclinedFunction],
    skipped_oversize_mirs: &'a [(std::path::PathBuf, u64)],
    skipped_no_harness: &'a [NoHarnessTarget],
    max_mir_mb: u64,
}

/// Emit the document for an error exit and return exit code 1.
/// stderr already carries the human-readable failure. JSON gets the
/// report with `verdict`, `error` and the partial data; JUnit gets a
/// document with one `<error>`; text prints nothing more on stdout.
fn emit_run_error(
    format: Format,
    partial: &Partial<'_>,
    verdict: RuntimeVerdict,
    error: &failure::RunError,
) -> ExitCode {
    match format {
        Format::Text => {}
        Format::Json => {
            let skipped = skipped_mir_entries(partial.skipped_oversize_mirs);
            if let Err(code) = print_json(&error_report(partial, verdict, error, &skipped)) {
                return code;
            }
        }
        Format::Junit => print!(
            "{}",
            failure::render_error_junit(
                &partial.manifest.display().to_string(),
                verdict.as_str(),
                error
            )
        ),
    }
    ExitCode::from(1)
}

/// The JSON report of an error exit: no analysis, no candidates (the
/// candidate join had not run), `summary.percentage` null.
fn error_report<'a>(
    partial: &Partial<'a>,
    verdict: RuntimeVerdict,
    error: &'a failure::RunError,
    skipped: &'a [SkippedMir],
) -> CargoRuntimeReport<'a> {
    CargoRuntimeReport {
        manifest: partial.manifest.display().to_string(),
        workspace_root: partial.workspace_root.map(|p| p.display().to_string()),
        test_targets: partial.test_targets,
        verdict,
        error: Some(error),
        tests_discovered: partial.tests_discovered,
        observations: &[],
        analysis: None,
        summary: CoverageSummary::new(None, 0),
        candidates: &[],
        out_of_scope: partial.out_of_scope,
        skipped_oversize_mirs: skipped,
        max_mir_size_mb: partial.max_mir_mb,
        skipped_no_harness: partial.skipped_no_harness,
    }
}

fn skipped_mir_entries(skipped: &[(std::path::PathBuf, u64)]) -> Vec<SkippedMir> {
    skipped
        .iter()
        .map(|(path, bytes)| SkippedMir {
            path: path.display().to_string(),
            bytes: *bytes,
        })
        .collect()
}

/// Print rustc's diagnostics (or the internal compiler error) for a
/// failed instrumented build to stderr and return the `error` object.
/// A failure before rustc ran (no Cargo.toml, `cargo metadata`
/// failing, no test artifacts) has no diagnostics; cargo's stderr is
/// printed as is.
fn build_error(
    manifest: &Path,
    e: &instrument::InstrumentError,
    started: std::time::SystemTime,
) -> failure::RunError {
    let package_dir = if manifest.is_dir() {
        manifest
    } else {
        manifest.parent().unwrap_or(Path::new("."))
    };
    let parsed = failure::BuildFailure::parse(&e.stdout, &e.stderr, &[package_dir], Some(started));
    let stderr = failure::trimmed_stderr(&e.stderr);
    let message = if parsed.diagnostics.is_empty() && parsed.ice.is_none() {
        eprintln!("cargo-floyd: {e}");
        let first = stderr
            .lines()
            .find(|l| l.trim_start().starts_with("error"))
            .or_else(|| stderr.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("")
            .trim();
        if first.is_empty() {
            format!("{} failed", e.stage)
        } else {
            format!("{} failed: {first}", e.stage)
        }
    } else {
        parsed.print(&mut std::io::stderr());
        parsed.summary_line()
    };
    failure::RunError {
        stage: e.stage.to_string(),
        message,
        diagnostics: parsed.diagnostics,
        ice: parsed.ice,
        stderr,
    }
}

/// The `error` object for a runner failure (`--list`, a test run,
/// coverage export, tool lookup).
fn runner_error(e: &runner::RunnerError) -> failure::RunError {
    let first = e.message.lines().next().unwrap_or("").trim();
    let first = first
        .trim_end_matches("; binary stderr:")
        .trim_end_matches("; stderr:");
    failure::RunError {
        stage: e.stage.to_string(),
        message: format!("{}: {first}", e.stage),
        diagnostics: Vec::new(),
        ice: None,
        stderr: failure::trimmed_stderr(&e.message),
    }
}

/// A `--max-mir-size` value, in megabytes, that admits every skipped
/// MIR file: the largest one rounded up to the next multiple of 50.
fn suggested_mir_limit(skipped: &[(std::path::PathBuf, u64)]) -> u64 {
    let largest_mb = skipped
        .iter()
        .map(|(_, b)| b.div_ceil(1024 * 1024))
        .max()
        .unwrap_or(DEFAULT_MAX_MIR_MB);
    (largest_mb / 50 + 1) * 50
}

fn format_mb(bytes: u64) -> String {
    format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// The stderr notice for MIR files skipped by the size guard: each
/// file and its size, the limit, and the flag that raises it.
fn print_oversize_notice(
    out: &mut dyn std::io::Write,
    skipped: &[(std::path::PathBuf, u64)],
    max_mir_mb: u64,
) {
    let _ = writeln!(
        out,
        "cargo-floyd: skipped {} test-target MIR file(s) larger than the \
         --max-mir-size limit of {max_mir_mb} MB; their tests and \
         decisions are not analysed:",
        skipped.len()
    );
    for (path, bytes) in skipped {
        let _ = writeln!(out, "  {}  ({})", path.display(), format_mb(*bytes));
    }
    let _ = writeln!(
        out,
        "  To analyse them, raise the limit: cargo floyd test --max-mir-size={} \
         (parsing takes longer and uses more memory).",
        suggested_mir_limit(skipped)
    );
}

/// The stderr line for the `no_candidates` verdict. When MIR files
/// were skipped by the size guard it says so first, since the
/// decisions in them were never looked at.
fn no_candidates_line(
    skipped: &[(std::path::PathBuf, u64)],
    test_targets: usize,
    max_mir_mb: u64,
) -> String {
    const SCOPE: &str = "The engine currently supports `&&`, `||`, `!`, and nested \
         combinations; `if let`, `?`, and `match` patterns are \
         partially supported (2-arm const-returning shapes); \
         non-boolean decisions and arms that end in a function \
         call are out of scope.";
    if skipped.is_empty() {
        return format!("cargo-floyd: no recognised decisions in any test target. {SCOPE}");
    }
    let all = skipped.len() >= test_targets;
    let which = if all {
        format!("all {} test target(s)", skipped.len())
    } else {
        format!("{} of {test_targets} test target(s)", skipped.len())
    };
    let rest = if all {
        String::new()
    } else {
        format!(" In the analysed targets: {SCOPE}")
    };
    format!(
        "cargo-floyd: no recognised decisions: the MIR of {which} exceeded \
         --max-mir-size={max_mir_mb} (MB) and was not analysed; rerun with \
         --max-mir-size={} to include it.{rest}",
        suggested_mir_limit(skipped)
    )
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
    // rustc ran in this process's working directory, so relative
    // MIR paths and the absolute paths llvm-cov reports both
    // normalise against it.
    let cwd = std::env::current_dir().ok();
    let paths = PathNormaliser::new(cwd.as_deref());
    let prepared = correlate::prepare_mir(&mir, &paths);
    let mut artifact_candidates: Vec<(floyd::DecisionTree, floyd::DecisionProvenance, usize)> =
        Vec::new();
    for (fn_idx, f) in mir.functions.iter().enumerate() {
        if let Ok((node, lift)) = decision::try_decompose_candidate_lifted(f) {
            artifact_candidates.push((
                floyd::DecisionTree {
                    decisions: vec![node],
                },
                floyd::DecisionProvenance {
                    lift,
                    ..correlate::provenance_for(f)
                },
                fn_idx,
            ));
        }
    }
    let wanted: std::collections::HashSet<floyd::mir::SourceSpan> = artifact_candidates
        .iter()
        .flat_map(|(tree, _, fn_idx)| prepared.lookup_spans(*fn_idx, tree))
        .collect();

    // 5. Run each test individually.
    let mut per_test_coverage: Vec<(String, CoverageIndex)> = Vec::with_capacity(tests.len());
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
        per_test_coverage.push((
            test_name.clone(),
            CoverageIndex::build_for(&coverage, &paths, &wanted),
        ));
    }

    // Join every candidate to every test's coverage by source span.
    // This MIR comes from a separate, uninstrumented rustc run, so
    // it carries no coverage mappings to classify candidates by;
    // the join alone decides.
    let mut joins: Vec<correlate::CandidateCoverage> = artifact_candidates
        .iter()
        .map(|(tree, _, fn_idx)| {
            correlate::join_candidate(&prepared, *fn_idx, tree, &per_test_coverage)
        })
        .collect();
    {
        let mut named: Vec<(&str, &mut correlate::CandidateCoverage)> = artifact_candidates
            .iter()
            .map(|(_, _, fn_idx)| mir.functions[*fn_idx].name.as_str())
            .zip(joins.iter_mut())
            .collect();
        correlate::mark_shared_coverage(&mut named);
    }
    let candidates: Vec<selector::Candidate> = artifact_candidates
        .into_iter()
        .zip(joins)
        .map(|((tree, mut provenance, _), join)| {
            provenance.display_name = join.display_name;
            selector::Candidate {
                tree,
                provenance,
                observations: join.observations,
            }
        })
        .collect();

    // 6. Pick the best candidate, run the analysis.
    let report = |verdict, observations, analysis| SingleRuntimeReport {
        source: source.display().to_string(),
        verdict,
        tests_discovered: &tests,
        observations,
        analysis,
        summary: CoverageSummary::new(analysis, 0),
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
            print!(
                "{}",
                render_runtime_junit(&source.display().to_string(), &analysis, 0)
            );
        }
    }
    ExitCode::from(CoverageSummary::new(Some(&analysis), 0).exit_code())
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
    eprintln!("    cargo floyd test [--format=text|json|junit] [--only=<pat>]...");
    eprintln!("                     [--max-mir-size=<MB>] [--features=<list>]");
    eprintln!("                     [--all-features] [--no-default-features] [<path>]");
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
    eprintln!("        --only=<pat> (repeatable) analyses only functions whose");
    eprintln!("        name contains <pat>.");
    eprintln!();
    eprintln!("        --max-mir-size=<MB> (default 50) skips test-target MIR");
    eprintln!("        files larger than <MB> megabytes; skipped files are");
    eprintln!("        listed on stderr and in the JSON `skipped_oversize_mirs`.");
    eprintln!();
    eprintln!("        --features=<list> (or -F <list>), --all-features and");
    eprintln!("        --no-default-features are passed to the instrumented");
    eprintln!("        `cargo build --tests`, as for `cargo test`.");
    eprintln!();
    eprintln!("        On an error (build failure, test discovery or a test run");
    eprintln!("        failing) the exit code is 1 and --format=json/junit still");
    eprintln!("        print a document: JSON `verdict` build_failed,");
    eprintln!("        test_discovery_failed, test_run_failed or setup_failed,");
    eprintln!("        with an `error` object; JUnit one testcase with <error>.");
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
    print_runtime_report_body(tests, observations, analysis, &[]);
}

fn print_runtime_report_body(
    tests: &[String],
    observations: &[ConditionObservation],
    analysis: &RuntimeAnalysis,
    not_measurable: &[&correlate::NotMeasurable],
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
    for cond in &analysis.matrix.conditions {
        match analysis.condition_status.get(cond) {
            Some(ConditionStatus::Exercised(pair)) => {
                let t1 = format_inputs(&analysis.matrix.conditions, &pair.test_1);
                let t2 = format_inputs(&analysis.matrix.conditions, &pair.test_2);
                println!(
                    "  ✓ {cond}: EXERCISED  via ({t1}) -> {}   vs   ({t2}) -> {}",
                    bool_glyph(pair.test_1.result),
                    bool_glyph(pair.test_2.result),
                );
            }
            Some(ConditionStatus::Ambiguous { tests }) => {
                println!("  ? {cond}: {}", unknown_reason(tests, observations));
            }
            Some(ConditionStatus::Unexercised) | None => {
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
                if let Some(note) = ruled_out_note(analysis, cond) {
                    println!("      {note}");
                }
            }
        }
    }
    let summary = CoverageSummary::new(Some(analysis), not_measurable.len());
    if let Some(line) = summary.coverage_line(analysis.ambiguous_observations) {
        println!();
        println!("{line}");
        if summary.percentage.is_none() {
            println!("{}", summary::unknown_hint(analysis.ambiguous_observations));
        }
    }
    if let Some(line) = summary::not_measurable_line(not_measurable.iter().copied()) {
        println!();
        println!("{line}");
    }
}

/// The status text after `? {cond}: ` for a condition whose only
/// evidence is ambiguous: `UNKNOWN`, why, and the tests (at most
/// [`summary::TESTS_SHOWN`] named, with their evaluation counts).
fn unknown_reason(tests: &[String], observations: &[ConditionObservation]) -> String {
    format!(
        "UNKNOWN — the outcomes an independence pair still needs were taken only in {} \
         test(s) that evaluated the decision more than once, so no condition vector can be \
         attributed: {}",
        tests.len(),
        format_ambiguous_tests(tests, observations)
    )
}

/// For an unexercised condition that multi-evaluation tests did
/// evaluate, why none of them counts as unknown evidence: each
/// never took an outcome the missing pair row needs, with that
/// test's counters (at most [`summary::TESTS_SHOWN`] tests named).
/// `None` when no multi-evaluation test evaluated the condition.
fn ruled_out_note(analysis: &RuntimeAnalysis, cond: &str) -> Option<String> {
    let why = analysis.ruled_out.get(cond).filter(|w| !w.is_empty())?;
    let conditions = &analysis.matrix.conditions;
    let labels: Vec<String> = why
        .iter()
        .map(|r| {
            let evals = r
                .evaluations
                .map_or_else(String::new, |n| format!(" ({n} evaluations)"));
            format!(
                "{}{evals} never took {}={} ({}T/{}F) as ({}) -> {} needs",
                r.test,
                r.condition,
                bool_glyph(r.value),
                r.counts.true_count,
                r.counts.false_count,
                format_inputs(conditions, &r.row),
                bool_glyph(r.row.result),
            )
        })
        .collect();
    Some(format!(
        "not unknown: no test took the outcome a missing pair row needs; the {} \
         multi-evaluation test(s) that evaluated it are ruled out by their counters: {}",
        why.iter()
            .map(|r| r.test.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        summary::cap_tests(&labels, str::to_string)
    ))
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

/// `t1 (256 evaluations), t2 (3 evaluations)` for the tests behind a
/// [`ConditionStatus::Ambiguous`], naming at most
/// [`summary::TESTS_SHOWN`] of them and summarising the rest as
/// `(+N more)` (all of them are in the per-test listing).
fn format_ambiguous_tests(tests: &[String], observations: &[ConditionObservation]) -> String {
    summary::cap_tests(tests, |t| {
        let evals = observations
            .iter()
            .find(|o| o.ambiguous && o.test_name.as_deref() == Some(t))
            .and_then(|o| o.evaluations);
        match evals {
            Some(n) => format!("{t} ({n} evaluations)"),
            None => t.to_string(),
        }
    })
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
    max_mir_mb: u64,
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
            "Skipped {} MIR file(s) above the {max_mir_mb} MB scalability \
             threshold (monomorphisation explosion in test \
             binaries). These artifacts were not analysed — \
             candidates and tests they would have contributed are \
             absent from the verdict. Raise the limit with \
             --max-mir-size={}:",
            skipped_oversize_mirs.len(),
            suggested_mir_limit(skipped_oversize_mirs)
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

/// Render a JUnit XML report for a runtime analysis. One
/// `<testsuite>` per analysis context (typically one per
/// invocation today), one `<testcase>` per recovered condition.
/// Exercised conditions pass (empty body); unexercised conditions
/// emit a `<failure type="UnexercisedCondition">` with the required
/// independence-pair inputs in the message; conditions whose only
/// evidence is ambiguous emit a `<failure type="AmbiguousCondition">`
/// naming at most [`summary::TESTS_SHOWN`] of the multi-evaluation
/// tests (not demonstrated, so not a pass).
///
/// The suite carries the [`CoverageSummary`] counts as
/// `<properties>`; `floyd.percentage` is present only when no
/// condition is unknown, matching the text and JSON reports.
///
/// The format is the de-facto JUnit XML that Jenkins, GitLab CI,
/// GitHub Actions, Bazel/Buck2, and the other CIs the automotive
/// industry already deploys render natively.
fn render_runtime_junit(source: &str, analysis: &RuntimeAnalysis, not_measurable: usize) -> String {
    use std::fmt::Write as _;
    let conditions = &analysis.matrix.conditions;
    let summary = CoverageSummary::new(Some(analysis), not_measurable);
    let total = summary.conditions_total;
    let failures = total - summary.exercised;
    let mut out = String::new();

    let _ = writeln!(out, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = writeln!(
        out,
        r#"<testsuites name="floyd" tests="{total}" failures="{failures}">"#
    );
    let _ = writeln!(
        out,
        r#"  <testsuite name="{}" tests="{total}" failures="{failures}">"#,
        xml_escape(source)
    );
    let _ = writeln!(out, "    <properties>");
    let mut props: Vec<(&str, String)> = vec![
        ("floyd.conditions_total", total.to_string()),
        ("floyd.exercised", summary.exercised.to_string()),
        ("floyd.not_exercised", summary.not_exercised.to_string()),
        ("floyd.unknown", summary.unknown.to_string()),
        ("floyd.not_measurable", summary.not_measurable.to_string()),
    ];
    if let Some(pct) = summary.percentage {
        props.push(("floyd.percentage", format!("{pct:.0}")));
    }
    for (name, value) in props {
        let _ = writeln!(
            out,
            r#"      <property name="{name}" value="{}"/>"#,
            xml_escape(&value)
        );
    }
    let _ = writeln!(out, "    </properties>");
    for cond in conditions {
        let case_name = xml_escape(cond);
        let (kind, message) = match analysis.condition_status.get(cond.as_str()) {
            Some(ConditionStatus::Exercised(_)) => {
                let _ = writeln!(
                    out,
                    r#"    <testcase name="{case_name}" classname="floyd.mcdc"/>"#
                );
                continue;
            }
            Some(ConditionStatus::Ambiguous { tests }) => (
                "AmbiguousCondition",
                format!(
                    "UNKNOWN: the outcomes an independence pair still needs were taken only \
                     in {} test(s) that evaluated the decision more than once ({}); aggregate \
                     counters cannot attribute a condition vector to them. Add tests that \
                     each evaluate the decision once.",
                    tests.len(),
                    summary::cap_tests(tests, str::to_string)
                ),
            ),
            Some(ConditionStatus::Unexercised) | None => (
                "UnexercisedCondition",
                analysis
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
                    .map(|needs| match ruled_out_note(analysis, cond) {
                        Some(note) => format!("{needs}; {note}"),
                        None => needs,
                    })
                    .unwrap_or_else(|| {
                        "no valid independence pair available for this condition".to_string()
                    }),
            ),
        };
        let _ = writeln!(
            out,
            r#"    <testcase name="{case_name}" classname="floyd.mcdc">"#
        );
        let _ = writeln!(
            out,
            r#"      <failure message="{}" type="{kind}"/>"#,
            xml_escape(&message)
        );
        let _ = writeln!(out, r#"    </testcase>"#);
    }
    let _ = writeln!(out, r#"  </testsuite>"#);
    let _ = writeln!(out, r#"</testsuites>"#);
    out
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
            reason: decision::DeclineReason::NonBoolLeaf,
        }];
        let candidates = vec![
            CandidateSummary {
                function_name: Some("crate::g".to_string()),
                display_name: Some("crate::g".to_string()),
                source_file: Some("src/lib.rs".to_string()),
                line: Some(12),
                invoked: false,
                observations: 0,
                not_measurable: None,
                note: Some("may be exercised by `wpt`".to_string()),
                conditions_without_branch_region: Vec::new(),
                lift: Some(decision::Lift::ValueArms),
            },
            CandidateSummary {
                function_name: Some(
                    "ast::comment::_::<impl at src/ast/comment.rs:11:38: 11:54>::serialize"
                        .to_string(),
                ),
                display_name: Some(
                    "ast::comment::_::<impl at src/ast/comment.rs:11:38: 11:54>::serialize"
                        .to_string(),
                ),
                source_file: Some("src/ast/comment.rs".to_string()),
                line: Some(11),
                invoked: false,
                observations: 0,
                not_measurable: Some(correlate::NotMeasurable {
                    reason: correlate::NotMeasurableReason::Uninstrumented,
                    detail: "no coverage mapping".to_string(),
                }),
                note: None,
                conditions_without_branch_region: Vec::new(),
                lift: None,
            },
        ];
        let skipped = vec![SkippedMir {
            path: "target/x.mir".to_string(),
            bytes: 1,
        }];
        let report = CargoRuntimeReport {
            manifest: "Cargo.toml".to_string(),
            workspace_root: Some(".".to_string()),
            test_targets: 1,
            verdict: RuntimeVerdict::AllUnobserved,
            error: None,
            tests_discovered: &[],
            observations: &[],
            analysis: None,
            summary: CoverageSummary::new(None, 1),
            candidates: &candidates,
            out_of_scope: &declined,
            skipped_oversize_mirs: &skipped,
            max_mir_size_mb: 50,
            skipped_no_harness: &[],
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&report).unwrap()).unwrap();
        assert_eq!(v["verdict"], "all_unobserved");
        assert!(v["analysis"].is_null());
        assert_eq!(v["observations"].as_array().unwrap().len(), 0);
        assert_eq!(v["out_of_scope"][0]["name"], "crate::f");
        assert_eq!(v["out_of_scope"][0]["line"], 7);
        assert_eq!(v["out_of_scope"][0]["reason"], "non_bool_leaf");
        assert_eq!(v["candidates"][0]["function_name"], "crate::g");
        assert_eq!(v["candidates"][0]["invoked"], false);
        assert_eq!(v["candidates"][0]["display_name"], "crate::g");
        assert!(v["candidates"][0].get("not_measurable").is_none());
        assert_eq!(v["candidates"][0]["note"], "may be exercised by `wpt`");
        assert!(v["candidates"][1].get("note").is_none());
        assert_eq!(v["candidates"][0]["lift"], "value_arms");
        assert!(v["candidates"][1].get("lift").is_none());
        assert_eq!(v["candidates"][1]["invoked"], false);
        assert_eq!(
            v["candidates"][1]["not_measurable"]["reason"],
            "uninstrumented"
        );
        assert_eq!(v["skipped_oversize_mirs"][0]["bytes"], 1);
        assert_eq!(v["summary"]["conditions_total"], 0);
        assert_eq!(v["summary"]["not_measurable"], 1);
        assert!(v["summary"]["percentage"].is_null());
    }

    fn coverage(status: correlate::CoverageStatus, entered: bool) -> correlate::CandidateCoverage {
        correlate::CandidateCoverage {
            status,
            observations: Vec::new(),
            display_name: None,
            function_keys: Vec::new(),
            entered,
            conditions_without_branch_region: Vec::new(),
        }
    }

    fn cand_summary(
        name: &str,
        invoked: bool,
        not_measurable: Option<correlate::NotMeasurableReason>,
        missing: &[&str],
    ) -> CandidateSummary {
        CandidateSummary {
            function_name: Some(name.to_string()),
            display_name: Some(name.to_string()),
            source_file: Some("src/lib.rs".to_string()),
            line: Some(1),
            invoked,
            observations: 0,
            not_measurable: not_measurable.map(|reason| correlate::NotMeasurable {
                reason,
                detail: String::new(),
            }),
            note: None,
            conditions_without_branch_region: missing.iter().map(|s| s.to_string()).collect(),
            lift: None,
        }
    }

    #[test]
    fn correlation_bug_wording_needs_a_condition_missing_beside_present_ones() {
        use correlate::NotMeasurableReason::NoBranchRegion;
        let bug = "most likely a Floyd correlation bug";
        // Invoked, every condition span has a branch region: the
        // decision was never evaluated; not blamed on Floyd.
        let text = all_unobserved_explanation(&[cand_summary("f", true, None, &[])]).join("\n");
        assert!(!text.contains(bug), "{text}");
        assert!(text.contains("no test that entered the function evaluated the decision"));
        // No branch region for the decision at all: not measurable,
        // named by reason.
        let mut c = cand_summary("g", false, Some(NoBranchRegion), &["x"]);
        c.note = Some("may be exercised by `wpt`".to_string());
        let text = all_unobserved_explanation(&[c]).join("\n");
        assert!(!text.contains(bug), "{text}");
        assert!(text.contains("(no_branch_region: 1)"), "{text}");
        assert!(
            text.contains("    `g` (src/lib.rs:1): may be exercised by `wpt`"),
            "{text}"
        );
        // One condition span missing from a record that has branch
        // regions for the others: the only correlation-bug case.
        let text = all_unobserved_explanation(&[cand_summary("h", true, None, &["b"])]).join("\n");
        assert!(text.contains(bug), "{text}");
        assert!(
            text.contains("`h` (src/lib.rs:1): no branch region for `b`"),
            "{text}"
        );
    }

    #[test]
    fn harness_free_note_names_the_skipped_targets_of_the_package() {
        use correlate::{CoverageStatus, NotMeasurable, NotMeasurableReason};
        let skipped = vec![
            NoHarnessTarget {
                package: "data-url".to_string(),
                target: "wpt".to_string(),
                binary: "target/debug/deps/wpt-1".to_string(),
            },
            NoHarnessTarget {
                package: "idna".to_string(),
                target: "bench".to_string(),
                binary: "target/debug/deps/bench-1".to_string(),
            },
        ];
        let no_branch = |entered| {
            coverage(
                CoverageStatus::NotMeasurable(NotMeasurable {
                    reason: NotMeasurableReason::NoBranchRegion,
                    detail: String::new(),
                }),
                entered,
            )
        };
        let note = harness_free_note(&no_branch(false), "data-url", &skipped).expect("a note");
        assert!(note.contains("`wpt`"), "{note}");
        assert!(note.contains("`data-url`"), "{note}");
        assert_eq!(
            package_display_name("path+file:///w/rust-url/data-url#0.3.2"),
            "data-url"
        );
        assert_eq!(
            package_display_name(
                "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"
            ),
            "serde"
        );
        assert_eq!(package_display_name("foo 0.1.0 (path+file:///w)"), "foo");
        assert_eq!(package_display_name("data-url"), "data-url");
        assert!(!note.contains("bench"), "{note}");
        let never = coverage(CoverageStatus::NeverInvoked, false);
        assert!(harness_free_note(&never, "data-url", &skipped).is_some());
        // Entered by a test Floyd ran, another package, or a reason
        // unrelated to whether it ran: no note.
        assert_eq!(
            harness_free_note(&no_branch(true), "data-url", &skipped),
            None
        );
        assert_eq!(harness_free_note(&never, "url", &skipped), None);
        let invoked = coverage(CoverageStatus::Invoked, true);
        assert_eq!(harness_free_note(&invoked, "data-url", &skipped), None);
        let uninstrumented = coverage(
            CoverageStatus::NotMeasurable(NotMeasurable {
                reason: NotMeasurableReason::Uninstrumented,
                detail: String::new(),
            }),
            false,
        );
        assert_eq!(
            harness_free_note(&uninstrumented, "data-url", &skipped),
            None
        );
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
            summary: CoverageSummary::new(None, 0),
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
        assert_eq!(format_ambiguous_tests(&many, &[]), "t0, t1, t2 (+2 more)");
    }

    fn ambiguous_analysis(ambiguous_tests: usize) -> RuntimeAnalysis {
        let tree = floyd::DecisionTree {
            // a && b
            decisions: vec![decision::Node::Ite {
                cond: "a".to_string(),
                then_branch: Box::new(decision::Node::Ite {
                    cond: "b".to_string(),
                    then_branch: Box::new(decision::Node::Const { value: true }),
                    else_branch: Box::new(decision::Node::Const { value: false }),
                }),
                else_branch: Box::new(decision::Node::Const { value: false }),
            }],
        };
        let vec_obs = |name: &str, a: bool, b: Option<bool>, result: bool| {
            let mut inputs: std::collections::BTreeMap<String, bool> =
                [("a".to_string(), a)].into_iter().collect();
            if let Some(b) = b {
                inputs.insert("b".to_string(), b);
            }
            ConditionObservation::vector(Some(name.to_string()), inputs, result)
        };
        // `a` is demonstrated by (F,-)->F vs (T,T)->T; `b`'s only
        // other evidence, (T,F)->F, comes from ambiguous tests.
        let mut observations = vec![
            vec_obs("a_false", false, None, false),
            vec_obs("both_true", true, Some(true), true),
        ];
        for i in 0..ambiguous_tests {
            observations.push(ConditionObservation::ambiguous(
                Some(format!("loop_{i}")),
                2,
                [
                    (
                        "a".to_string(),
                        floyd::BranchCounts {
                            true_count: 2,
                            false_count: 0,
                        },
                    ),
                    (
                        "b".to_string(),
                        floyd::BranchCounts {
                            true_count: 1,
                            false_count: 1,
                        },
                    ),
                ]
                .into_iter()
                .collect(),
            ));
        }
        masking::analyze_with_runtime(&tree, &observations, Variant::Masking)
    }

    #[test]
    fn junit_caps_ambiguous_tests_and_withholds_the_percentage() {
        let analysis = ambiguous_analysis(5);
        assert!(matches!(
            analysis.condition_status.get("b"),
            Some(ConditionStatus::Ambiguous { tests }) if tests.len() == 5
        ));
        let xml = render_runtime_junit("lib.rs", &analysis, 2);
        assert!(xml.contains(r#"type="AmbiguousCondition""#), "{xml}");
        assert!(xml.contains("(loop_0, loop_1, loop_2 (+2 more))"), "{xml}");
        assert!(!xml.contains("loop_3"), "{xml}");
        assert!(xml.contains(r#"<testsuite name="lib.rs" tests="2" failures="1">"#));
        assert!(xml.contains(r#"<property name="floyd.unknown" value="1"/>"#));
        assert!(xml.contains(r#"<property name="floyd.not_measurable" value="2"/>"#));
        assert!(!xml.contains("floyd.percentage"), "{xml}");
        assert!(!xml.contains('%'), "{xml}");
        assert_eq!(
            CoverageSummary::new(Some(&analysis), 2).exit_code(),
            summary::EXIT_MCDC_NOT_DEMONSTRATED
        );
    }

    #[test]
    fn junit_carries_the_percentage_when_nothing_is_unknown() {
        let analysis = ambiguous_analysis(0);
        assert!(matches!(
            analysis.condition_status.get("b"),
            Some(ConditionStatus::Unexercised)
        ));
        let xml = render_runtime_junit("lib.rs", &analysis, 0);
        assert!(xml.contains(r#"type="UnexercisedCondition""#), "{xml}");
        assert!(xml.contains(r#"<property name="floyd.percentage" value="50"/>"#));
        assert!(xml.contains(r#"<property name="floyd.unknown" value="0"/>"#));
    }

    #[test]
    fn json_summary_reports_null_percentage_for_an_analysed_unknown() {
        let analysis = ambiguous_analysis(1);
        let report = SingleRuntimeReport {
            source: "lib.rs".to_string(),
            verdict: RuntimeVerdict::Analysed,
            tests_discovered: &[],
            observations: &[],
            analysis: Some(&analysis),
            summary: CoverageSummary::new(Some(&analysis), 0),
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["summary"]["conditions_total"], 2);
        assert_eq!(v["summary"]["exercised"], 1);
        assert_eq!(v["summary"]["not_exercised"], 0);
        assert_eq!(v["summary"]["unknown"], 1);
        assert!(v["summary"]["percentage"].is_null());
        assert!(v["analysis"]["condition_status"]["b"]["Ambiguous"].is_object());
    }

    /// anyhow `src/ensure.rs:60` (`b == 32 || b == 10`) with
    /// `test_whitespace`'s counters kept as one ambiguous observation.
    fn ruled_out_analysis() -> RuntimeAnalysis {
        let tree = floyd::DecisionTree {
            decisions: vec![decision::Node::Ite {
                cond: "b == 32".to_string(),
                then_branch: Box::new(decision::Node::Const { value: true }),
                else_branch: Box::new(decision::Node::Condition {
                    name: "b == 10".to_string(),
                }),
            }],
        };
        let counts = |t, f| floyd::BranchCounts {
            true_count: t,
            false_count: f,
        };
        let observations = [ConditionObservation::ambiguous(
            Some("test_whitespace".to_string()),
            9,
            [
                ("b == 32".to_string(), counts(1, 8)),
                ("b == 10".to_string(), counts(0, 8)),
            ]
            .into_iter()
            .collect(),
        )];
        masking::analyze_with_runtime(&tree, &observations, Variant::Masking)
    }

    #[test]
    fn unexercised_condition_evaluated_by_an_ambiguous_test_says_why_with_counts() {
        let analysis = ruled_out_analysis();
        assert!(matches!(
            analysis.condition_status.get("b == 10"),
            Some(ConditionStatus::Unexercised)
        ));
        let note = ruled_out_note(&analysis, "b == 10").expect("note");
        assert!(
            note.contains(
                "test_whitespace (9 evaluations) never took b == 10=T (0T/8F) as \
                 (b == 10=T, b == 32=F) -> T needs"
            ),
            "{note}"
        );
        assert!(ruled_out_note(&analysis, "b == 32").is_none());

        let xml = render_runtime_junit("lib.rs", &analysis, 0);
        assert!(xml.contains(r#"type="UnexercisedCondition""#), "{xml}");
        assert!(xml.contains("never took b == 10=T (0T/8F)"), "{xml}");

        let v = serde_json::to_value(&analysis).unwrap();
        let r = &v["ruled_out"]["b == 10"][0];
        assert_eq!(r["test"], "test_whitespace");
        assert_eq!(r["condition"], "b == 10");
        assert_eq!(r["value"], true);
        assert_eq!(r["counts"]["true_count"], 0);
        assert_eq!(r["counts"]["false_count"], 8);
        assert_eq!(v["condition_status"]["b == 10"], "Unexercised");
    }

    #[test]
    fn unknown_status_line_says_unknown_and_caps_tests() {
        let tests: Vec<String> = (0..4).map(|i| format!("loop_{i}")).collect();
        let line = unknown_reason(&tests, &[]);
        assert!(
            line.starts_with(
                "UNKNOWN — the outcomes an independence pair still needs were taken only in 4 \
                 test(s)"
            ),
            "{line}"
        );
        assert!(line.ends_with("loop_0, loop_1, loop_2 (+1 more)"), "{line}");
        assert!(!line.contains("UNEXERCISED"));
    }

    fn partial_for_tests<'a>(
        tests: &'a [String],
        skipped: &'a [(std::path::PathBuf, u64)],
        no_harness: &'a [NoHarnessTarget],
    ) -> Partial<'a> {
        Partial {
            manifest: Path::new("/w/Cargo.toml"),
            workspace_root: Some(Path::new("/w")),
            test_targets: 2,
            tests_discovered: tests,
            out_of_scope: &[],
            skipped_oversize_mirs: skipped,
            skipped_no_harness: no_harness,
            max_mir_mb: 50,
        }
    }

    /// Each error verdict serialises with `error`, a null percentage
    /// and the partial data collected before the failure.
    #[test]
    fn error_reports_carry_verdict_error_and_partial_data() {
        let tests = vec!["t1".to_string()];
        let skipped = vec![(std::path::PathBuf::from("/w/t/big.mir"), 60 * 1024 * 1024)];
        let no_harness = vec![NoHarnessTarget {
            package: "p".into(),
            target: "bench".into(),
            binary: "/w/t/bench".into(),
        }];
        let partial = partial_for_tests(&tests, &skipped, &no_harness);
        for (verdict, name) in [
            (RuntimeVerdict::BuildFailed, "build_failed"),
            (RuntimeVerdict::SetupFailed, "setup_failed"),
            (RuntimeVerdict::TestDiscoveryFailed, "test_discovery_failed"),
            (RuntimeVerdict::TestRunFailed, "test_run_failed"),
        ] {
            assert_eq!(verdict.as_str(), name);
            let error = failure::RunError {
                stage: "list_tests".into(),
                message: "list_tests: binary exited with status 3".into(),
                ..Default::default()
            };
            let entries = skipped_mir_entries(&skipped);
            let v =
                serde_json::to_value(error_report(&partial, verdict, &error, &entries)).unwrap();
            assert_eq!(v["verdict"], name);
            assert!(v["summary"]["percentage"].is_null());
            assert_eq!(v["error"]["stage"], "list_tests");
            assert_eq!(
                v["error"]["message"],
                "list_tests: binary exited with status 3"
            );
            assert!(v["error"]["ice"].is_null());
            assert_eq!(v["tests_discovered"][0], "t1");
            assert_eq!(v["skipped_oversize_mirs"][0]["path"], "/w/t/big.mir");
            assert_eq!(v["skipped_no_harness"][0]["target"], "bench");
            assert_eq!(v["workspace_root"], "/w");
            assert_eq!(v["test_targets"], 2);
            assert!(v["analysis"].is_null());
        }
    }

    #[test]
    fn oversize_skip_is_named_with_the_limit_and_the_flag() {
        let skipped = vec![(std::path::PathBuf::from("/t/fixed-0dd5.mir"), 116_578_721)];
        assert_eq!(suggested_mir_limit(&skipped), 150);
        assert_eq!(
            suggested_mir_limit(&[(std::path::PathBuf::new(), 100 * 1024 * 1024)]),
            150
        );
        let mut out = Vec::new();
        print_oversize_notice(&mut out, &skipped, 50);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("--max-mir-size limit of 50 MB"), "{out}");
        assert!(out.contains("/t/fixed-0dd5.mir  (111 MB)"), "{out}");
        assert!(out.contains("--max-mir-size=150"), "{out}");

        let line = no_candidates_line(&skipped, 1, 50);
        assert!(line.starts_with(
            "cargo-floyd: no recognised decisions: the MIR of all 1 test target(s) \
             exceeded --max-mir-size=50 (MB)"
        ));
        assert!(!line.contains("In the analysed targets"), "{line}");
        let some = no_candidates_line(&skipped, 3, 50);
        assert!(some.contains("1 of 3 test target(s)"));
        assert!(some.contains("In the analysed targets: The engine"));
        assert!(!no_candidates_line(&[], 3, 50).contains("max-mir-size"));

        let mut out = Vec::new();
        print_out_of_scope_report(&mut out, &[] as &[DeclinedFunction], &skipped, 80, &[]);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("above the 80 MB scalability threshold"),
            "{out}"
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
