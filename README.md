# Floyd

[![crates.io: floyd](https://img.shields.io/crates/v/floyd.svg?label=floyd)](https://crates.io/crates/floyd)
[![crates.io: cargo-floyd](https://img.shields.io/crates/v/cargo-floyd.svg?label=cargo-floyd)](https://crates.io/crates/cargo-floyd)
[![license: MIT OR Apache-2.0](https://img.shields.io/crates/l/floyd.svg)](#license)

Floyd is an open-source MC/DC (Modified Condition/Decision Coverage) engine
for Rust, focused on automotive safety-critical use cases (ISO 26262,
ASIL-D, TCL3).

## Status — Phase 0

🚧 **This repo is currently scaffolding only.** Names are reserved on
crates.io (`floyd`, `cargo-floyd`). The actual engine — MIR/HIR decision
decomposition, masking analysis, condition independence proofs, and the
`cargo floyd` developer workflow — is under active development.

## Architectural commitment

Floyd is an **external** MC/DC engine: it consumes rustc's existing
`-Cinstrument-coverage` + `-Zcoverage-options=branch,condition` output
and performs MC/DC reasoning in its own code. No rustc fork, no required
upstream change. The primary qualification target is **ISO 26262 TCL3**
via the qualification-by-validation path, with the benchmark corpus as
the central evidence artifact. See
[ADR-0001](architecture/decisions/0001-external-engine-automotive-first.md)
for the full rationale and alternatives considered.

## Why MC/DC

DO-178C, ISO 26262, and IEC 61508 require MC/DC analysis at the highest
safety levels. Until recently `rustc` shipped partial MC/DC instrumentation
behind `-Zcoverage-options=mcdc`, but that implementation was removed in
August 2025 (PR #144999). No cargo-native open-source MC/DC tool exists at
the time of writing. Floyd aims to fill that gap and become the canonical
evaluation substrate for Rust MC/DC.

## Layout

```
.
├── floyd/              — the engine (library crate)
├── cargo-floyd/        — `cargo floyd` subcommand driver (binary)
└── architecture/       — workflow + per-tool design graphs
    ├── workflow.toml     Top-level pipeline (stages + typed data contracts)
    ├── tools/            Per-tool sub-graphs (entry fn, internal calls)
    └── types/            Payload type definitions referenced by edges
```

The `architecture/` graph is the design source of truth. As code lands,
the per-tool sub-graphs are checked against the actual `syn`-extracted call
graph in CI — drift between design and implementation fails the build.

## Roadmap

The open-source v0.x line ships the `cargo floyd test` workflow end to
end on Rust source files with `#[test]` functions, including a
real-crate benchmark corpus and a `crates.io` v0.1.0 release. The
in-scope feature set is pinned in
[ADR-0003](architecture/decisions/0003-open-source-vs-commercial-scope.md);
items beyond that are addressed by Floyd's commercial editions, outside
this repository.

## License

Dual-licensed under either:

- MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in this project shall be dual licensed
as above, without any additional terms or conditions.

## Contributing

Floyd is in early scaffolding — issues are welcome, but rapid breaking
changes should be expected. See [architecture/](architecture/) for the
planned design before opening larger PRs.
