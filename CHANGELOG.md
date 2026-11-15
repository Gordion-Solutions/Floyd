# Changelog

All notable changes to `floyd` and `cargo-floyd` are recorded here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: a minor bump marks a breaking change).

## 0.4.0 - 2026-11-14

### Breaking

- **`floyd` library API.** `DecisionTree` has a new public field
  `condition_order` (struct literals must set it; `DecisionTree::from_function`
  fills it), `ConditionStatus` a new variant `ExercisedUnidentified`,
  `RuledOut` a new field `remainder`, `ConditionObservation` new fields
  `attributed_evaluations` and `decompositions`, `MirFunction` a new
  field `branch_markers`, and `DeclineReason` new variants `NoDecision`
  and `TooManyConditions`. New public modules `floyd::toolchain` and
  `floyd::scope`; new `decision::try_decompose_candidate_detailed`
  (with `Decline`, `MAX_DECISION_CONDITIONS`, `MAX_DECOMPOSE_STEPS`),
  `DecisionTree::from_function` / `conditions`, and
  `correlate::earliest_condition_span_is_std`. Exhaustive matches and
  struct literals need updating; existing function signatures are
  unchanged.
- **`out_of_scope` reasons.** A function with no Boolean decision the
  user wrote is listed as `no_decision` with a `detail` (most were
  `unsupported_shape`); a decision with more than 12 conditions, or
  past the decomposer's step budget, as `too_many_conditions` with
  `conditions`. CTFE bodies of `const fn`s are no longer listed (no
  more `ctfe_body` entries): they are counted in
  `out_of_scope_summary.ctfe_bodies_skipped`, and the list has one
  entry per function. No JSON field was renamed or removed.

### Added

- `YIELD.md`: per-crate yield table (functions with a decision, recovered, declined by reason,
  not measurable by reason, analysed, condition statuses) rendered by `tools/yield/render_yield.py`
  from a pinned sweep of 19 public crates; `tools/yield/sweep.sh` and `crates.tsv` reproduce the sweep.
- **`--toolchain=<name>|env|path:<dir>`** (or `FLOYD_TOOLCHAIN`;
  default `nightly`, the previous behaviour). `<name>` is a rustup
  toolchain (`cargo +<name>`); `env` uses `$CARGO` / `$RUSTC`, else
  `cargo` / `rustc` on `PATH` (a compiler outside rustup, such as
  Ferrocene; untested); `path:<dir>` uses `<dir>/cargo` and
  `<dir>/rustc`. On a stable or beta compiler Floyd prints one line to
  stderr and sets `RUSTC_BOOTSTRAP=1` for the instrumented `rustc`
  only (through `floyd-rustc-shim`), never for cargo, build scripts or
  the tests. `llvm-profdata` / `llvm-cov` come from the chosen
  toolchain's sysroot. The `fixtures/mcdc-cases` verdicts are identical
  on stable 1.99.1 and 1.100 and on nightly 1.101 and 1.102, with or
  without `rust-src`.
- **Toolchain in every report:** a `Toolchain:` line in the text
  header, a JSON `toolchain` object (`name`, `version`, `release`,
  `host`, `channel`, `rustc_bootstrap`; `null` when the toolchain
  could not be probed, verdict `setup_failed`, `error.stage`
  `toolchain`) and JUnit `floyd.toolchain*` properties. Library:
  `floyd::toolchain` and `*_with_toolchain` variants of the build and
  run functions in `instrument` and `runner`.
- **Flow decomposition for multi-evaluation tests.** A test that
  evaluated a non-tree decision several times was always ambiguous.
  Floyd now solves the test's branch counters as linear equations on
  how many evaluations took each path through the decision, enumerates
  every non-negative integer solution, and credits a condition vector
  only for a path every solution takes (a unique solution gives exact
  counts; otherwise the least count, with the rest an ambiguous
  remainder). Observations carry `"resolved_by": "flow_decomposition"`;
  the text marks lower bounds `at least N evaluations`; the remainder
  carries `attributed_evaluations` and `decompositions`, and a
  `ruled_out` entry computed from it `"remainder": true`.
- **Exercised without an identifiable pair.** When every decomposition
  of one test, with all condition vectors, completes some independence
  pair for a condition (not the same one), the condition is EXERCISED:
  JSON `ExercisedUnidentified` (`pair_candidates`, `pair_identifiable:
  false`, `test`, `decompositions`), text `EXERCISED via one of: ...`,
  JUnit a passing testcase with `floyd.pair_identifiable=false`. It
  counts as exercised in the summary, percentage and exit code. chrono
  0.4 `NaiveTime::from_hms_nano_opt` goes from 2 of 6 exercised (exit 2)
  to 6 of 6 (exit 0).
- **`no_decision` classification** (`floyd::scope`), with `detail`
  `std_macro_only`, `pattern_match_only`, `derived_impl`,
  `question_mark_or_loop_only`, `assert_only` or `drop_flags_only`,
  assigned only on positive evidence for every `switchInt` (rustc's
  `coverage branch` markers, switch and discriminant spans, drop-flag
  writes, `#[derive]` / `assert!` in the source); anything else keeps
  its decline reason. 249 of 255 hand-labelled sweep functions agree
  (precision 100%, recall 95%).
- **`out_of_scope_summary`** in the JSON (`total`, `by_reason`,
  `no_decision` by detail, `ctfe_bodies_skipped`), `floyd.out_of_scope.*`
  and `floyd.ctfe_bodies_skipped` JUnit properties, and out-of-scope
  counts by reason in the text report.

### Changed

- **Conditions are listed in source order** (line, then column;
  conditions without a known span follow, by name) everywhere: the
  `Conditions:` line, observations, pair and missing rows, JSON
  `conditions` and truth table, JUnit testcases. They were sorted by
  name (anyhow's `b == b' ' || b == b'\n'` printed `b == 10, b == 32`).
  Verdicts are unchanged.
- **UNKNOWN wording**: "the rows an independence pair still needs are
  possible only in N test(s) that evaluated the decision more than
  once, whose counters do not attribute those rows to condition
  vectors", naming what a partly resolved test attributed; the JUnit
  failure message uses the same text.

### Fixed

- **User `matches!` predicates were silently dropped.** A decision
  whose condition spans lie in `core`'s `matches!` arms (rust-url
  `Url::has_host`, `SchemeType::is_special`; time
  `Context::is_component`) was dropped from both `candidates` and
  `out_of_scope` by a filter meant for `assert_eq!` bodies (20
  functions in the validation sweep). Every function with a
  `switchInt` is now exactly one of: a candidate, an `out_of_scope`
  entry, a counted CTFE body, or left out by `--only`.
  `floyd::correlate::provenance_for` locates such a decision at the
  user's call, not in `core`.
- **`not_measurable` reason depended on the `rust-src` component.**
  A decision whose conditions all expand from `core` (a user
  `matches!`) is `external_macro`. rustc prints those spans as the
  toolchain's `lib/rustlib/src/rust/library/..` when `rust-src` is
  installed and as `/rustc/<hash>/library/..` when it is not, and the
  check read the normalised decision spans, which leave `/rustc/` spans
  out: without `rust-src` (as on CI) the span list was empty, the check
  never fired, and the same decision was reported `no_branch_region`.
  The check now reads the condition spans as rustc printed them, so the
  reason is `external_macro` with or without `rust-src`, on any
  toolchain.
- **Exponential decomposition.** rust-url's 40-alternative
  `is_url_code_point` took minutes and over a gigabyte of memory to
  decompose; it is now declined as `too_many_conditions` in
  milliseconds.

## 0.3.0 - 2026-11-09

### Breaking

- **`cargo floyd test` exit codes.** The exit code used to be 0
  whenever the analysis ran, whatever the verdict. It is now:
  - `0`: a decision was analysed and every condition was exercised;
  - `1`: error, or no decision was analysed (build, test discovery
    or test run failure, missing llvm-tools, no recognised decision,
    no usable observation);
  - `2`: a decision was analysed but MC/DC was not demonstrated (a
    condition is unexercised or unknown).

  A CI step running `cargo floyd test` now fails on incomplete MC/DC
  in every output format. Not-measurable candidates do not affect the
  exit code. Keep `if: always()` / `when: always` on report-publishing
  steps.
- **Instrumented build directory.** The instrumented tests are built
  in `<target>/floyd` (for example `target/floyd/`), not in the
  directory `cargo test` uses. New `--target-dir=<dir>` option.
  Expect one full instrumented rebuild on first run; CI caches of
  `target/` cover it.
- **Unknown conditions.** A test that evaluates a decision more than
  once with differing values is an *ambiguous* observation and is no
  longer collapsed into a vector that may never have occurred. A
  condition whose missing pair could only lie in ambiguous tests is
  reported `UNKNOWN`; when any condition is unknown no MC/DC
  percentage is printed and `summary.percentage` is `null`.
- **`floyd` library API.** Public MIR and analysis types gained
  variants and fields, for example `MirStatement::AssignUnaryNot`,
  `MirStatement::AssignCopy { deref, .. }`, `MirTerminator::Assert`
  and `MirTerminator::NoReturnCall`, `ConditionStatus::Ambiguous`.
  New public module `coverage_index`; new re-exports `BranchCounts`,
  `Resolution`, `RuledOut`.

### Added

- `--all-candidates` (with `--format=json`, cargo projects): also
  analyse every measurable candidate and add them as `analyses`; the
  selected decision's report and the exit code are unchanged.
- `--only=<pat>`: analyse only functions whose name contains `<pat>`.
- `--max-mir-size=<MB>` (default 50): skip larger test-target MIR
  files. Skipped files are listed on stderr and in
  `skipped_oversize_mirs` / `max_mir_size_mb`, with the value that
  would admit them.
- `--features`, `-F`, `--all-features`, `--no-default-features`,
  passed to the instrumented build.
- JSON `summary` object (`conditions_total`, `exercised`,
  `not_exercised`, `unknown`, `not_measurable`, `percentage`); the
  same counts as `floyd.*` properties in JUnit.
- JSON and JUnit documents on every exit path. Error exits carry
  `verdict` (`build_failed`, `test_discovery_failed`,
  `test_run_failed`, `setup_failed`) and an `error` object; decline
  paths carry the candidate list and `out_of_scope`.
- `out_of_scope[].reason`: `non_bool_leaf`, `no_conditions`,
  `ctfe_body` or `unsupported_shape`.
- `mir_parse_problems` (JSON, stderr and text report): MIR files that
  could not be read or parsed, or parsed only in part.
- `candidates[].not_measurable` with a reason for decisions coverage
  cannot measure (no coverage record, dependency macro expansion,
  shared spans, no branch region), counted in the summary.
- `skipped_no_harness`: `harness = false` targets are no longer run
  to list tests; a candidate no test entered carries a `note` naming
  the skipped targets of its package.
- Ambiguous observations in text, JSON and JUnit
  (`AmbiguousCondition`), with evaluation counts and raw counters.
  Multi-evaluation tests are resolved into vectors when the counters
  determine them (tree-counter rule, then single-two-sided rule;
  `resolved_by`).
- An unexercised condition names the missing row(s) of its pair
  (`missing_rows`) and, where multi-evaluation tests evaluated it,
  the counter that rules each one out (`ruled_out`).
- Value-arm lift: a condition whose arms write a non-`bool` value
  (`if upper { 7 } else { 39 }`, `Some(..)` / `None`) is analysed
  with the arm taken as the outcome; candidates report `lift`
  (`call_arms` or `value_arms`).
- rustc's diagnostics (or ICE summary) are printed when the
  instrumented build fails.
- Per-test coverage extraction runs in parallel; correlation span
  maps are built once per MIR.
- `demo/restraint-controller`: an end-to-end walkthrough crate.
- README: "Found in the wild" (test gaps in anyhow and ipnet),
  Options, Build directory, Errors, Exit codes and JSON summary
  sections.
- `fixtures/mcdc-cases`, `fixtures/failure-paths`,
  `fixtures/proc-macro-test` and gated end-to-end tests, run in CI.

### Fixed

- Decisions whose body is the returned expression
  (`fn f(a: bool, b: bool) -> bool { a && b }`) were declined: the
  coverage branch counter rustc emits after the terminal leaf is now
  decomposed through. Unary `!` emitted as its own MIR statement under
  coverage instrumentation is decomposed.
- MIR parser: an `alloc` hex dump whose ASCII column contained `{`
  made the parser skip the rest of the file silently; `coroutine
  layout` blocks failed the parse. Both are now skipped correctly.
- MIR parser: fn headers with an arrow in an argument type
  (`impl FnOnce(..) -> T`, `dyn Fn(..) -> T`, `for<'a> fn(..) -> T`)
  are recognised. An unrecognised header no longer drops the whole
  file.
- `assert(..)` terminators (overflow, bounds checks) are parsed;
  checked arithmetic in arms no longer blocks analysis.
- Profiles of child processes spawned by a test were appended to the
  test's own profile and attributed to it. Build scripts of the
  primary package were instrumented and wrote `default_*.profraw`
  into the project; Floyd's `RUSTC_WRAPPER` leaked into tests that
  spawn cargo. `--list` no longer leaves profiles in the working
  directory.
- Test artifacts are sorted, so report order no longer follows
  cargo's job scheduling.
- Multi-evaluation tests: the tree-counter rule is tried before the
  single-two-sided rule.
- A pair is credited only through vectors that evaluated the
  condition; a condition is `UNKNOWN` only if an ambiguous test could
  complete its pair.
- Candidates are joined to coverage by source span, not by name
  (impl methods, trait impls, generics and closures were missed);
  same-span functions with different paths are kept apart.
- Spurious candidates declined: non-`bool` leaves, operand set-up
  writes, assertion guards, drop-flag writes, zero-condition trees,
  boolean passthroughs, `// MIR FOR CTFE` bodies, std macro
  expansions.
- `if let` / `match` condition names are correlated to their
  `switchInt` spans.
- Test binaries get the toolchain library directory on the loader
  path (proc-macro crates' tests failed to start).
- A target directory first built by `cargo +nightly test` no longer
  causes "no test artifacts produced".
- `--format=json` emits JSON when no decision is recognised.

## 0.2.9 and earlier

See the git history and the
[GitHub releases](https://github.com/Gordion-Solutions/Floyd/releases).
