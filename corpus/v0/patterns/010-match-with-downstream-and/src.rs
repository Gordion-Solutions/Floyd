//! Floyd corpus pattern 010-match-with-downstream-and.
//!
//! A `match` produces an intermediate boolean that's then `&&`'d
//! against another condition. Before the intermediate-propagation
//! fold, the engine stopped at the per-arm intermediate
//! (`_3 = const true/false`) and silently missed the downstream
//! `&& b`. With the fold, the engine follows the intermediate's
//! value into the downstream `switchInt` and recovers the
//! combined decision.

pub fn classify(n: i32, b: bool) -> bool {
    let lit = match n {
        0 => true,
        _ => false,
    };
    lit && b
}
