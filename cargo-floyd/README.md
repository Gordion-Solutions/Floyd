# cargo-floyd

`cargo-floyd` is the cargo subcommand driver for
[Floyd](https://github.com/Gordion-Solutions/Floyd), the open-source
MC/DC coverage engine for Rust. After installation via
`cargo install cargo-floyd`, run inside any cargo project with
`#[test]` functions:

```sh
cargo floyd test            # text report (default)
cargo floyd test --format=json
```

Floyd builds the project's tests with rustc's existing branch +
condition coverage instrumentation, runs each test in isolation,
and reports which decisions the test suite exercises under masking
MC/DC. Per-test evidence comes from aggregate branch counters, so a
test should evaluate each decision once; a test that evaluates it
several times with differing values is reported as ambiguous rather
than guessed at (see [Tests that evaluate a decision more than
once](https://github.com/Gordion-Solutions/Floyd#tests-that-evaluate-a-decision-more-than-once)).
A condition left unknown that way gets no percentage in the summary
and, like an unexercised condition, makes the run exit with code 2
(0: every condition exercised; 1: error or no decision analysed).

Requires a toolchain with the LLVM coverage tools, nightly (the
default) or stable:

```sh
rustup install nightly
rustup component add llvm-tools-preview --toolchain nightly
# or: rustup component add llvm-tools-preview --toolchain stable
#     cargo floyd test --toolchain=stable
```

With a stable or beta compiler Floyd sets `RUSTC_BOOTSTRAP=1` for
the instrumented build only, and records the toolchain in every
report. `--toolchain=env` uses the `cargo`/`rustc` on `PATH` (for
example Ferrocene installed with criticalup; untested).

See the [main repository README](https://github.com/Gordion-Solutions/Floyd)
for the full feature matrix, qualification stance, and ADRs.
Project page: <https://gordionsolutions.co.uk/software/#floyd>.
