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
MC/DC.

Requires the nightly toolchain plus the LLVM coverage tools:

```sh
rustup install nightly
rustup component add llvm-tools-preview --toolchain nightly
```

See the [main repository README](https://github.com/Gordion-Solutions/Floyd)
for the full feature matrix, qualification stance, and ADRs.
