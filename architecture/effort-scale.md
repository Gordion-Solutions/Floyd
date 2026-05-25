# Floyd Effort Scale

A 10-level qualitative scale for describing engineering effort
without committing to specific timing. Used by ADRs, roadmap
discussions, and feature scoping so that "how hard is this?"
gets a sharp answer that ages well and doesn't anchor on a
calendar.

## Scale

| # | Label | Meaning |
|---|-------|---------|
| 1 | **Trivial** | Mechanical, rote — the instructions are the work. |
| 2 | **Easy** | Familiarity with the codebase needed, but no judgment calls. |
| 3 | **Routine** | Competent practitioner, well-trodden pattern, low surprise risk. |
| 4 | **Moderate** | Larger scope than Routine, still no new design needed. |
| 5 | **Substantial** | Design choices needed, multi-component, no novel invention. |
| 6 | **Significant** | Architectural — touches multiple modules, requires upfront design. |
| 7 | **Ambitious** | Invention required, but in known territory; prior art exists to translate from. |
| 8 | **Hard** | Uncharted; the approach itself has to be invented for this codebase. |
| 9 | **Research-grade** | Open problem in the field; published partial work but no clean answer. |
| 10 | **Frontier** | Genuinely unknown if achievable; nobody has published anything that works. |

## Reading the scale

The discrimination axis shifts as you climb:

- **Levels 1–4** vary by **scope**. Same kind of work, more of it.
  A Trivial change at scale becomes Moderate; it doesn't become
  Substantial just because it's big.
- **Levels 5–7** vary by **design and invention weight**.
  Substantial work has design choices but established patterns;
  Significant adds architectural reach; Ambitious adds invention
  steps that translate prior art rather than discover it.
- **Levels 8–10** are dominated by **invention**, not scope.
  Hard work invents an approach; Research-grade tackles a problem
  the field has only partial answers for; Frontier ventures
  beyond what anyone has published.

## When to cite a level

ADRs use the scale wherever effort would otherwise be expressed
in calendar terms ("a few weeks", "a phase"). The scale answer
is more honest because it names the *nature* of the difficulty,
not a guess at the calendar.

Roadmap discussions cite the scale when ordering work: a sequence
of Substantial → Significant → Ambitious work has different risk
characteristics than three Substantial items, even if they sum to
the same calendar estimate.

Scoping discussions cite the scale to decide what belongs in a
given line. The Floyd v0.2.0 line is bounded at Ambitious (level
7). Anything Hard (8) or above belongs in commercial scope or in
a research roadmap, not in the open-source engine.

## Examples (anchored to current Floyd work)

| Level | Example |
|-------|---------|
| 1 Trivial | Fix a README typo. |
| 2 Easy | Add a `--format=text` alias for the default output. |
| 3 Routine | Add a new MIR statement parser for an unrecognised shape. |
| 4 Moderate | Wire a new decomposer handler into the existing pipeline (the `if let` / `?` / literal-match work the v0.1.0 line shipped). |
| 5 Substantial | Comparison-operator recovery (the v0.2.0 line's first item). |
| 6 Significant | Multi-decision functions (architectural — touches `decompose()`, runtime correlation, report rendering). |
| 7 Ambitious | Enum variant-name recovery without bindings (MIR type-metadata work; prior art in rustc internals and `stable_mir`). |
| 8 Hard | Pattern destructuring with multiple bindings — needs a new internal representation; no clean external blueprint for MC/DC over nested patterns. |
| 9 Research-grade | Full pattern-matching MC/DC semantics — acknowledged open problem in the field. |
| 10 Frontier | Provably-complete MC/DC for `async` effect-tracking systems — no published approach that works. |

## Maintenance

The scale's anchor examples drift over time as work ships. When
a level-7 item lands, it stops being the right anchor for level 7
(it's now a worked example, not an unsolved item). The scale
itself is stable; the anchor examples are refreshed alongside
roadmap reviews. Do not edit the level definitions to match a
finished example — promote the next-hardest comparable item to
the anchor slot instead.
