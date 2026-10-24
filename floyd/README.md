# floyd

`floyd` is the engine library for
[Floyd](https://github.com/Gordion-Solutions/Floyd), the open-source
MC/DC (Modified Condition/Decision Coverage) coverage engine for Rust,
focused on automotive safety-critical code (ISO 26262, ASIL-D, TCL3).
It recovers decisions from the compiler's MIR and reports, under
masking MC/DC, which conditions in each decision the test suite
exercises.

Most users want the cargo subcommand rather than this crate directly:

```sh
cargo install cargo-floyd
cargo floyd test
```

See the [main repository README](https://github.com/Gordion-Solutions/Floyd)
for the full feature matrix, qualification stance, and ADRs.
Project page: <https://gordionsolutions.co.uk/software/#floyd>.
