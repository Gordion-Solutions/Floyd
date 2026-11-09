# Changelog

All notable changes to `floyd` and `cargo-floyd` are recorded here.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: a minor bump marks a breaking change).

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
