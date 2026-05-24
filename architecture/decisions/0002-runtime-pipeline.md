# ADR-0002: Phase 1 runtime pipeline

| | |
|---|---|
| Status | Accepted (Q3 / Q4 resolved 2026-05-23 — see Scouting Findings) |
| Date | 2026-05-23 |
| Supersedes | — |

## Context

Phase 0 delivered the *static* half of Floyd: given a function's MIR,
[`floyd::decision::decompose`] recovers an If-Then-Else
representation of every boolean decision, and
[`floyd::masking::analyze`] computes the truth table and per-condition
independence pairs from that ITE. Verified end-to-end against
synthetic corpus patterns `001-simple-and`, `002-simple-or`,
`003-nested-and-or`, and `004-not-and`.

Phase 1's open-source release target requires the *runtime* half:
- Build user crates with rustc coverage instrumentation
- Execute their test suites under that instrumentation
- Read the raw counter data (`.profraw`) and the coverage map
- Cross-reference runtime counters back to the static decisions
- Compute which independence pairs were *exercised* by tests vs
  which remain unexercised
- Render that as a usable coverage report

Per [ADR-0001](0001-external-engine-automotive-first.md) Floyd is an
external engine and does **not** depend on a future rustc MC/DC
re-attempt. The runtime pipeline must work on rustc as it ships
today (post-PR-#144999): branch + condition instrumentation via
`-Cinstrument-coverage` with `-Zcoverage-options=branch,condition`,
plus structural MIR via `--emit=mir`.

## Decision

The Phase 1 runtime pipeline ingests coverage data via **subprocess
invocations of `llvm-profdata` and `llvm-cov`** from the
`llvm-tools-preview` rustup component. It does **not** link native
LLVM bindings into Floyd. The structural MIR analysis continues to
read rustc's `--emit=mir` text output via the existing
[`floyd::mir`] parser. A new **correlation layer** joins coverage map
regions to MIR decisions by source span `(file, line range)`.

### Data flow

```
[user] cargo floyd test
   │
   ▼
[cargo-floyd] orchestrate:
   1. cargo build --tests with
        -Cinstrument-coverage
        -Zcoverage-options=branch,condition
        --emit=mir
   2. cargo metadata → locate test binaries + MIR dump paths
   3. Run each test binary with
        LLVM_PROFILE_FILE=<workdir>/test-%m.profraw
   │
   ▼
[subprocess] llvm-profdata merge *.profraw → merged.profdata
[subprocess] llvm-cov export --instr-profile=merged.profdata
                <binary> --format=text → coverage.json
   │
   ▼
[floyd::mir]          parse .mir files       → Mir
[floyd::profile]      parse coverage.json    → CoverageMap + Counters  (NEW)
[floyd::decision]     decompose(Mir)         → DecisionTree
[floyd::correlate]    join regions ↔ MIR     → DecisionMap             (NEW)
[floyd::masking]      analyze + classify     → IndependenceMatrix
                                                + exercised pairs       (EXTENDED)
[floyd::report]       render                 → HTML/JSON/SARIF/LCOV     (NEW)
```

### New modules

| Module | Responsibility |
|---|---|
| `floyd::instrument` | Build the cargo command line with the right flags and invoke it. |
| `floyd::runner` | Execute instrumented test binaries with unique `LLVM_PROFILE_FILE` paths. Collect produced `.profraw` files. |
| `floyd::profile` | Subprocess `llvm-profdata merge` then `llvm-cov export --format=text`. Parse the JSON into typed structures. |
| `floyd::coverage_map` | Decode the structural regions of the coverage map output (files, line ranges, condition IDs, counter expressions). |
| `floyd::correlate` | Cross-reference MIR decisions with coverage map regions by source span. Produce a `DecisionMap` that ties each `Node` in the `DecisionTree` to runtime counter values. |
| `floyd::report` | Render outputs (HTML first, then JSON / SARIF / LCOV). |

`floyd::masking::analyze` is extended to accept an optional
`DecisionMap` and, when provided, classify each independence pair
as Exercised, PartiallyExercised, or Unexercised.

## Consequences

### Positive

- **No native LLVM linking.** Floyd's build stays pure-Rust + `cargo`;
  no `clang-sys` / `llvm-sys` / nightly-only LLVM crate dependency.
- **Toolchain available out of the box.** `llvm-tools-preview` is a
  standard rustup component already installed on most CI runners.
- **Stability inheritance.** rustc's coverage emission is exercised
  by the much larger `cargo-llvm-cov` user base; we benefit from
  bug fixes upstream without depending on rustc internals.
- **Stable upgrade path.** When `rustc_driver` or `stable_mir`
  matures enough to replace the `--emit=mir` text parser, only
  `floyd::mir` changes — everything downstream is insulated.

### Negative

- **Brittle to `llvm-cov export --format=text` changes.** The JSON
  schema is documented but not strictly versioned. Mitigation: pin
  a supported `llvm-tools-preview` range, snapshot-test the parser
  against fixture JSON, document the supported version window.
- **Cross-format source-span join is the riskiest block.** The join
  algorithm in `floyd::correlate` has to handle macro expansion,
  inlining, generic monomorphization, and `?`-desugaring. Phase 0
  doesn't exercise any of these; first contact with real-world code
  will likely produce surprises.
- **Subprocess invocation cost.** Each `cargo floyd test` run shells
  out at least twice. Tolerable for CI, possibly noticeable for
  interactive workflows on large workspaces. Mitigation: batch
  binaries through a single `llvm-cov` invocation; cache merged
  profdata between runs.
- **Nightly Rust dependency for `-Zcoverage-options`.** The
  `-Z` flag is nightly-only as of today. Floyd's runtime pipeline
  therefore inherits a nightly-toolchain requirement until / unless
  this flag stabilises. The static side (decomposer, masking) remains
  stable-compatible.

### Neutral

- The existing static API (`decompose`, `analyze`, `DecisionTree`,
  `IndependenceMatrix`) is unaffected. Downstream consumers that use
  Floyd as a static analyzer continue to work.

## Alternatives considered

### Native LLVM bindings (`llvm-sys` / `llvm-coverage-tools`)

Link LLVM directly, parse `.profraw` and the coverage map in
process. Rejected because:
- Pulls a heavyweight C++ build dependency into Floyd's build
  pipeline.
- Versioning headache: LLVM major versions break ABI; we'd track
  rustc's bundled LLVM version explicitly.
- The benefits (slightly faster, no JSON intermediary) do not justify
  the maintenance cost at Phase 1.

A future ADR may re-route to a native binding once the engine's
hot path matters; today it does not.

### Full `rustc_driver` integration

Use rustc as a library, get typed MIR access plus direct coverage
map APIs. Rejected for Phase 1 because:
- Requires building against `rustc-private` crates with nightly +
  `rustc-dev` component — production deployment friction.
- Internal APIs change every release; high maintenance cost.
- The structural MIR information we need is already accessible via
  `--emit=mir` text output.

Re-evaluated as a separate ADR if the text-format brittleness
becomes problematic.

### Skip MIR entirely; trust the coverage map's condition regions

Use only `-Zcoverage-options=condition` instrumentation. The
coverage map's *condition regions* directly identify atomic
conditions and their evaluation outcomes per test run. We could
compute MC/DC analysis purely from those without joining against
MIR.

This is **Open Question Q4 below**. If the condition regions provide
sufficient information, this would dramatically simplify the
pipeline by eliminating the `correlate` module. The static MIR
analysis would remain as a development-time cross-check rather than
a runtime dependency. Scouting required before this can be accepted
or rejected.

## Open questions (scouting required)

| ID | Question | Blocks |
|----|----------|--------|
| Q1 | How do we recover **source spans** from MIR text output? `--emit=mir` does not include them by default. | Source-span join in `floyd::correlate`. |
| Q2 | Is `llvm-cov export --format=text` JSON **stable enough across LLVM versions** that we can pin a supported range and ship? | Long-term stability of `floyd::profile`. |
| Q3 | What does `-Zcoverage-options=condition` actually emit in the coverage map? What does each region carry? | Whole `floyd::coverage_map` design. |
| Q4 | Given Q3, can we compute MC/DC **without joining against MIR** at all? | Whether `floyd::correlate` is needed. |

## Scouting findings (2026-05-23)

A scouting crate (`/tmp/floyd-runtime-scout`) reproduced corpus
pattern 001 (`fn decide(a, b) -> bool { a && b }`) with two tests
exercising it: `tests::ff` calls `decide(false, false)`, `tests::tt`
calls `decide(true, true)` — deliberately partial MC/DC coverage.

Built with:

```
RUSTFLAGS='-Cinstrument-coverage -Zcoverage-options=branch,condition'
LLVM_PROFILE_FILE='cov-%m-%p.profraw'
cargo +nightly test --tests
```

Ingested with `llvm-profdata merge` + `llvm-cov export --format=text`
from `llvm-tools-preview` (nightly toolchain).

### Q3 — coverage-map condition region semantics: **RESOLVED**

Each atomic boolean condition emits one entry in the function's
`branches` array. The entry format is a 9-tuple:

```
[start_line, start_col, end_line, end_col,
 true_counter, false_counter,
 file_id, expanded_file_id, kind]
```

For `fn decide(a, b) -> bool { a && b }` (source at line 2), the
aggregate run yields:

| Position | Condition | true | false |
|---|---|---|---|
| `2:5..2:6` | `a` | 1 | 1 |
| `2:10..2:11` | `b` | 1 | 0 |

The `(b: false=0)` value is the direct on-the-wire evidence of
short-circuit semantics — `b` was never evaluated when `a` was
false. `kind = 4` denotes a branch region. `file_id = 0` references
the function's `filenames` array.

There is also an `mcdc_records` field on each function in the export
schema, but it is empty under `-Zcoverage-options=branch,condition`.
That field was populated only by the removed `-Zcoverage-options=mcdc`
instrumentation. Floyd does not rely on it.

### Q4 — can we skip MIR cross-reference entirely?: **MOSTLY POSITIVE**

Per-test profraw works as designed. Running

```
LLVM_PROFILE_FILE=test_ff.profraw <bin> --exact tests::ff
LLVM_PROFILE_FILE=test_tt.profraw <bin> --exact tests::tt
```

and exporting each separately produces:

| Test | `a` true/false | `b` true/false |
|---|---|---|
| `tests::ff` | 0 / 1 | 0 / 0 |
| `tests::tt` | 1 / 0 | 1 / 0 |

This is exactly the per-test condition-assignment matrix that MC/DC
analysis needs. The pipeline can therefore:

1. Use MIR (via [`crate::decision::decompose`]) to recover the
   *structural* decision tree and to enumerate the conditions and
   their declared source spans.
2. Use per-test branch data (from [`floyd::profile`]) to read the
   *runtime* values of those conditions per test.
3. Join the two by source span only — no counter-ID resolution
   across the MIR / coverage-map boundary.

`floyd::correlate` therefore shrinks to a thin source-span lookup:
"given a MIR decision with conditions at source spans S₁, S₂, … ,
find each Sᵢ in the per-test branches data and read off its true /
false count." No deep counter-expression resolution required.

### Q1 — MIR source spans: **PROMOTED TO LOAD-BEARING**

With Q4's resolution, Q1 becomes the gating sub-question for the
correlation layer: the MIR parser must capture source spans on
`switchInt` terminators (and ideally on every value-setting
statement) so they can be matched against `branches` entries.

Two options:

- **(a)** Augment the existing text-format MIR parser. Source spans
  are emitted by `rustc -Zdump-mir-spanview` / `-Zdump-mir=all` in
  adjacent `.spanview.html` / span-annotated dump files. Parse those
  alongside the textual MIR.
- **(b)** Migrate to `rustc_driver` / `stable_mir` for typed access.
  Cleaner long-term but pulls in the nightly + `rustc-dev`
  dependency the Decision section above defers.

Recommend **(a)** for Phase 1 since it preserves the existing
parser shape and avoids the nightly-rustc-private build complexity.
A future ADR can route to (b) when the `rustc_driver` text-format
brittleness becomes a real cost.

### Q2 — `llvm-cov export` JSON stability: **DEFERRED**

Answerable during `floyd::profile` implementation. Strategy:
snapshot-test the parser against committed fixture JSONs; pin a
supported `llvm-tools-preview` version range; document the support
window.

### Effort impact

The `correlate` block was the highest-risk item before scouting;
collapsing it into a source-span lookup is the most valuable
outcome of the Q4 resolution and shortens the overall effort.

## References

- rustc unstable book: [`-Cinstrument-coverage`](https://doc.rust-lang.org/rustc/instrument-coverage.html)
- LLVM [Coverage Mapping Format](https://llvm.org/docs/CoverageMappingFormat.html)
- `cargo-llvm-cov` source for prior art on the subprocess approach
- rustc tracking issue [#124144](https://github.com/rust-lang/rust/issues/124144) (re: condition / MC/DC instrumentation)
- ADR-0001 (architectural commitment to external engine)
