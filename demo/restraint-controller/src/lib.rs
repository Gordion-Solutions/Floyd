//! Occupant-aware frontal airbag arming decision.
//!
//! This crate simulates a decision in a vehicle Restraint Control
//! Module (RCM). It is a single pure boolean function —
//! intentionally small, so the MC/DC analysis Floyd produces is
//! legible end-to-end.
//!
//! # Spec
//!
//! The frontal airbag shall be armed if and only if:
//!
//! 1. An occupant is present in the seat.
//! 2. **Either** a crash has been detected by the sensor fusion
//!    module, **or** the vehicle is above the deploy-threshold
//!    speed **and** the occupant is NOT belted (pre-crash
//!    high-energy flight risk).
//!
//! Rendered as a decision:
//!
//! ```text
//! arm iff: occupant_present
//!      AND (crash_detected
//!           OR (speed_above_threshold AND NOT seatbelt_fastened))
//! ```
//!
//! # Demo
//!
//! As shipped, the implementation below matches the spec and the two
//! test files (`tests/weak.rs`, `tests/mcdc.rs`) reach 100% MC/DC of
//! the decision. The walkthrough in the crate `README.md` plants a
//! wiring bug (the `!` on `seatbelt_fastened` is dropped) and removes
//! `tests/mcdc.rs`. The remaining `tests/weak.rs` suite passes against
//! the buggy code with 100% line coverage of the function, and
//! Floyd's MC/DC analysis shows the two conditions it never
//! demonstrates. Writing the tests Floyd asks for exposes the bug.

/// Decide whether to arm the frontal airbag for the front occupant.
///
/// See the crate-level docs for the spec. All inputs are boolean
/// signals from upstream sensor-fusion and seat-sensor modules.
pub fn arm_frontal_airbag(
    occupant_present: bool,
    crash_detected: bool,
    speed_above_threshold: bool,
    seatbelt_fastened: bool,
) -> bool {
    // The inner right-hand leg is
    //   speed_above_threshold AND NOT seatbelt_fastened
    // (unbelted occupants are a pre-crash flight risk). The demo
    // walkthrough plants a bug by deleting the `!` below.
    occupant_present
        && (crash_detected || (speed_above_threshold && !seatbelt_fastened))
}
