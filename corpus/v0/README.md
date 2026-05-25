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
| [005-if-let-some](patterns/005-if-let-some/)     | `if let Some(x) = opt { x } else { false }` | `if let` with a binding; pattern-match becomes a synthetic condition |
| [006-try-with-and](patterns/006-try-with-and/)   | `let x = opt?; Some(x && b)` | `?` operator skip-through: engine looks past `Try::branch` plumbing and recovers the inner `&&` |
| [007-match-int-literal](patterns/007-match-int-literal/) | `match n { 0 => false, _ => true }` | Literal `match` on an integer; engine emits a single condition `n == 0` |
| [008-inline-comparison](patterns/008-inline-comparison/) | `speed > 50 && brake` | Inline-comparison + `&&`; engine recognises the comparison and synthesizes the condition name `speed > 50` |
| [009-enum-match-no-binding](patterns/009-enum-match-no-binding/) | `match mode { Mode::On => ..., Mode::Off => ... }` | Enum dispatch with no bindings; engine synthesizes `mode == Mode::variant_0` from the discriminant index |
