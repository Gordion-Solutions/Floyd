//! # Floyd
//!
//! Open-source MC/DC (Modified Condition/Decision Coverage) engine for Rust.
//!
//! Floyd consumes the per-branch and per-condition coverage instrumentation
//! that rustc already emits via `-Cinstrument-coverage`, recovers the
//! logical decision structure from MIR, and produces an MC/DC analysis:
//! condition independence matrices, masking violations, and the minimum
//! test set that would achieve full MC/DC coverage.
//!
//! ## Scope
//!
//! This crate is the engine. The `cargo floyd` driver lives in the sibling
//! [`cargo-floyd`](https://crates.io/crates/cargo-floyd) crate. The
//! engine's external interface follows the contract pinned by
//! `architecture/workflow.toml`:
//!
//! - Inputs:  [`Mir`], [`CoverageReport`]
//! - Outputs: [`IndependenceMatrix`]
//!
//! ## Status
//!
//! v0.2.1. The `cargo floyd test` workflow runs end-to-end on real
//! cargo projects, recovering boolean decisions built from `&&`,
//! `||`, `!`, inline comparisons (`>`, `<`, `==`, `!=`, `>=`, `<=`),
//! `if let` with a binding, the `?` operator (skip-through),
//! literal integer `match`, enum `match` without bindings,
//! match-into-`&&` intermediate-propagation, and closures
//! capturing outer booleans (by-value and by-reference). JUnit XML
//! output is available for CI integration via
//! `cargo floyd test --format=junit`. The recovered pattern set is
//! pinned by the `corpus/` directory; see
//! [`corpus/v1/`](https://github.com/Gordion-Solutions/Floyd/tree/main/corpus/v1)
//! for the safety-critical decision patterns the engine is
//! validated against.
//!
//! See the repository's
//! [`architecture/`](https://github.com/Gordion-Solutions/Floyd/tree/main/architecture)
//! directory for the full design graph and
//! [ADR-0001](https://github.com/Gordion-Solutions/Floyd/blob/main/architecture/decisions/0001-external-engine-automotive-first.md)
//! for the architectural commitment that shapes the engine.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod correlate;
pub mod decision;
pub mod instrument;
pub mod masking;
pub mod mir;
pub mod profile;
pub mod runner;

pub use correlate::DecisionMap;
pub use decision::DecisionTree;
pub use masking::{ConditionObservation, ConditionStatus, IndependenceMatrix, RuntimeAnalysis};
pub use mir::Mir;
pub use profile::CoverageReport;
