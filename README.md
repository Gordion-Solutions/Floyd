# Floyd

[![crates.io: floyd](https://img.shields.io/crates/v/floyd.svg?label=floyd)](https://crates.io/crates/floyd)
[![crates.io: cargo-floyd](https://img.shields.io/crates/v/cargo-floyd.svg?label=cargo-floyd)](https://crates.io/crates/cargo-floyd)
[![license: MIT OR Apache-2.0](https://img.shields.io/crates/l/floyd.svg)](#license)

Floyd is an open-source MC/DC (Modified Condition/Decision Coverage)
engine for Rust, focused on automotive safety-critical code
(ISO 26262, ASIL-D, TCL3).

Project page: <https://gordionsolutions.co.uk/software/#floyd>

```
$ cd demo/restraint-controller && cargo floyd test
floyd  runtime MC/DC analysis of /path/to/floyd/demo/restraint-controller/Cargo.toml

Test targets: 3
Tests discovered: 5
Observations:     5 (5 used, 0 ambiguous)
Conditions:       !seatbelt_fastened, crash_detected, occupant_present, speed_above_threshold

Per-test observations:
  arms_at_high_speed_when_occupant_unbelted_and_no_crash  !seatbelt_fastened=T  crash_detected=F  occupant_present=T  speed_above_threshold=T  result=T
  no_arm_at_high_speed_when_occupant_belted_and_no_crash  !seatbelt_fastened=F  crash_detected=F  occupant_present=T  speed_above_threshold=T  result=F
  arms_when_crash_detected_with_occupant  !seatbelt_fastened=-  crash_detected=T  occupant_present=T  speed_above_threshold=-  result=T
  no_arm_at_low_speed_without_crash  !seatbelt_fastened=-  crash_detected=F  occupant_present=T  speed_above_threshold=F  result=F
  no_arm_without_occupant         !seatbelt_fastened=-  crash_detected=-  occupant_present=F  speed_above_threshold=-  result=F

Per-condition MC/DC status:
  ✓ !seatbelt_fastened: EXERCISED  via (!seatbelt_fastened=F, crash_detected=F, occupant_present=T, speed_above_threshold=T) -> F   vs   (!seatbelt_fastened=T, crash_detected=F, occupant_present=T, speed_above_threshold=T) -> T
  ✓ crash_detected: EXERCISED  via (!seatbelt_fastened=F, crash_detected=F, occupant_present=T, speed_above_threshold=F) -> F   vs   (!seatbelt_fastened=F, crash_detected=T, occupant_present=T, speed_above_threshold=F) -> T
  ✓ occupant_present: EXERCISED  via (!seatbelt_fastened=F, crash_detected=F, occupant_present=F, speed_above_threshold=F) -> F   vs   (!seatbelt_fastened=T, crash_detected=T, occupant_present=T, speed_above_threshold=T) -> T
  ✓ speed_above_threshold: EXERCISED  via (!seatbelt_fastened=T, crash_detected=F, occupant_present=T, speed_above_threshold=F) -> F   vs   (!seatbelt_fastened=T, crash_detected=F, occupant_present=T, speed_above_threshold=T) -> T

MC/DC coverage: 4 of 4 conditions exercised (100%)

Out of scope: 5 function(s) have a decision in a shape the engine's current patterns do not recognise. The engine declined them rather than report a wrong verdict. See --format=json's `out_of_scope` for the full list.
$ echo $?
0
```

Each test contributes one observation per decision it evaluates.
An observation is *used* when the test evaluated the decision once
(or its counters otherwise identify the condition vector); a test
that evaluated it several times with differing values is reported
as *ambiguous* and can leave a condition `UNKNOWN` rather than
exercised or unexercised. When any condition is unknown, Floyd
prints no percentage; the summary line then reads
`MC/DC coverage: N exercised, N not exercised, N unknown of N
conditions ...`. See
[Tests that evaluate a decision more than once](#tests-that-evaluate-a-decision-more-than-once).
The "Out of scope" line lists functions (here the `#[test]` functions'
own `assert!` expansions) whose decisions Floyd declines rather than
analyse; it does not affect the verdict.

The exit code is 0 when every condition is exercised, 2 when a
decision was analysed but MC/DC was not demonstrated (a condition is
unexercised or unknown), and 1 on an error or when no decision was
analysed (see [Exit codes](#exit-codes)).

For an end-to-end walkthrough, see
[`demo/restraint-controller`](demo/restraint-controller/): an airbag
arming decision at 100% MC/DC, then a planted wiring bug that the
remaining tests and 100% line coverage miss, and the two unexercised
conditions Floyd reports for it.

## Install

```sh
# Nightly toolchain + LLVM coverage tools (one-time).
rustup install nightly
rustup component add llvm-tools-preview --toolchain nightly

# Floyd itself.
cargo install cargo-floyd
```

Then, inside any cargo project with `#[test]` functions:

```sh
cargo floyd test
```

Floyd builds the project's tests with `-Cinstrument-coverage
-Zcoverage-options=branch,condition`, runs each test in isolation,
ingests its `profraw` via `llvm-profdata` + `llvm-cov`, and reports
which decisions the test suite exercises under masking MC/DC.

## What works (and what doesn't)

Floyd recovers logical decisions from rustc MIR and analyses them
against the [masking MC/DC variant][cast-10] (the modern qualified
default). The engine is correct on what it claims to recover — and
when it doesn't recognise a decision, it returns *no decision*
rather than wrong output. The [`corpus/`](corpus/) directory pins
the ground-truth analyses Floyd's output is checked against (see
"Qualification stance" below).

### What works

| Shape | Example |
|-------|---------|
| Boolean expressions over named bool values | `a && b`, `a \|\| b`, `!a`, arbitrary nesting |
| Inline comparisons in a decision | `if speed > 50 && brake { ... }`, `code == 1`, `value >= 0 && value <= 100` (Eq, Ne, Lt, Le, Gt, Ge) |
| `if let` with a binding | `if let Some(x) = opt { x && b } else { false }` |
| `?` operator (skip-through to the success path) | `let x = opt?; x && b` |
| Literal `match` on integer scrutinees | `match n { 0 => ..., 1 => ..., _ => ... }` |
| Bool `match` | `match b { true => ..., false => ... }` |
| **Enum `match` without binding** | `match state { State::Idle => ..., State::Running => ... }` — condition names use the rustc discriminant index (e.g. `state == State::variant_0`); use `if let` with a binding to get source variant names instead |
| Boolean derived from a `let`-bound expression | `let fast = speed > 50; fast && brake` |
| **Conditions whose arms are not Boolean** | `let off = if upper { 7 } else { 39 };`, `if -86_400 < secs && secs < 86_400 { Some(..) } else { None }` — the decision is the condition expression, its outcome is the arm taken (see [Non-Boolean arms](#non-boolean-arms)) |
| **Closures capturing outer booleans** | `let f = \|x: bool\| x && b; f(a)` — works for by-value (`move \|\|`) and by-reference captures; FnMut closures with internal mutation are out of MVP scope |
| JSON output | `cargo floyd test --format=json` — includes a `summary` object (see [JSON summary](#json-summary)) |
| **JUnit XML output** | `cargo floyd test --format=junit` — one `<testcase>` per condition (pass = exercised, failure = unexercised or unknown); rendered natively by Jenkins, GitLab CI, GitHub Actions, Bazel/Buck2, etc. |

#### Non-Boolean arms

For MC/DC the decision is the Boolean condition; what its arms
compute does not matter. Floyd lifts a `bool` condition (or a
`&&` / `||` chain of them) whose arms are not Boolean, taking the
arm taken as the decision's outcome (`then` = true, `else` = false),
with the conditions' branch counters as the observations. Two
shapes are lifted, reported per candidate as `lift` in the JSON:

- `call_arms`: both arms of a single condition end in calls that are
  not panics (`if c { f(x) } else { g(x) }`).
- `value_arms`: both arms are straight-line code (no call, branch,
  loop or return; overflow and bounds checks are fine) that writes
  one common
  non-`bool` value, `_0` or a `let`, before the arms join
  (`if upper { 7 } else { 39 }`,
  `if -86_400 < secs && secs < 86_400 { Some(..) } else { None }`).
  Only the function's entry decision is lifted. Declined (as
  `unsupported_shape`, or `non_bool_leaf` when an arm copies a named
  non-`bool` local): an `if` nested in an arm or an `else if`
  (three arms; the inner `if` is a decision of its own), an arm that
  loops, calls, panics or returns, arms writing different locals,
  an `if` without `else`, and a `bool` result (which must decompose
  as a Boolean expression).

Outcome labels follow the branch edges, so for `if a && !b` the
source `else` arm is reported as the `true` outcome; MC/DC
independence pairs do not depend on which outcome is called true.

### What doesn't (yet)

The current v0.1.0 line targets pure boolean-variable decisions.
Several shapes that appear regularly in real safety code are
declined; the v0.2.0 line (engine-correctness milestone — see
[ADR-0004](architecture/decisions/0004-engine-correctness-oss-boundary.md))
closes the biggest gaps. The current declines:

| Shape | Workaround / status |
|-------|---------------------|
| **Functions composing multiple decisions into a non-bool output**, e.g. `fn dual(a, b, c) -> (bool, bool) { (a && b, a || c) }` | The OSS engine recovers only the first decision; the others are silently dropped. Per ADR-0004 this shape falls under the commercial closed-source pattern-matching extensions category — bool-returning multi-decision functions (with `match ... && b`, dispatch with arm-internal decisions, early return + post-return) recover correctly in OSS; the structural rework needed to recover tuple / array / struct compositions sits in the commercial tier. |
| Match guards (`match n { 0 if c => ... }`) | Not in MVP scope. |
| Pattern destructuring beyond a single bound value (`if let Some((a, b)) = ...`) | Not in MVP scope. |
| `async` desugaring, macro-expansion provenance | Not in MVP scope. |
| FnMut closures with internal mutation | The Fn / move-closure capture forms recover; closures that mutate captured state internally (`b = b && x`) produce a different MIR shape that's not yet handled. |

Most bool-returning safety-critical Rust functions recover
correctly. The remaining gaps in the "what doesn't" table are
either explicit Phase 2 deliverables or items that sit in the
commercial tier per ADR-0004 — the OSS engine's surface is the
v0.2.x line as listed in "what works" above.

### Tests that evaluate a decision more than once

Floyd reads per-test evidence from `llvm-cov`'s branch counters,
which count how often each condition was true and how often it was
false, *summed over every evaluation of the decision in that test*.
They do not record which values occurred together. For
`(a && b) || c`, a test that evaluates (T,F,T) and then (F,-,F)
leaves exactly the same counters (`a = 1 T / 1 F`, `b = 0 T / 1 F`,
`c = 1 T / 1 F`) as one that evaluates (T,F,F) and then (F,-,T), so
in general the counters of a multi-evaluation test cannot be
attributed to condition vectors. Floyd does not guess.

The rule is **one evaluation per test**: Floyd derives a condition
vector from a test only when

- the test evaluated the decision exactly once (counted from the
  first condition the decision tests, which runs on every
  evaluation), or
- every condition was only ever true or only ever false in that
  test (every evaluation was then identical), or
- the decision has a single condition (its true and false counters
  each prove that outcome occurred, however many evaluations there
  were), or
- the decision is a **tree** and the counters are consistent with
  it, however many conditions were both true and false. A decision
  is a tree when every condition is reached along one path only:
  `a && b`, `a || b` and `a && (b || c)` are trees; `(a || b) && c`
  is not (`c` is reached both when `a` is true and when `a` is
  false and `b` true), nor is `(a && b) || c`. In a tree each
  condition's true and false counters are the number of
  evaluations that left it along its true and its false edge, and
  every evaluation reaching a condition came along the one edge
  into it, so the counters give, top down from the evaluation
  count, how many evaluations ended at each outcome leaf, and each
  leaf is one condition vector. Floyd checks that every
  condition's true + false equals the count of the edge leading
  into it (the evaluation count for the first condition), that a
  condition with no counter is never reached, that no condition
  appears at two places and no two conditions share a branch
  counter, and that the leaf counts add up to the evaluation count.
  Otherwise the next rule is tried. For example, rust-base64's
  `byte >= 32 && byte <= 126` evaluated 256 times with
  `byte >= 32 = 224 T / 32 F` and `byte <= 126 = 95 T / 129 F` gives
  (T,T) -> T 95 times, (T,F) -> F 129 times and (F,-) -> F 32 times.
  Each such vector's `evaluations` is its own count and it carries
  `"resolved_by": "tree_counters"`.
- the decision is not a tree (or the tree check fails), but exactly
  one condition was both true and false in that test, every
  other condition was only ever true or only ever false, and the
  counters add up exactly. Every other condition then always takes
  its one value, so each evaluation's path through the decision is
  fixed by the two-sided condition's value: the test evaluated
  exactly two condition vectors, one per value of that condition,
  with outcomes computed from the decision. Floyd checks the counts
  before it relies on this: the two-sided condition's total must
  equal the evaluation count, and every condition's counters must
  equal the number of times those two paths test it (a condition
  short-circuited on one path counts only the other path's
  evaluations). Otherwise the test stays ambiguous. For example,
  `(a || b) && c` evaluated 3 times with `a = 2 T / 1 F`,
  `b = 1 T / 0 F` and `c = 3 T / 0 F` gives (T,-,T) -> T and
  (F,T,T) -> T. These vectors carry `"resolved_by":
  "single_two_sided"` in `--format=json`, and each records the
  test's total evaluation count (here 3): the counters fix which
  two vectors occurred, and Floyd does not split the count between
  them. Where both this and the tree rule apply they give the same
  vectors, so Floyd tries the tree rule first for its exact
  per-vector counts.

Any other test, such as one where two or more conditions were both
true and false in a decision that is not a tree, is reported as an
**ambiguous observation**: `--format=json` marks it `"ambiguous": true` with its
`evaluations` count and raw per-condition `counts`, the text report
lists it as `AMBIGUOUS`, and it is excluded from independence-pair
finding.

A pair counts only through vectors that evaluated the condition
under test. Under masking, the truth table also pairs rows where the
condition is short-circuited in one of them (the outcome flips
because other, masked conditions differ); such a row cannot show the
condition's effect, so those pairs are never credited.

A condition that no pair of condition vectors demonstrates is
`UNKNOWN` only if ambiguous tests could have completed one of its
independence pairs: each pair row that no vector covers must be
*possible* in some ambiguous test, meaning that test's counters took
every outcome the row's evaluation path needs (each condition the
row tests has a non-zero counter for the row's value). It is then
reported with the ambiguous tests that could hold the missing rows
(the first three by name, then `(+N more)`; JUnit:
`type="AmbiguousCondition"`). If every pair needs an outcome that no
test took (a zero counter in every ambiguous test that evaluated the
decision, and no vector), the condition is provably **UNEXERCISED**,
and the report says why with the counters, for example
`test_whitespace (9 evaluations) never took b == 10=T (0T/8F)`
(text and JUnit; `ruled_out` in the JSON analysis).

An independence pair needs both of its rows, so for an unexercised
condition the report names the rows still missing from one pair
whose two rows both evaluate the condition (the pair with the fewest
missing rows), and says when the other row is already covered:

```text
  ✗ b: UNEXERCISED — needs a test where (a=T, b=F) -> F [the matching row (a=T, b=T) -> T is already covered]
  ✗ b: UNEXERCISED — needs tests for both (a=T, b=F) -> F and (a=T, b=T) -> T
```

The JUnit failure message says the same; the JSON analysis lists the
missing rows per condition under `missing_rows`.

Floyd does not state a percentage it cannot stand behind. With no
unknown conditions the summary line is unchanged
(`MC/DC coverage: 2 of 3 conditions exercised (67%)`); with any
unknown condition it gives counts only:

```
MC/DC coverage: 1 exercised, 0 not exercised, 1 unknown of 2 conditions (no percentage: 1 condition could not be determined)
```

The JSON `summary.percentage` is then `null`, and the JUnit suite
omits its `floyd.percentage` property.

**Unknown conditions block a pass.** The run did not demonstrate
MC/DC for them, so they produce the same non-zero exit code as
unexercised conditions (see [Exit codes](#exit-codes)). To resolve
an unknown condition, write a test that evaluates the decision
exactly once with the inputs you want to demonstrate, for example
one `#[test]` per input vector instead of a loop over all of them.

[cast-10]: https://www.faa.gov/aircraft/air_cert/design_approvals/air_software/cast/cast_papers (CAST-10 — Modified Condition/Decision Coverage)

## Using Floyd in CI

Floyd's `--format=junit` emits JUnit XML, which every CI in
regulated industries renders natively. The snippets below are
copy-paste starting points; adapt to your project's existing
toolchain conventions. They are documentation, not packaged
integrations — anyone using them is doing the integration
themselves.

### GitHub Actions

```yaml
name: MC/DC coverage

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  floyd:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
        with:
          components: llvm-tools-preview
      - run: cargo install cargo-floyd
      - name: Run Floyd
        run: cargo floyd test --format=junit > floyd-mcdc.xml
      - name: Upload report
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: floyd-mcdc-report
          path: floyd-mcdc.xml
```

### GitLab CI

```yaml
floyd-mcdc:
  image: rustlang/rust:nightly
  before_script:
    - rustup component add llvm-tools-preview
    - cargo install cargo-floyd
  script:
    - cargo floyd test --format=junit > floyd-mcdc.xml
  artifacts:
    when: always
    reports:
      junit: floyd-mcdc.xml
```

GitLab's native `reports: junit:` field surfaces the per-condition
results directly in the merge-request UI; unexercised conditions
appear alongside failing tests.

### Jenkins and Buildkite

Both ingest JUnit XML out of the box. In a Jenkins declarative
pipeline, run `cargo floyd test --format=junit > floyd-mcdc.xml`
in a stage and add `junit 'floyd-mcdc.xml'` in `post { always
{ ... } }`. In Buildkite, run the same command and use the
`junit-annotate` plugin (or call `buildkite-agent artifact
upload floyd-mcdc.xml`).

### Options

| Option | Effect |
|--------|--------|
| `--format=text\|json\|junit` | Output format (default `text`). |
| `--only=<pat>` | Analyse only functions whose name contains `<pat>` (repeatable). |
| `--max-mir-size=<MB>` | Skip test-target MIR files larger than `<MB>` megabytes (default 50). Skipped files are listed on stderr with their sizes and in the JSON `skipped_oversize_mirs`; when they are why no decision was found, the `no_candidates` message says so. Raise it for crates with heavy monomorphisation (for example `fixed` needs `--max-mir-size=200`). |
| `--features=<list>`, `-F <list>` | Enable cargo features for the instrumented `cargo build --tests` (repeatable; comma- or space-separated, as for cargo). |
| `--all-features`, `--no-default-features` | Passed to the instrumented build, as for `cargo test`. Crates whose test suite expects `cargo test --all-features` (such as `time`) need it. |
| `--target-dir=<dir>` | The project's target directory, as for cargo (default: `CARGO_TARGET_DIR`, `build.target-dir`, else `target/`). The instrumented build goes into `<dir>/floyd`. |

### Build directory

Floyd builds the instrumented tests in its own directory,
`<target>/floyd` (for example `target/floyd/`), never in the
directory `cargo test` uses. Cargo does not see the instrumentation
Floyd adds per crate as a change, so in a shared directory it would
reuse plain test binaries left by `cargo +nightly test`; a separate
directory also keeps Floyd's build and yours from invalidating each
other. A second `cargo floyd test` reuses the instrumented build.
`target/floyd` sits inside `target/`, so an ignore rule for `target/`
already covers it; to speed up CI, cache it with the rest of
`target/` (or add `target/floyd` if you cache only part of it).
`-Zcoverage-options` and `-Zmir-include-spans` go through cargo's
rustflags (appended to `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` when
you set one, otherwise to `build.rustflags`), so cargo rebuilds when
they change.

### Errors

When the run stops before an analysis, the exit code is 1 and every
format still reports why:

- **Build failure.** rustc's error diagnostics (which Floyd receives
  as cargo JSON messages) are printed to stderr, followed by a
  one-line summary naming the failed units. If rustc crashed, its
  internal-compiler-error lines and the path of the `rustc-ice-*.txt`
  report are printed instead.
- **Test discovery or a test run failing** (`<binary> --list`, or a
  test producing no coverage data): the binary's stderr is printed.

`--format=json` prints the usual report with `verdict` set to
`build_failed`, `test_discovery_failed`, `test_run_failed` or
`setup_failed` (llvm-tools missing), `summary.percentage` `null`, an
`error` object (`stage`, `message`, `diagnostics`, `ice`, `stderr`),
and whatever was collected before the failure (`tests_discovered`,
`out_of_scope`, `skipped_no_harness`, `skipped_oversize_mirs`,
`mir_parse_problems`).
`error` is `null` on the other verdicts. `--format=junit` prints a
document with one `<testcase>` carrying an `<error>` whose `type` is
the verdict.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | A decision was analysed and every condition was exercised. |
| 1 | Error, or no decision was analysed (no recognised decision, or no test produced a usable observation). |
| 2 | A decision was analysed but MC/DC was not demonstrated: at least one condition is unexercised or unknown. |

So `cargo floyd test` fails a CI step on incomplete MC/DC coverage
in every format. Unknown conditions count as failures: the run did
not demonstrate MC/DC for them (see [Tests that evaluate a decision
more than once](#tests-that-evaluate-a-decision-more-than-once)).
Not-measurable candidates (below) do not affect the exit code. In
the JUnit XML, `type="UnexercisedCondition"` marks a missing pair
and `type="AmbiguousCondition"` an unknown condition; the CI-native
test-result steps (`reports: junit:` on GitLab, `junit '...'` on
Jenkins, JUnit-aware actions on GitHub) surface both as failing
tests. To publish the report even when the step fails, keep the
`if: always()` / `when: always` settings shown above.

### JSON summary

`--format=json` carries a `summary` object alongside the existing
fields:

```json
"summary": {
  "conditions_total": 2,
  "exercised": 1,
  "not_exercised": 0,
  "unknown": 1,
  "not_measurable": 0,
  "percentage": null
}
```

`conditions_total` counts the conditions of the analysed decision.
`percentage` is `exercised / conditions_total * 100`, and is `null`
whenever `unknown > 0` (or there are no conditions, on the decline
paths). `not_measurable` counts candidates whose execution coverage
cannot measure (code rustc does not instrument, such as derive
output; decisions expanded from a dependency's macro; spans no
coverage record covers or that several records share; decisions
whose record has no branch region at their conditions, such as a
`match` on an enum or a `matches!` pattern, for which rustc emits
no branch counters). They are
listed with their reason in `candidates[].not_measurable`, are never
part of `conditions_total`, and are reported in the text summary as
a count per reason. The JUnit suite carries the same counts as
`floyd.*` `<properties>`.

Floyd does not run test targets built with `harness = false` (they
are listed in `skipped_no_harness`). When no test Floyd ran entered
a candidate's function and its package has such a target, the
candidate carries a `note` naming the skipped target(s), which may be
what exercises it; the text explanation of an all-unobserved verdict
prints the same note.

A test-target MIR file Floyd could not read, could not parse, or
parsed only in part (a function header its MIR grammar does not
recognise, or an item whose closing brace it never found) is listed
in `mir_parse_problems` (`path`, `problem`) and on stderr. Functions
in the unparsed part are neither candidates nor `out_of_scope`.

Functions that have a decision the engine declined are listed in
`out_of_scope`, each with a `reason`: `non_bool_leaf` when an arm's
value is a named local that is not `bool` (for example a branch that
returns a struct or an integer, which is not a Boolean condition)
and the arms are not a lifted shape (see
[Non-Boolean arms](#non-boolean-arms)),
`no_conditions` when the recovered tree is a bare constant that
tests no condition, `ctfe_body` for the `// MIR FOR CTFE` rendering
of a `const fn` (never run at runtime; the function's runtime body
is considered instead), otherwise `unsupported_shape`. A candidate
whose outcome is the arm taken carries `lift` (`call_arms` or
`value_arms`).

## Why MC/DC, why this engine

DO-178C, ISO 26262, and IEC 61508 require MC/DC analysis at the
highest safety levels. Until recently `rustc` shipped partial MC/DC
instrumentation behind `-Zcoverage-options=mcdc`, but that
implementation was removed in August 2025 (PR #144999). No
cargo-native open-source MC/DC tool exists at the time of writing.
Floyd fills that gap.

The closest neighbouring tool is AdaCore's [gnatcov][gnatcov],
which is qualified for DO-178C Level A and ISO 26262 ASIL-D — but
only on Ada and SPARK, and via the AdaCore toolchain. Floyd takes
the qualification-by-validation route on rustc's existing
instrumentation, with the differential corpus in this repo as the
load-bearing evidence artifact.

[gnatcov]: https://www.adacore.com/gnatpro/toolsuite/gnatcoverage

## Qualification stance

Floyd's qualification target is ISO 26262 TCL3 via the
qualification-by-validation path. The
[`corpus/`](corpus/) directory pins every decision shape Floyd
claims to handle, alongside the hand-computed MC/DC analysis a
qualified reviewer would produce. CI re-runs the corpus on every
commit; any drift between engine output and pinned ground truth
fails the build. This is the substrate auditors use to trust the
engine without reading its source.

See
[ADR-0001](architecture/decisions/0001-external-engine-automotive-first.md)
for the architectural commitment (external engine, no rustc fork,
automotive-first scope) and
[ADR-0002](architecture/decisions/0002-runtime-pipeline.md) for the
runtime pipeline design.

## Floyd Enterprise

The open-source engine is complete as a coverage tool. Floyd
Enterprise, from Gordion Solutions, is what a safety team needs to
put it into an ISO 26262 evidence package: a tool qualification kit
for ISO 26262-8 clause 11, delivered per Floyd release against named
Rust toolchain versions; extended recovery for the decision shapes
the open-source engine declines; assessor-facing reports; and
support with a qualified update channel. Details and enquiries:
<https://gordionsolutions.co.uk/software/#floyd-enterprise>

## Layout

```
.
├── floyd/              the engine (library crate)
├── cargo-floyd/        cargo subcommand driver (binary)
│   └── tests/fixture_cases.rs    gated `cargo floyd test` run on fixtures/mcdc-cases
├── fixtures/mcdc-cases/          regression fixture: one module per engine bug class
├── corpus/             qualification evidence
│   ├── v0/             synthetic decision patterns
│   └── v1/             safety-critical decision patterns
├── architecture/       workflow + per-tool design graphs
│   ├── workflow.toml     top-level pipeline (stages + typed contracts)
│   ├── decisions/        accepted architecture decision records
│   └── tools/            per-tool sub-graphs
└── tests/end_to_end_001.rs    gated full-pipeline integration test
```

## License

Dual-licensed under either:

- MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in this project
shall be dual licensed as above, without any additional terms or
conditions.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for how to add corpus
patterns, file bug reports, and propose engine changes. The
short version: corpus patterns are the highest-value drop-in
contribution; architectural changes that alter Floyd's internal
data model need a design issue first per
[ADR-0004](architecture/decisions/0004-engine-correctness-oss-boundary.md)'s
OSS/commercial scope boundary.
