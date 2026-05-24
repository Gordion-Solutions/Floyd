//! Alarm raise logic with hysteresis.
//!
//! Trip the alarm if the measurement is over the absolute maximum,
//! OR if we were already alarming and remain over the (lower)
//! re-arming threshold. The hysteresis prevents flapping when the
//! measurement sits near the threshold — once latched, the alarm
//! only clears below the lower threshold. This is the canonical
//! shape for diagnostic-trouble-code (DTC) latching in safety
//! monitors.

pub fn raise_alarm(over_max: bool, was_alarming: bool, over_threshold: bool) -> bool {
    over_max || (was_alarming && over_threshold)
}
