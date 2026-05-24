# Corpus v1 — Safety-Critical Decision Patterns

The v1 cut moves past the synthetic v0 shapes into decision patterns
shaped like the boolean logic Floyd targets in production: actuator
interlocks, alarm hysteresis, authorization gating. Each pattern is
a small, self-contained function with a clearly safety-relevant
story and a hand-computed MC/DC analysis pinned in `pattern.toml`.

The functions are deliberately short. The point of v1 is not to
demonstrate that Floyd handles a 1,000-line state machine — that
belongs further down the corpus roadmap. The point is to demonstrate
that Floyd recognises the *kinds of boolean expressions an automotive
engineer actually writes*, and that the MC/DC analysis it produces
matches the analysis a qualified reviewer would produce by hand.

## Scope

Each v1 pattern exercises one MVP feature against a realistic naming
and framing:

- `&&`, `||`, `!` and nested combinations (the bulk of safety-critical
  boolean logic).
- `if let` with a bound value (e.g. authorization tokens, parser
  results).
- `?` operator skip-through (recover the boolean decision inside a
  fallible function's success path).
- Literal `match` arms on integer scrutinees (command codes, mode
  identifiers).

Out of scope for v1 (Phase 2 deliverables):

- Enum `match` without bindings (the engine declines; engineers
  should use `if let` for binding-based MC/DC on enums).
- Pattern bindings with destructuring, exhaustiveness reasoning,
  match guards, async desugaring.
- Multi-decision functions (the engine recovers the function's
  entry-block decision tree; a function with two separate decisions
  has only the first one reported).

## Index

| Pattern | Shape | Notes |
|---------|-------|-------|
| [001-safety-interlock](patterns/001-safety-interlock/) | `enable && ch_a && ch_b` | Triple-redundant actuator approval — a 3-condition AND |
| [002-alarm-hysteresis](patterns/002-alarm-hysteresis/) | `over_max \|\| (was_alarming && over_threshold)` | Classic alarm hysteresis: latching OR over a guarded threshold |
| [003-command-validation](patterns/003-command-validation/) | `if let Some(payload_ok) = req { payload_ok && armed } else { false }` | `if let` with inner `&&` — guarded payload validation |
