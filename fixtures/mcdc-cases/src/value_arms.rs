//! Class 6: the value-arm lift.
//!
//! A `bool` condition (or `&&`/`||` chain) whose two arms are
//! straight-line code writing one common non-`bool` value is lifted:
//! the decision is the condition, its outcome the arm taken
//! (`lift: "value_arms"`). Other shapes must decline.

/// `if upper { 7 } else { 39 }` (base64/hex offset).
///
/// Expected: candidate, `lift: "value_arms"`, condition `upper`,
/// `invoked: true`, 2 observations.
pub fn lift_const_arms(upper: bool) -> u8 {
    if upper {
        7
    } else {
        39
    }
}

/// `if a && b { Some(x) } else { None }`.
///
/// Expected (`--only=lift_option_arms`): candidate,
/// `lift: "value_arms"`, conditions `a`, `b`, 3 observations, both
/// exercised, percentage 100, exit 0.
pub fn lift_option_arms(a: bool, b: bool, x: u32) -> Option<u32> {
    if a && b {
        Some(x)
    } else {
        None
    }
}

/// One condition whose arms go through checked arithmetic (`Assert`
/// terminators) before they join. `Assert`-terminated arms are taken
/// by the call-arm lift first.
///
/// Expected: candidate, `lift: "call_arms"`, condition `c`,
/// `invoked: true`, 2 observations.
pub fn lift_checked_arms(c: bool, x: u32) -> u32 {
    if c {
        x + 1
    } else {
        x * 2
    }
}

/// `a && b` whose arms go through checked arithmetic before they
/// join.
///
/// Expected: candidate, `lift: "value_arms"`, conditions `a`, `b`,
/// `invoked: true`, 3 observations.
pub fn lift_checked_and(a: bool, b: bool, x: u32) -> u32 {
    if a && b {
        x + 1
    } else {
        x * 2
    }
}

/// An `if` nested in the `then` arm: three arms, the inner `if` is a
/// decision of its own.
///
/// Expected: declined, `out_of_scope` reason `unsupported_shape`.
pub fn decline_nested_if(a: bool, b: bool) -> u32 {
    if a {
        if b {
            1
        } else {
            2
        }
    } else {
        3
    }
}

/// An arm that loops.
///
/// Expected: declined, `out_of_scope` reason `unsupported_shape`.
pub fn decline_loop_arm(c: bool, n: u32) -> u32 {
    if c {
        let mut acc = 0;
        for i in 0..n {
            acc ^= i;
        }
        acc
    } else {
        5
    }
}

/// Arms writing different locals.
///
/// Expected: declined, `out_of_scope` reason `unsupported_shape`.
pub fn decline_split_locals(c: bool) -> (u32, u32) {
    let mut x = 0;
    let mut y = 0;
    if c {
        x = 1;
    } else {
        y = 2;
    }
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lift_const_arms_upper() {
        assert_eq!(lift_const_arms(true), 7);
    }

    #[test]
    fn lift_const_arms_lower() {
        assert_eq!(lift_const_arms(false), 39);
    }

    #[test]
    fn lift_option_arms_tt() {
        assert_eq!(lift_option_arms(true, true, 4), Some(4));
    }

    #[test]
    fn lift_option_arms_f() {
        assert_eq!(lift_option_arms(false, true, 4), None);
    }

    #[test]
    fn lift_option_arms_tf() {
        assert_eq!(lift_option_arms(true, false, 4), None);
    }

    #[test]
    fn lift_checked_arms_t() {
        assert_eq!(lift_checked_arms(true, 3), 4);
    }

    #[test]
    fn lift_checked_arms_f() {
        assert_eq!(lift_checked_arms(false, 3), 6);
    }

    #[test]
    fn lift_checked_and_tt() {
        assert_eq!(lift_checked_and(true, true, 3), 4);
    }

    #[test]
    fn lift_checked_and_f() {
        assert_eq!(lift_checked_and(false, true, 3), 6);
    }

    #[test]
    fn lift_checked_and_tf() {
        assert_eq!(lift_checked_and(true, false, 3), 6);
    }

    #[test]
    fn decline_nested_if_runs() {
        assert_eq!(decline_nested_if(true, false), 2);
    }

    #[test]
    fn decline_loop_arm_runs() {
        assert_eq!(decline_loop_arm(true, 3), 3);
    }

    #[test]
    fn decline_split_locals_runs() {
        assert_eq!(decline_split_locals(true), (1, 0));
    }
}
