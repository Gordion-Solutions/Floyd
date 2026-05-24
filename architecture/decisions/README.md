# Architecture Decision Records (ADRs)

This directory captures the load-bearing architectural decisions for Floyd.
Each ADR is a numbered, immutable record of one decision: what was chosen,
why, what alternatives were considered, and what the consequences are.

When a previous decision is superseded, a *new* ADR is added and the
superseded one is marked accordingly — old ADRs are never edited or
deleted. This is what makes the directory load-bearing: by Phase 4 it's a
versioned audit trail of every architectural call, dating back to Phase 0,
suitable to hand to a DO-330 / ISO 26262 auditor.

## Format

```
ADR-NNNN-short-title.md
├── Status      Proposed | Accepted | Superseded by ADR-XXXX | Deprecated
├── Date        ISO 8601
├── Context     Why this decision matters now
├── Decision    What we chose, in one paragraph
├── Consequences  Positive, negative, neutral
└── Alternatives  What else was considered and why not chosen
```

## Index

| ID | Status   | Title |
|----|----------|-------|
| 0001 | Accepted | [External MC/DC engine, automotive-first focus](0001-external-engine-automotive-first.md) |
| 0002 | Accepted | [Phase 1 runtime pipeline](0002-runtime-pipeline.md) (scouting resolved 2026-05-23) |
| 0003 | Accepted | [Open-source vs commercial scope boundary](0003-open-source-vs-commercial-scope.md) |
