//! Triple-redundant actuator interlock.
//!
//! An actuator is allowed to fire only when (a) the master enable
//! signal is asserted, (b) safety channel A reports OK, and (c)
//! safety channel B reports OK. All three must agree; any single
//! channel reporting a fault inhibits actuation. This is the
//! canonical shape of an ASIL-D actuator approval gate.

pub fn allow_actuation(master_enable: bool, channel_a_ok: bool, channel_b_ok: bool) -> bool {
    master_enable && channel_a_ok && channel_b_ok
}
