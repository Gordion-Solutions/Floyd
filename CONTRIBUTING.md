# Contributing to Floyd

Thanks for your interest. Floyd is an open-source MC/DC engine
focused on automotive safety-critical Rust code; contributions
that move the engine forward in that direction are welcome.

## How to contribute

The most useful contribution is **a new corpus pattern**. The
corpus is Floyd's qualification evidence: each pattern pins a
Rust source shape with its hand-computed MC/DC analysis, and CI
fails any time the engine's output drifts from the pinned
analysis. Adding patterns directly strengthens the qualification
story.

To add a pattern:

1. Pick the next available number under `corpus/v0/patterns/`
   (synthetic shapes) or `corpus/v1/patterns/` (safety-critical
   shapes). The numbering is sequential.
2. Create `<NNN>-<slug>/src.rs` — a single function that
   exercises the shape — and `<NNN>-<slug>/pattern.toml` — the
   MC/DC analysis. The schema is documented in
   [`corpus/README.md`](corpus/README.md).
3. Verify with the local quality gate (see below).
4. Open a PR.

Bug reports against engine output that disagrees with a pinned
corpus pattern are highest priority. Please include a minimal
reproducer.

## Engine and architectural changes

Floyd's open-source scope is pinned by
[ADR-0004](architecture/decisions/0004-engine-correctness-oss-boundary.md):
**engine-correctness work belongs in OSS; reporting, packaging,
and qualification artefacts are commercial**. Changes that fit
the OSS scope (new decision shape recovery, parser extensions,
masking-analysis correctness fixes, JUnit / text / JSON report
content improvements) are welcome.

**Before starting work on changes that would alter Floyd's
internal data model**, please open a design issue first.
"Internal data model" means anything that changes the shape of
`DecisionTree`, `MirFunction`, `RuntimeAnalysis`,
`IndependenceMatrix`, `CoverageReport`, or `DecisionMap`, or
that adds a new analysis pass to the pipeline (`mir-extractor`,
`decomposer`, `masking`, `report`).

The reason is twofold:

- Some seemingly-in-scope changes turn out to overlap with
  commercial features. Opening an issue first saves you from
  investing effort that won't land here.
- Internal data-model changes propagate through several stages
  (decomposer, masking, correlate, runtime, report rendering)
  and need coordination across them.

For self-contained changes (new corpus patterns, parser
extensions that don't add new MIR statement variants, bug fixes
to existing functions, documentation improvements), open a PR
directly — no design issue required.

## Quality gate

Before sending a PR, run locally:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps
```

If your change touches the runtime pipeline, also run the gated
end-to-end test (requires nightly + `llvm-tools-preview`):

```sh
cargo test --test end_to_end_001 -- --ignored
```

If your change adds a corpus pattern, the
[CI corpus-check job](.github/workflows/ci.yml) validates the
schema and truth-table consistency. You can replicate the schema
check locally by running the Python script inside that job
against your `pattern.toml`.

## Pull request expectations

- Each PR should land one logically coherent change. Multi-purpose
  PRs are harder to review and harder to revert if a problem
  surfaces later.
- Commit messages should explain the *why*, not just the *what*.
  The codebase has examples; recent commits are a reasonable
  template.
- CI must pass before merge. The maintainer reviews every PR.
- Patterns and tests committed to the corpus are immutable once
  accepted — corrections land as new patterns in a higher
  version directory, not by editing accepted ones. This
  preserves the audit trail.

## Licence

Contributions are dual-licensed under MIT OR Apache-2.0 (the
project's licence), without any additional terms or conditions.

## Code of conduct

Be respectful. Disagree with the technical decision, not the
person making it. Maintainer decisions on scope and architecture
are final, but reasons will always be given so future
contributors can understand them.
