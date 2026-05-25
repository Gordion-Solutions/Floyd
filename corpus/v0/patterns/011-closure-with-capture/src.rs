//! Floyd corpus pattern 011-closure-with-capture.
//!
//! A closure captures an outer boolean by reference and combines
//! it with its parameter via `&&`. The closure body lives in its
//! own MIR function (`outer::{closure#0}`), where the captured `b`
//! appears as a dereferenced field access of the closure's
//! environment. The capture-propagation pass folds the captured
//! variable's name into the closure body's `debug_names`, letting
//! the decomposer recover `x && b` as a 2-condition decision.

pub fn outer(a: bool, b: bool) -> bool {
    let f = |x: bool| x && b;
    f(a)
}
