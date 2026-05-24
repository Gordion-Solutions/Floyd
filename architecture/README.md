# Architecture

Floyd is designed as a graph before it's designed as code.

`workflow.toml` describes the top-level pipeline as a directed graph: each
node is a tool / pipeline stage, each edge is a typed data contract between
stages. Sub-graphs under `tools/` describe the internal structure of
individual tools (entry function, helper functions, intra-tool call edges).

The graph is the design source of truth. As code lands, CI validates that
the actual `syn`-extracted call graph matches the declared sub-graphs.
Drift between design and implementation fails the build — that is what
keeps the graph load-bearing instead of decorative.

## Layout

```
architecture/
├── workflow.toml          Top-level pipeline graph
├── tools/
│   └── decomposer.toml    First sub-graph (the Phase 0 hardness)
├── types/                 Payload type definitions referenced by edges
└── decisions/             Architecture Decision Records (ADRs)
```

The `corpus-v0` node in `workflow.toml` references `../corpus/v0/` —
the benchmark corpus that drives qualification-by-validation per
[ADR-0001](decisions/0001-external-engine-automotive-first.md).

## Phase 0 scope

Per the Phase 0 discipline (don't pre-design what isn't being built yet),
only one sub-graph exists today: `decomposer.toml`. The other Phase 0
nodes — `driver`, `instrument`, `runner`, `mir-extractor`, `masking`,
`report` — appear as nodes in `workflow.toml` but have no sub-graph yet.
Each gains a sub-graph when its implementation starts.

## Why this exists

Three reasons:

1. **The domain is graph-shaped.** MC/DC is decision graphs, condition
   independence matrices, control flow. Designing the tool as a graph is
   dogfooding.
2. **ISO 26262 wants traceability.** Requirements → design → implementation
   → tests. A versioned graph that drives the code makes that traceability
   mechanical instead of retrofitted. By the time qualification matters,
   this directory is years of audit evidence rather than something to
   reverse-engineer. (DO-330 / TQL has similar needs if Floyd ever pursues
   that lane — see [decisions/0001](decisions/0001-external-engine-automotive-first.md).)
3. **Multi-tool data contracts must not drift per-tool.** Floyd is 10+
   tools across 4 phases with typed data flowing between them (profraw →
   decision tree → independence matrix → signed report). Pinning the
   contracts as typed edges before writing code stops them drifting.
