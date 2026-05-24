# Corpus v0 — Synthetic Decision Patterns

Phase 0 cut. Hand-crafted patterns that exercise the Phase 0 engine
hardness: short-circuit boolean evaluation, `if let`, basic `match`,
match guards, and `?`. No real-crate excerpts at this stage —
those land in v1 alongside the Phase 1 release.

## Scope

- **In scope**: `&&`, `||`, `!`, nested combinations, `if let`,
  simple `match` with literal patterns, `match` guards, `?` on
  `Option` / `Result`.
- **Out of scope (Phase 2 hardness)**: pattern matching on
  structs/enums with bindings, async desugaring, macro expansion
  provenance, closures.

The full pattern-matching MC/DC story is an acknowledged open problem
in the field — Phase 0 only commits to the simple cases.

## Index

| Pattern | Decision shape | Notes |
|---------|----------------|-------|
| [001-simple-and](patterns/001-simple-and/)       | `a && b`         | Canonical two-condition AND |
| [002-simple-or](patterns/002-simple-or/)         | `a \|\| b`         | Canonical two-condition OR (mirror of 001) |
| [003-nested-and-or](patterns/003-nested-and-or/) | `(a && b) \|\| c`  | First nested pattern; rustc flattens both operators into one CFG of two switchInts |
| [004-not-and](patterns/004-not-and/)             | `!a && b`        | Negated AND; rustc folds `!` into switchInt arm swap (no Not variant needed) |
