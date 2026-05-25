# ADR-0003: Open-source vs commercial scope boundary

| | |
|---|---|
| Status | Superseded by [ADR-0004](0004-engine-correctness-oss-boundary.md) (2026-05-25) |
| Date | 2026-05-23 |
| Supersedes | — |

## Context

Floyd's strategy is open-core: an open-source engine that builds
ecosystem credibility and benchmark-corpus moat, with commercial
editions on top. Phase 1 ships fully open-source; later phases
introduce the open-core split.

After Phase 1's runtime pipeline landed (per ADR-0002), the question
became concrete: what exactly goes into the public release at
crates.io v0.1.0?

Without an explicit boundary, the public repository would either
creep into commercial territory or contract arbitrarily. This ADR
pins the boundary.

## Decision

The open-source release (`floyd` / `cargo-floyd` v0.1.0 on
crates.io, public source at this repository) ships a **focused MVP**
targeting one user journey: an automotive engineer running
`cargo floyd test` against a Rust source file with `#[test]`
functions and getting a per-condition MC/DC verdict under the
masking variant.

### In scope for the open-source release (v0.1.0)

- Cargo workspace support (point at Cargo.toml, find tests).
- `cargo floyd test` runtime mode (per-test observations, MC/DC verdict).
- Common boolean expression shapes recovered from `rustc` MIR.
- JSON output format.
- README, install instructions, ISO 26262 stance, caveats.
- 3–5 real-crate corpus examples.
- crates.io v0.1.0 release engineering.

This is the minimum that lets an automotive engineer install Floyd,
point it at a real ASIL-relevant Rust component, see an MC/DC
report, and decide whether to file issues or pilot further.

### Out of scope for the open-source release

Capabilities beyond the static decomposition and per-test runtime
pipeline described above are not in the open-source v0.x line.
Roadmap items further along the strategy are addressed by Floyd's
commercial editions, in a separate codebase outside this repository.

The public README will state the v0.x feature set positively (what
works), so external observers see the open-source line clearly
without having to infer what is or isn't held back.

## Consequences

### Positive

- **Open-source ship date is realistic.** Achievable on a roadmap
  aligned to early-adopter evaluation, vs much longer for a fully
  featured tool.
- **Commercial editions have a defensible differentiator.** The
  open release demonstrates the engine; commercial customers pay
  for what they can't get elsewhere.
- **Funnel works.** The adoption-then-monetisation model needs the
  open release to be useful but bounded. This scope hits both.

### Negative

- **Open-source engine is acknowledged-bounded.** The README must
  state limitations clearly so evaluators aren't surprised by what
  isn't there yet.
- **Community contribution friction.** Contributions outside the
  open-source scope land in commercial editions rather than
  upstream; this needs to be communicated kindly when it arises.
- **Commercial repository doesn't exist yet.** Routing deferred
  work needs a private codebase to be set up; that's its own
  follow-up.

### Neutral

- The CLI in the open-source release is the binary end-users invoke.
  Commercial editions layer additional capability via the same CLI
  rather than as a separate executable, with details specified in a
  future ADR.

## Alternatives considered

### Ship a fully featured open-source release

Rejected because:

- It delays the open release significantly beyond the MVP target.
- The strategically valuable extensions are commercial moat; shipping
  them open-source dissolves the commercial story.
- First-mover advantage matters for the open-source position;
  shipping sooner with a smaller scope wins the
  canonical-open-source-MC/DC-tool position faster than a delayed
  more-complete release.

### Ship even less than the MVP

Rejected because the MVP scope was derived from "what does an
automotive engineer need to actually evaluate Floyd on real code."
Cutting further makes the open-source release unusable on real code
and defeats the adoption-funnel rationale.

### Open-core via feature flags inside the same crate

Considered: keep all code in one crate but gate commercial features
behind a build-time feature flag with a licence check. Rejected
because:

- It exposes commercial implementation in public source, inviting
  forks that strip the licence check.
- It complicates the build process for both editions.
- The intended model is a separate commercial codebase consuming
  the open crate, not a feature-gated single codebase.

## References

- [ADR-0001](0001-external-engine-automotive-first.md):
  automotive-first commitment that shapes what's in scope.
- [ADR-0002](0002-runtime-pipeline.md): runtime pipeline.
