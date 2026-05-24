# Payload type definitions

Each edge in `workflow.toml` declares a `payload` — a typed data contract
between stages. The types are defined here.

Phase 0 placeholders only. As stages are implemented, each type gains a
real schema (Rust struct, JSON Schema, or both) so the contract is
mechanically enforced rather than asserted in prose.

## Phase 0 types

| Type                   | Source stage    | Sink stage(s)  | Notes |
|------------------------|-----------------|----------------|-------|
| `RustWorkspace`        | driver          | instrument     | Path to a Cargo workspace + CLI args |
| `InstrumentedArtifacts`| instrument      | runner         | Test binary paths + LLVM coverage map metadata |
| `CrateMetadata`        | instrument      | mir-extractor  | Per-crate target dir, deps, edition info needed to load MIR |
| `ProfRaw`              | runner          | masking        | LLVM raw profile counters from instrumented binary execution |
| `Mir`                  | mir-extractor   | decomposer     | Per-function MIR with source locations preserved |
| `DecisionTree`         | decomposer      | masking        | Per-decision condition tree (booleans, if-let, match guards, try) |
| `IndependenceMatrix`   | masking         | report         | Condition independence matrix + missing condition pairs + masking violations |
| `GroundTruth`          | corpus-v0       | masking        | Known-correct MC/DC outputs for synthetic patterns; differential-validation input |
