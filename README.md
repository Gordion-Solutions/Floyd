# Floyd

[![crates.io: floyd](https://img.shields.io/crates/v/floyd.svg?label=floyd)](https://crates.io/crates/floyd)
[![crates.io: cargo-floyd](https://img.shields.io/crates/v/cargo-floyd.svg?label=cargo-floyd)](https://crates.io/crates/cargo-floyd)
[![license: MIT OR Apache-2.0](https://img.shields.io/crates/l/floyd.svg)](#license)

Floyd is an open-source MC/DC (Modified Condition/Decision Coverage)
engine for Rust, focused on automotive safety-critical code
(ISO 26262, ASIL-D, TCL3).

Project page: <https://gordionsolutions.co.uk/software/#floyd>

```
$ cargo floyd test
floyd  runtime MC/DC analysis of Cargo.toml

Tests discovered: 4
Observations:     4 (4 used, 0 ambiguous)
Conditions:       channel_a_ok, channel_b_ok, master_enable

Per-condition MC/DC status:
  ✓ master_enable: EXERCISED  via (channel_a_ok=T, channel_b_ok=T, master_enable=F) -> F   vs   (channel_a_ok=T, channel_b_ok=T, master_enable=T) -> T
  ✓ channel_a_ok:  EXERCISED  via (channel_a_ok=F, channel_b_ok=T, master_enable=T) -> F   vs   (channel_a_ok=T, channel_b_ok=T, master_enable=T) -> T
  ✓ channel_b_ok:  EXERCISED  via (channel_a_ok=T, channel_b_ok=F, master_enable=T) -> F   vs   (channel_a_ok=T, channel_b_ok=T, master_enable=T) -> T

MC/DC coverage: 3 of 3 conditions exercised (100%)
```

Each test contributes one observation per decision it evaluates.
An observation is *used* when the test evaluated the decision once
(or its counters otherwise identify the condition vector); a test
that evaluated it several times with differing values is reported
as *ambiguous* and can leave a condition `UNKNOWN` rather than
exercised or unexercised. When any condition is unknown, Floyd
prints no percentage. See
[Tests that evaluate a decision more than once](#tests-that-evaluate-a-decision-more-than-once).

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
| **Closures capturing outer booleans** | `let f = \|x: bool\| x && b; f(a)` — works for by-value (`move \|\|`) and by-reference captures; FnMut closures with internal mutation are out of MVP scope |
| JSON output | `cargo floyd test --format=json` — includes a `summary` object (see [JSON summary](#json-summary)) |
| **JUnit XML output** | `cargo floyd test --format=junit` — one `<testcase>` per condition (pass = exercised, failure = unexercised or unknown); rendered natively by Jenkins, GitLab CI, GitHub Actions, Bazel/Buck2, etc. |

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
  were).

Any other test is reported as an **ambiguous observation**:
`--format=json` marks it `"ambiguous": true` with its `evaluations`
count and raw per-condition `counts`, the text report lists it as
`AMBIGUOUS`, and it is excluded from independence-pair finding. A
condition whose only evidence comes from ambiguous observations is
reported as `UNKNOWN`, not as unexercised, with the tests that
evaluated it (the first three by name, then `(+N more)`; JUnit:
`type="AmbiguousCondition"`).

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
coverage record covers or that several records share). They are
listed with their reason in `candidates[].not_measurable`, are never
part of `conditions_total`, and are reported in the text summary as
a count per reason. The JUnit suite carries the same counts as
`floyd.*` `<properties>`.

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
