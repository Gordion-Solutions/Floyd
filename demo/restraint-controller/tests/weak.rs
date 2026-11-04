//! The "shippable" test suite of the demo walkthrough.
//!
//! All three tests pass against both the correct and the planted-bug
//! implementation, and every line of `arm_frontal_airbag` runs.
//! Yet short-circuit evaluation hides two of the four conditions:
//! `seatbelt_fastened` is never evaluated, and
//! `speed_above_threshold` is only ever false. Floyd's MC/DC
//! analysis reports both as UNEXERCISED.

use restraint_controller::arm_frontal_airbag;

#[test]
fn arms_when_crash_detected_with_occupant() {
    // Crash sensor fired, occupant present. Belt + speed don't
    // need to be checked once a crash is confirmed.
    assert!(arm_frontal_airbag(true, true, false, true));
}

#[test]
fn no_arm_without_occupant() {
    // Empty seat. Never deploy — airbag replacement cost + injury
    // risk to any debris in the seat with no safety benefit.
    assert!(!arm_frontal_airbag(false, true, true, false));
}

#[test]
fn no_arm_at_low_speed_without_crash() {
    // Idle state: no crash, low speed. Deployment would be a
    // false-positive injury event.
    assert!(!arm_frontal_airbag(true, false, false, true));
}
