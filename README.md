# Floyd

[![crates.io: floyd](https://img.shields.io/crates/v/floyd.svg?label=floyd)](https://crates.io/crates/floyd)
[![crates.io: cargo-floyd](https://img.shields.io/crates/v/cargo-floyd.svg?label=cargo-floyd)](https://crates.io/crates/cargo-floyd)
[![license: MIT OR Apache-2.0](https://img.shields.io/crates/l/floyd.svg)](#license)

Floyd is an open-source MC/DC (Modified Condition/Decision Coverage)
engine for Rust, focused on automotive safety-critical code
(ISO 26262, ASIL-D, TCL3).

```
$ cargo floyd test
floyd  runtime MC/DC analysis of Cargo.toml

Tests discovered: 4
Observations:     4
Conditions:       channel_a_ok, channel_b_ok, master_enable

Per-condition MC/DC status:
  ✓ master_enable: EXERCISED  via (channel_a_ok=T, channel_b_ok=T, master_enable=F) -> F   vs   (channel_a_ok=T, channel_b_ok=T, master_enable=T) -> T
  ✓ channel_a_ok:  EXERCISED  via (channel_a_ok=F, channel_b_ok=T, master_enable=T) -> F   vs   (channel_a_ok=T, channel_b_ok=T, master_enable=T) -> T
  ✓ channel_b_ok:  EXERCISED  via (channel_a_ok=T, channel_b_ok=F, master_enable=T) -> F   vs   (channel_a_ok=T, channel_b_ok=T, master_enable=T) -> T

MC/DC coverage: 3 of 3 conditions exercised (100%)
```

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
| JSON output | `cargo floyd test --format=json` |
| **JUnit XML output** | `cargo floyd test --format=junit` — one `<testcase>` per condition (pass = exercised, failure = unexercised); rendered natively by Jenkins, GitLab CI, GitHub Actions, Bazel/Buck2, etc. |

### What doesn't (yet)

The current v0.1.0 line targets pure boolean-variable decisions.
Several shapes that appear regularly in real safety code are
declined; the v0.2.0 line (engine-correctness milestone — see
[ADR-0004](architecture/decisions/0004-engine-correctness-oss-boundary.md))
closes the biggest gaps. The current declines:

| Shape | Workaround / status |
|-------|---------------------|
| **Functions composing multiple decisions into a non-bool output** | A bool-returning function with several decisions (`if ... { ... } let r = ...`, `match ... && b`, dispatch with arm-internal decisions, early return + post-return) recovers correctly. A function that *composes* multiple decisions into a tuple, array, or other non-bool output — e.g. `fn dual(a, b, c) -> (bool, bool) { (a && b, a || c) }` — recovers only the first decision; the others are silently dropped. |
| Match guards (`match n { 0 if c => ... }`) | Not in MVP scope. |
| Pattern destructuring beyond a single bound value (`if let Some((a, b)) = ...`) | Not in MVP scope. |
| `async` desugaring, macro-expansion provenance | Not in MVP scope. |

Most bool-returning safety-critical Rust functions recover
correctly. If your code returns boolean tuples or arrays composed
of multiple separate decisions, only the first one comes through
today.

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

### Failing the build on unexercised conditions

Floyd's exit code today reports the engine's own success, not
whether every condition was exercised. To fail a CI run on
incomplete MC/DC coverage, post-process the JUnit XML — any
`<failure>` element with `type="UnexercisedCondition"` indicates
a missing pair. The CI-native test-result steps (`reports: junit:`
on GitLab, `junit '...'` on Jenkins, JUnit-aware actions on GitHub)
all surface these as failing tests in their respective UIs.

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

Issues and PRs are welcome — see [architecture/](architecture/) for
the design source of truth before proposing larger changes. New
corpus patterns are the highest-value contribution: open a PR
adding a directory under `corpus/v<N>/patterns/` with
`pattern.toml` (analysis) + `src.rs` (the function under test).
The schema is documented in
[`corpus/README.md`](corpus/README.md).
