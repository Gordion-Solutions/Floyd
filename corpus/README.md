# Floyd Benchmark Corpus

This directory is **Floyd's qualification evidence**.

Per [ADR-0001](../architecture/decisions/0001-external-engine-automotive-first.md),
Floyd targets ISO 26262 TCL3 via the *qualification-by-validation* path:
Floyd's confidence is established by demonstrating correctness against a
versioned corpus of decision patterns with known-correct MC/DC outputs.
This corpus IS that evidence.

Every pattern here pins:

- **A piece of Rust source** containing one or more logical decisions.
- **The known-correct MC/DC analysis** of those decisions: truth table,
  independence pairs, masking violations (if any).

The Floyd engine, when run against the source, must produce output that
matches the expected analysis. CI runs the full corpus on every commit;
any drift between the engine and the expected output fails the build.

This loop — corpus pins the answer, engine has to reproduce it, CI
enforces it — is what lets external auditors trust the engine without
needing to read its source.

## Layout

```
corpus/
├── v0/                          Synthetic patterns (Phase 0 scope)
│   ├── README.md                v0 scope and pattern index
│   └── patterns/
│       └── <id>-<slug>/
│           ├── pattern.toml     Metadata + expected MC/DC output
│           └── src.rs           The function under analysis
├── v1/                          Real-crate excerpts (Phase 1 scope, not yet)
└── README.md                    (this file)
```

## Versioning

The corpus is versioned as a whole. `v0` is the Phase 0 cut: synthetic
decision patterns only. `v1` adds excerpts from real Rust crates in
safety-relevant domains. Higher versions are reserved for qualification
expansion. **An accepted pattern in any version is immutable** — fixes
and improvements are added as new patterns or in a new corpus version,
never by editing committed ones. This preserves the audit trail.

## Contributing

Open a PR adding a directory under `corpus/v<N>/patterns/`. The CI
`corpus-check` job validates:

1. `pattern.toml` parses against the schema.
2. `src.rs` compiles as a Rust function.
3. The truth table covers every combination of the declared conditions.
4. Each declared independence pair points to two rows of the truth
   table that differ only in the one condition being tested (masking
   MC/DC) or that satisfy the strict unique-cause criterion.

Patterns must be deterministic, free of external I/O, and licensed
under the dual MIT/Apache-2.0 of the project.
