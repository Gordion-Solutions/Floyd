# ADR-0004: Engine-correctness scope is open-source; reporting and packaging are commercial

| | |
|---|---|
| Status | Accepted |
| Date | 2026-05-25 |
| Supersedes | [ADR-0003](0003-open-source-vs-commercial-scope.md) |

## Context

ADR-0003 drew the open-source / commercial boundary around a 6-step
MVP — cargo workspace support, runtime mode, `if let`, `?`, simple
`match`, JSON output, real-crate corpus, README, v0.1.0 release.
That MVP shipped as `floyd` and `cargo-floyd` 0.1.0.

While preparing the v0.1.0 release we surfaced a set of
engine-correctness gaps that ADR-0003 had placed in commercial
scope but that turn out to dominate real safety code:

- **Inline comparisons.** `if speed > 50 && brake` produces a
  synthetic MIR bool temporary with no debug name; the engine
  declines. Inline-comparison decisions are the most common shape
  in real ASIL code, so an evaluator pointing Floyd at a real
  component sees "no recognised decisions" on the majority of
  functions. The workaround (`let fast = speed > 50; fast && brake`)
  is a code change Floyd can't ask of the engineer it's trying to
  help.
- **Multi-decision functions.** Floyd reports only the decision
  rooted at a function's entry block. A function with `if a && b`
  followed by `let r = c || d;` returns just the first decision.
  Real automotive functions routinely contain several decisions.
  An engineer seeing the second one silently missing reads it as
  "tool is broken."
- **Enum `match` without a binding.** State machines are pervasive
  in automotive code, and the canonical shape is
  `match state { Idle => ..., Running => ... }` — no binding, so
  the v0.1.0 `if let` handler doesn't apply. The engine declines.

ADR-0003 placed all three in commercial scope on the reasoning that
commercial differentiation requires the open-source release to be
*bounded*. But the open-source release also needs to be
*useful enough to evaluate* — otherwise the adoption funnel doesn't
feed the commercial product at all. The three gaps above sit on the
wrong side of that line: they're not commercial differentiators;
they're prerequisites for credibility.

A second observation from the same review: advanced report
renderings were on the ADR-0003 commercial list, but a closer look
showed the perception gap they're meant to close (i.e. "tool
produces output a CI can consume and a human can read at all") is
closed more cheaply and more honestly by JUnit XML output. JUnit
is the universal test-result interchange format — every CI in
regulated industries already renders it natively — and shipping it
doesn't commit Floyd to the long-tail maintenance richer report
renderings attract. Advanced report rendering stays commercial;
JUnit moves to open-source.

A third disambiguation: CI integration. A short YAML snippet in
the README documenting how to wire `cargo floyd test` into a CI
job is documentation. A packaged, marketplace-listed CI integration
with versioned releases, PR-comment formatter, and inline
annotations is a product. ADR-0003 treated both as one commercial
item; the right split is documentation in the public README,
packaged CI integrations in commercial.

## Decision

Redraw the open-source / commercial boundary so that **engine
correctness on the dominant real-world decision shapes is
open-source**, while **reporting beyond the JSON+JUnit substrate,
packaged CI integrations, and qualification artefacts remain
commercial**. The new v0.2.0 line is the milestone where Floyd
becomes credible on real ASIL code.

### Open-source scope (v0.2.0 line)

Everything currently in v0.1.0, plus the following. Effort levels
reference [the Floyd effort scale](../effort-scale.md).

- **Inline-comparison recovery** *(Substantial)*. `BinaryOp::Eq`,
  `Ne`, `Lt`, `Le`, `Gt`, `Ge` MIR shapes synthesise condition
  names like `speed > 50` so inline comparisons inside boolean
  decisions recover as atomic MC/DC conditions.
- **Multi-decision functions** *(Significant)*. The engine recovers
  every distinct decision in a function (not just the entry-block
  decision), and reports MC/DC per decision site. The runtime
  correlation associates per-test condition observations with the
  correct decision tree.
- **Enum `match` without a binding** *(Ambitious)*. The engine
  recovers state-machine-style match expressions, with reasonable
  condition naming derived from the scrutinee's debug name and
  variant identifiers extractable from the MIR.
- **JUnit XML output** *(Moderate)* via `--format=junit`. Designed
  to be readable by Jenkins, GitLab CI, GitHub Actions, Bazel/Buck2,
  and the other CIs the automotive industry already uses.
- **CI integration documentation** *(Easy)* in the README: a
  self-contained GitHub Actions snippet, a self-contained GitLab
  CI snippet, and a short note on Jenkins / Buildkite. Copy-
  pasteable; anyone using them is doing the integration themselves.
- All v0.1.0 capabilities continue to work and are validated by the
  v0.1.0 corpus on every commit; the engine remains
  decline-rather-than-guess on shapes it cannot recover.

### Commercial scope (explicit, symmetric)

The following capability categories are **not** in the open-source
crate and live in the private `floyd-enterprise` codebase:

- **Report formats beyond text / JSON / JUnit** — alternative
  coverage-report renderings targeting downstream pipelines whose
  customers expect more than the OSS substrate provides.
- **Packaged CI integrations distributed as products** —
  versioned, marketplace-listed CI integrations with maintained
  release cadence and rich in-PR rendering, as distinct from the
  OSS README's copy-pasteable CI snippets.
- **Multi-target and embedded instrumentation** beyond host
  testing.
- **Qualification artefacts** — validation reports, tool
  classifications, and evidence packs for safety-case integration
  in regulated industries.
- **Enterprise reporting** — cryptographically-signed records,
  audit trails, and policy/waiver layers for safety-case
  integration.
- **LTS compiler matrix** — validated combinations of Floyd
  against pinned compiler versions for regulated rollouts.
- **Closed-source pattern-matching extensions** — recovery for
  pattern shapes beyond the OSS engine's surface (the engine
  continues to decline-rather-than-guess on these in the public
  release).

The split is along the axis "engine correctness vs. packaging,
rendering, and qualification artefacts." Code that makes Floyd
*recover more decisions correctly* lives open-source; code that
*packages, renders, integrates, signs, or qualifies* the engine's
output lives commercial.

### What the open-source README will state

The public README's "What works / What doesn't" matrix is the
authoritative public statement of the v0.2.0 line's capabilities
and limitations. The matrix names each gap, the workaround if any,
and the queued resolution. Evaluators reading the README form an
accurate expectation before installing.

## Consequences

### Positive

- **Open-source adoption funnel becomes credible.** Engineers
  pointing Floyd at real ASIL code see decisions recovered on the
  majority of functions, not the minority. The adoption-then-
  monetisation model has a working first stage.
- **Commercial differentiator is sharper and more defensible.**
  Qualification artefacts, signed reports, packaged CI integrations,
  and enterprise-grade reporting are the parts safety-critical
  organisations have budget for. Stripping engine-correctness gaps
  out of the commercial list focuses the commercial pitch on what
  enterprises actually buy.
- **Boundary is symmetric and resists drift.** Both the open-source
  and commercial lists are explicit. Future scope questions
  ("should X be open-source?") can be answered by checking which
  list it appears on, not by inferring from absence.
- **JUnit as the CI substrate trains the market for Floyd as
  integrates-with-what-you-have.** Universal test-result format,
  zero commercial-boundary cost, large hidden positioning benefit.

### Negative

- **Open-source engineering work is larger before commercial
  transition.** The three engine-correctness items sit at
  Substantial, Significant, and Ambitious on the
  [effort scale](../effort-scale.md); commercial work does not
  start until they land. The trade is that commercial work, when
  it starts, has a credible OSS engine to layer on top of.
- **Commercial differentiation surface is narrower.** Some
  capabilities that ADR-0003 reserved as commercial moat
  (engine-correctness extensions) are now open-source. The
  remaining commercial surface (reporting, packaging, qualification
  artefacts) is enough to defend the commercial tier, but the moat
  is narrower than ADR-0003 anticipated.
- **README must stay honest as the v0.2.0 line lands.** Each
  feature shipped against this ADR removes a row from the "what
  doesn't" matrix; the README and the matrix must be updated in
  lockstep with the engine. A stale matrix is worse than no matrix
  because it actively misleads evaluators.

### Neutral

- The corpus continues to grow with every shipped feature: each new
  recovery shape needs at least one synthetic pattern in `corpus/v0/`
  and ideally one safety-critical pattern in `corpus/v1/`. This is
  already the project's pattern and ADR-0004 doesn't change it.
- The CLI surface changes additively (`--format=junit` is new; the
  existing text and JSON formats remain). No breaking changes
  inside the v0.x line.
- ADR-0003 remains the authoritative record of why the v0.1.0
  boundary was drawn where it was; this ADR supersedes only the
  scope decision itself, not the strategic reasoning ADR-0003
  documented for the open-core funnel model. The funnel model
  itself continues to apply; this ADR adjusts where the funnel's
  open mouth is.

## Alternatives considered

### Hold the ADR-0003 line and transition to commercial now

Rejected because the engineering review surfaced that the v0.1.0
engine declines on the majority of real safety code (inline
comparisons especially). The open-source funnel only feeds the
commercial product if evaluators see Floyd work on their actual
code. Holding the line means accepting that early adoption
feedback will be "tool doesn't work on my code," which kills the
funnel before commercial work has a customer to land on.

### Ship advanced report rendering as part of the v0.2.0 line

Rejected because advanced report rendering is the canonical
example of an "infinite feature request surface" — once shipped,
the open-source release owns the long tail of presentation polish
(themes, drill-down, accessibility, formatting variants) forever.
JUnit XML closes the same perception gap ("the tool produces
output a CI consumes and a human reads") at a fraction of the
maintenance cost and without burning a commercial differentiator.

### Ship a packaged CI integration product as open-source

Rejected because a packaged, marketplace-listed CI integration is
a product (versioned releases, in-PR rendering, maintained
documentation) and consumes commercial differentiation. The weaker
version (README snippets showing how to wire `cargo floyd test`
into a CI job) is documentation and stays open-source; the
distinction is made explicit in this ADR so it does not drift
later when someone reasonably says "we already have the YAML,
let's just package it."

### Expand commercial scope to recapture moat

Considered: declare additional capabilities commercial (e.g.
pattern destructuring, async, macros) to widen the commercial
list and rebuild moat. Rejected because those capabilities are
genuinely Phase 2 hard problems (acknowledged open research in
the pattern-matching MC/DC literature) and shipping them
open-source is not on any near-term horizon regardless of this
boundary. Listing them as commercial doesn't widen the moat; it
just creates a longer wish-list. They appear on the explicit
commercial list above because that is where they will land *if*
they ever land, not because they are imminent.

## References

- [ADR-0001](0001-external-engine-automotive-first.md): automotive-
  first commitment that shapes what's in scope.
- [ADR-0002](0002-runtime-pipeline.md): runtime pipeline.
- [ADR-0003](0003-open-source-vs-commercial-scope.md): superseded
  by this ADR. The v0.1.0 release was scoped per ADR-0003 and
  remains valid; the open-source / commercial boundary is
  redrawn going forward per this document.
