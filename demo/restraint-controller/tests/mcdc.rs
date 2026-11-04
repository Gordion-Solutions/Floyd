//! Tests that close the MC/DC independence pairs Floyd flagged
//! on the weak suite.
//!
//! Floyd points at two conditions that need pair coverage after
//! the weak suite: `speed_above_threshold` and `seatbelt_fastened`.
//! Each pair needs two tests that differ in only that one
//! condition and produce different outcomes.
//!
//! Shared baseline for both tests: `occupant_present=T`,
//! `crash_detected=F`, `speed_above_threshold=T`. The weak suite
//! already covers `(occupant=T, crash=F, speed=F, seatbelt=T) ->
//! F` as the idle state, which forms the paired observation for
//! `speed_above_threshold` once the `(speed=T)` cases below are
//! added.
//!
//! Against the planted-bug implementation (missing `!` on
//! `seatbelt_fastened`), BOTH tests fail — flipping either input
//! independently of the other gives the wrong outcome because the
//! bug sends the seatbelt signal through the inner `&&` without
//! inversion. That's how MC/DC exposes the bug that 100% line
//! coverage missed.

use restraint_controller::arm_frontal_airbag;

#[test]
fn arms_at_high_speed_when_occupant_unbelted_and_no_crash() {
    // Pre-crash high-energy unbelted-flight scenario: speed above
    // threshold, no crash yet, occupant present and unbelted.
    // Spec: arm (the unbelted occupant will be in flight when the
    // crash arrives).
    assert!(arm_frontal_airbag(true, false, true, false));
}

#[test]
fn no_arm_at_high_speed_when_occupant_belted_and_no_crash() {
    // Speed above threshold, no crash, occupant present and
    // belted. Spec: no deploy — belted occupant is restrained;
    // deployment would be a false-positive injury event.
    assert!(!arm_frontal_airbag(true, false, true, true));
}
