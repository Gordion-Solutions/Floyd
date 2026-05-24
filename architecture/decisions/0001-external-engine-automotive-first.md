# ADR-0001: External MC/DC engine, automotive-first focus

| | |
|---|---|
| Status | Accepted |
| Date | 2026-05-22 |
| Supersedes | — |

## Context

Floyd's job is to measure MC/DC (Modified Condition/Decision Coverage) of
Rust code for safety-critical use cases. MC/DC requires the compiler to
emit per-condition instrumentation around boolean operators, `if let`,
`match` arms, and short-circuit logic.

Two facts shape the decision:

1. **rustc no longer ships MC/DC instrumentation.** Partial MC/DC support
   existed behind `-Zcoverage-options=mcdc` from 2024 until August 2025,
   when it was removed by PR #144999. The rustc coverage maintainer judged
   the implementation unmaintainable. Tracking issue #124144 remains open
   for a complete re-attempt.

2. **rustc still ships branch + condition instrumentation.** The flags
   `-Cinstrument-coverage`, `-Zcoverage-options=branch`, and
   `-Zcoverage-options=condition` continue to emit per-branch and
   per-condition counters. MC/DC reasoning can be performed externally
   on this signal — the building blocks remain, the analysis layer does
   not.

Floyd therefore had three plausible architectural paths:

1. **Upstream** — re-implement MC/DC inside rustc as a sustained
   contribution to the Rust project, then build Floyd on top of it.
2. **External** — build Floyd as a separate tool that consumes rustc's
   existing branch+condition instrumentation and performs the MC/DC
   reasoning itself.
3. **Hybrid** — ship external first for ecosystem reach, pursue upstream
   in parallel, migrate when in-tree support is mature.

The qualification ceiling argument (auditors prefer compiler-internal
analysis) is the strongest pull toward Options 1 or 3. That argument
applies most strongly under DO-178C / DO-330 (aerospace) at the highest
Tool Qualification Levels (TQL-1 through TQL-4). Under ISO 26262
(automotive), the qualification regime accepts a *qualification-by-
validation* approach: a well-validated external tool can reach TCL3 (the
highest Tool Confidence Level) without being inside the compiler. The
benchmark corpus we plan to build *is* the validation evidence.

## Decision

**Floyd is an external MC/DC engine.** It consumes rustc's existing
`-Cinstrument-coverage` + `-Zcoverage-options=branch,condition` output
and performs MC/DC reasoning in its own code. No rustc fork. No required
upstream change.

**The primary qualification target is ISO 26262 TCL3** (and ASIL-D
component-level analysis), via the qualification-by-validation path.
Other regimes (DO-178C, IEC 61508, IEC 62304) are not in scope through
the early phases; they may be revisited later but are not promised.

**Upstream rustc engagement is a community track, not an engineering
track.** Floyd will track rustc issue #124144 and comment when relevant,
but no Floyd engineering hours are committed to drafting an upstream
rustc MC/DC RFC at this time. If a credible upstream re-attempt lands
and stabilizes, a future ADR can supersede this one and re-route Floyd
onto in-tree instrumentation.

## Consequences

### Positive

- **Time to first useful release** is bounded by Floyd's own
  engineering rather than rustc team review cycles.
- **Architectural simplicity**: one codebase, one release pipeline,
  no rebasing-on-upstream treadmill.
- **No fork burden**: avoids the multi-million-dollar engineering
  commitment of maintaining a compiler fork (explicit non-goal).
- **Qualification path is clear**: ISO 26262 TCL3 via
  qualification-by-validation, with the benchmark corpus as the central
  evidence artifact.
- **Benchmark corpus becomes the moat**: every contribution and citation
  compounds the validation evidence; external tools that benchmark
  against the corpus reinforce Floyd's position.

### Negative

- **DO-178C / DO-330 Level A market is out of reach** until upstream
  rustc MC/DC lands and Floyd migrates. AdaCore's `gnatcov` continues
  to dominate that lane.
- **TCL3 ceiling exists**: above TCL3 is not relevant in ISO 26262
  (TCL3 is the top), so this is not a practical limit — but a
  multi-standard customer wanting a unified tool across automotive and
  aerospace cannot use Floyd alone for both.
- **Vulnerable to a future upstream re-attempt**: if a third party
  produces a credible in-tree MC/DC implementation, Floyd is
  positioned as a parallel competitor rather than the canonical
  effort. Mitigation: stay engaged in #124144, ensure Floyd can
  consume in-tree instrumentation if it lands.
- **Single-domain narrative**: open-source pitch is sharper but less
  universal than "MC/DC for everyone."

### Neutral

- The decomposer, masking, and report stages are unchanged across all
  three options — they operate on a `DecisionTree` representation that
  is independent of where the upstream signal comes from. A future
  migration to in-tree instrumentation replaces the `mir-extractor`
  stage but leaves the rest intact.

## Alternatives considered

### Option 1: Upstream

Re-implement MC/DC inside rustc and have Floyd depend on the in-tree
output. Rejected for Phase 0 because:

- Long horizon before any output is possible (gated on rustc team
  review and acceptance).
- The prior implementation was removed for unmaintainability; the
  rustc team will hold a re-attempt to a high bar.
- The Rust ecosystem void is open *now*; waiting cedes the position.

Not foreclosed permanently — a future ADR may re-route Floyd onto
in-tree instrumentation if the upstream situation matures.

### Option 3: Hybrid (Section A's original recommendation)

Ship external first, pursue upstream in parallel. Rejected as the
*formal* commitment because:

- ISO 26262 automotive focus dissolves the qualification ceiling
  argument that motivated the hybrid path.
- Committing Floyd engineering hours to upstream RFC drafting in
  Phase 0 dilutes the prototype effort.
- Community engagement on #124144 captures most of the upstream
  optionality without the engineering cost.

This ADR effectively chooses "Option 2 with passive upstream
observation," which sits between Section A's original Options 2 and 3.

### Forking rustc

Explicit non-goal in Section A. Not considered.

## References

- rustc tracking issue #124144 (MC/DC, open)
- rustc PR #144999 (removal of prior `-Zcoverage-options=mcdc`)
- ISO 26262-8:2018 Clause 11 (Tool qualification)
- DO-330 / TQL framework (for the contrast)
- *Toward Modified Condition/Decision Coverage of Rust* (AIAA, September 2025)
