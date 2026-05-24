# cargo-floyd

`cargo-floyd` will be the cargo subcommand driver for [Floyd](https://github.com/Gordion-Solutions/Floyd), the open-source MC/DC coverage engine for Rust. Once installed via `cargo install cargo-floyd`, it will expose a `cargo floyd` workflow for running MC/DC analysis, generating coverage reports (HTML/JSON/SARIF/LCOV), and integrating with CI. This crate currently reserves the name; the implementation is under active development alongside the [`floyd`](https://crates.io/crates/floyd) library.
