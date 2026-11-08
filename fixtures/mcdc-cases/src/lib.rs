//! Regression fixture for `cargo floyd test`.
//!
//! Each module holds small, deliberately shaped functions for one
//! class of engine bug found on real crates, and the unit tests that
//! exercise them. Every function documents the class it exercises
//! and the outcome Floyd must report for it. The integration test
//! `cargo-floyd/tests/fixture_cases.rs` runs `cargo floyd test
//! --format=json` on this crate and checks those outcomes.
//!
//! No fixture function name is a substring of another's, so
//! `--only=<name>` selects exactly one function.
//!
//! The crate has no dependencies and builds on stable and nightly.

pub mod assert_terminators;
pub mod child_process;
pub mod correlation;
pub mod not_measurable;
pub mod observation;
pub mod spurious;
pub mod value_arms;
