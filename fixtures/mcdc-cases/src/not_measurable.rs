//! Class 5: decisions coverage cannot measure.
//!
//! Some candidates are real decisions that execution coverage cannot
//! measure. They must be classified `not_measurable` (or, for code no
//! harnessed test runs, never invoked with a note), never blamed on
//! the span join.

/// Derived `PartialEq` compares both fields: `self.armed ==
/// other.armed && self.level == other.level`. rustc does not
/// instrument `#[automatically_derived]` bodies.
///
/// Expected: never a measured candidate. The engine does not
/// decompose the field projections, so `eq` is in `out_of_scope`; if
/// it ever becomes a candidate it must be `not_measurable` with
/// reason `uninstrumented`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    /// Whether the channel is armed.
    pub armed: bool,
    /// Its level.
    pub level: u8,
}

/// A trait for the derive-shaped impl below.
pub trait Derived {
    /// `armed && ok`.
    fn derived_and(&self, ok: bool) -> bool;
}

/// What a derive macro emits: an `#[automatically_derived]` trait
/// impl. rustc does not instrument such bodies, so the MIR has no
/// coverage mapping for `derived_and` (dbc-parser's serde derives).
#[automatically_derived]
impl Derived for Reading {
    /// `armed && ok`, in a body rustc does not instrument.
    ///
    /// Expected: candidate `not_measurable` with reason
    /// `uninstrumented`, `invoked: false`, 0 observations.
    fn derived_and(&self, ok: bool) -> bool {
        let armed = self.armed;
        armed && ok
    }
}

/// A two-state switch.
#[derive(Clone, Copy, Debug)]
pub enum Switch {
    /// On.
    On,
    /// Off.
    Off,
}

/// A `match` on a fieldless enum (BLAKE3 `IncrementCounter::yes`).
/// rustc emits code regions for the arms but no branch region.
///
/// Expected: candidate `not_measurable` with reason
/// `no_branch_region`, `invoked: false`, 0 observations.
pub fn enum_match(s: Switch) -> bool {
    match s {
        Switch::On => true,
        Switch::Off => false,
    }
}

/// `a && b`, called only from the `harness = false` test target
/// `no_harness`, which Floyd does not run.
///
/// Expected: candidate `invoked: false`, 0 observations, `note`
/// naming the `no_harness` target.
pub fn harness_free_and(a: bool, b: bool) -> bool {
    a && b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_eq() {
        let r = Reading {
            armed: true,
            level: 3,
        };
        assert_eq!(r, r);
    }

    #[test]
    fn derived_and_runs() {
        let r = Reading {
            armed: true,
            level: 0,
        };
        assert!(r.derived_and(true));
        assert!(!r.derived_and(false));
    }

    #[test]
    fn enum_match_on() {
        assert!(enum_match(Switch::On));
    }

    #[test]
    fn enum_match_off() {
        assert!(!enum_match(Switch::Off));
    }
}
