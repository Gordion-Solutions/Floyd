//! Class 8: functions with no Boolean decision, user `matches!`
//! predicates, and decisions too wide to analyse.
//!
//! Every function with a `switchInt` must appear exactly once, as a
//! candidate or in `out_of_scope`. A function none of whose
//! `switchInt`s is a decision the user wrote is listed with reason
//! `no_decision` and a `detail`; a user `matches!` (whose `switchInt`
//! sits in `core`'s `matches!` definition) is never dropped; a
//! decision with too many conditions is declined quickly.

/// Operating mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Armed.
    Armed,
    /// Self-test.
    Test,
    /// Off.
    Off,
}

/// A user `matches!` predicate (rust-url `Url::has_host`). Its
/// `switchInt` and arms carry spans in `core`'s `matches!`; only the
/// scrutinee read is in this file. Floyd once dropped such functions
/// from both `candidates` and `out_of_scope`.
///
/// Expected: a candidate located in this file. The decision's spans
/// all lie in `core`, so coverage cannot measure it:
/// `not_measurable` with reason `external_macro`, whether rustc
/// prints those spans as `/rustc/<hash>/library/..` (no `rust-src`
/// component, as on CI) or as the toolchain's
/// `lib/rustlib/src/rust/library/..` (`rust-src` installed). Floyd
/// once reported `no_branch_region` without `rust-src`.
pub fn mode_is_live(m: Mode) -> bool {
    matches!(m, Mode::Armed | Mode::Test)
}

/// A 40-alternative `matches!` over `char` ranges (rust-url
/// `is_url_code_point`). Expanded into a tree, both edges of each
/// range's first test lead to the next alternative, so the tree
/// doubles per alternative.
///
/// Expected: not a candidate; `out_of_scope` reason
/// `too_many_conditions`, declined in well under a second.
pub fn is_code_point(c: char) -> bool {
    matches!(
        c,
        'a'..='b' | 'c'..='d' | 'e'..='f' | 'g'..='h' | 'i'..='j' | 'k'..='l' | 'm'..='n' | 'o'..='p'
            | 'q'..='r' | 's'..='t' | 'u'..='v' | 'w'..='x' | 'y'..='z' | 'A'..='B' | 'C'..='D'
            | 'E'..='F' | 'G'..='H' | 'I'..='J' | 'K'..='L' | 'M'..='N' | 'O'..='P' | 'Q'..='R'
            | 'S'..='T' | 'U'..='V' | 'W'..='X' | 'Y'..='Z' | '0'..='1' | '2'..='3' | '4'..='5'
            | '6'..='7' | '8'..='9' | '\u{A0}'..='\u{D7FF}' | '\u{E000}'..='\u{FDCF}'
            | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{1FFFD}' | '\u{20000}'..='\u{2FFFD}'
            | '\u{30000}'..='\u{3FFFD}' | '\u{40000}'..='\u{4FFFD}' | '\u{50000}'..='\u{5FFFD}'
            | '\u{60000}'..='\u{6FFFD}'
    )
}

/// Only an `assert_eq!`: its comparison and `switchInt` are in
/// `core`.
///
/// Expected: `out_of_scope` reason `no_decision`, detail
/// `std_macro_only`.
pub fn require_even(x: u32) {
    assert_eq!(x % 2, 0, "odd");
}

/// Only an `assert!` on a user condition.
///
/// Expected: `out_of_scope` reason `no_decision`, detail
/// `assert_only`.
pub fn require_positive(x: i32) -> i32 {
    assert!(x > 0 && x < 100);
    x
}

/// Only `?`.
///
/// Expected: `out_of_scope` reason `no_decision`, detail
/// `question_mark_or_loop_only`.
pub fn first_digit(s: &str) -> Option<u32> {
    let c = s.chars().next()?;
    c.to_digit(10)
}

/// A plain enum `match` with non-`bool` arms.
///
/// Expected: `out_of_scope` reason `no_decision`, detail
/// `pattern_match_only`.
pub fn mode_code(m: Mode) -> u8 {
    match m {
        Mode::Armed => 7,
        Mode::Test => 3,
        Mode::Off => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_is_live_armed() {
        assert!(mode_is_live(Mode::Armed));
        assert!(!mode_is_live(Mode::Off));
    }

    #[test]
    fn is_code_point_letters() {
        assert!(is_code_point('q'));
        assert!(!is_code_point(' '));
    }

    #[test]
    fn requirements_hold() {
        require_even(4);
        assert_eq!(require_positive(5), 5);
        assert_eq!(first_digit("7x"), Some(7));
        assert_eq!(mode_code(Mode::Test), 3);
    }
}
