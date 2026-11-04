//! Class 3: spurious candidates.
//!
//! Shapes the decomposer once turned into fake decisions: operand
//! set-up copies, drop flags written before a loop's `goto`, arms
//! that copy a non-`bool` value, the `// MIR FOR CTFE` body of a
//! `const fn`, and trees that test no condition. None of these may
//! become a condition. A function whose decision cannot be recovered
//! without them is listed in `out_of_scope` with its `reason`; one
//! whose non-`bool` arms are lifted or ignored is a candidate whose
//! only condition is the comparison.

/// A non-`Copy` pair, for the struct-arm case.
#[derive(Debug)]
pub struct Pair {
    /// First half.
    pub hi: u64,
    /// Second half.
    pub lo: u64,
}

/// Drop flag before a loop: `text` is moved on one path inside the
/// loop, so rustc keeps a drop flag written as `const false` /
/// `const true` in the entry block, which ends in `goto` to the loop
/// head (BLAKE3 `copy_wide`, hashbrown `fold_impl`).
///
/// Expected: not a candidate; `out_of_scope` reason
/// `unsupported_shape` (the drop flag is never read as the value of a
/// zero-condition tree).
pub fn drop_flag_loop(text: String, limit: usize) -> usize {
    let mut i = 0;
    loop {
        if i >= limit {
            return i;
        }
        if i == 7 {
            drop(text);
            return 0;
        }
        i += 1;
    }
}

/// A `Copy` 256-bit value, for the `else if` struct-arm case.
#[derive(Clone, Copy, Debug)]
pub struct Wide {
    /// High half.
    pub hi: u128,
    /// Low half.
    pub lo: u128,
}

/// The entry block writes the returned local's constant and jumps to
/// a loop head that branches on something else: the entry-block
/// decomposition is a bare `Const false`.
///
/// Expected: not a candidate; `out_of_scope` reason `no_conditions`.
pub fn const_entry(limit: u32) -> bool {
    let found = false;
    let mut i = 0;
    while i < limit {
        i += 1;
    }
    found
}

/// Struct arms through an `else if` (fixed's `shl_i256_max_128`):
/// `if sh == 0 { a } else if sh == 128 { .. } else { .. }`. The first
/// arm copies a named non-`bool` local; three arms are not a lifted
/// shape.
///
/// Expected: not a candidate; `out_of_scope` reason `non_bool_leaf`
/// (never a decision with a condition named `a`).
pub fn wide_leaf(sh: u32, a: Wide) -> Wide {
    if sh == 0 {
        a
    } else if sh == 128 {
        Wide { hi: a.lo, lo: 0 }
    } else {
        Wide {
            hi: a.hi << 1,
            lo: a.lo << 1,
        }
    }
}

/// Moved struct arms: `if sh == 0 { a } else { b }`.
///
/// Expected: the decision is `sh == 0` alone; the arms `a`, `b` are
/// never conditions.
pub fn struct_leaf(sh: u32, a: Pair, b: Pair) -> Pair {
    if sh == 0 {
        a
    } else {
        b
    }
}

/// `usize` arms written to a `let`, not to the return place
/// (hashbrown `fit_to_page_size`).
///
/// Expected: the decision is `n == 0` alone (`lift: "value_arms"`);
/// the arms `page`, `n` are never conditions.
pub fn usize_leaf(n: usize, page: usize) -> usize {
    let size = if n == 0 { page } else { n };
    size + 1
}

/// Slice arms: `&[u8]` copies.
///
/// Expected: the decision is `first` alone (`lift: "value_arms"`);
/// the arms `x`, `y` are never conditions.
pub fn slice_leaf<'a>(first: bool, x: &'a [u8], y: &'a [u8]) -> &'a [u8] {
    if first {
        x
    } else {
        y
    }
}

/// Operand set-up copy feeding an `assert_eq!` guard (regex's
/// `Captures::extract`): `_k = copy _j; _m = Eq(..)` consumed in the
/// same block, which must not be read as the block's value.
///
/// Expected: not a candidate (no `Condition` named `len`).
pub fn setup_copy(len: usize, want: usize) -> usize {
    assert_eq!(len, want, "length mismatch");
    len * 2
}

/// A `const fn` whose `// MIR FOR CTFE` body keeps the
/// `_5 = copy _1; switchInt(move _5)` shape (rust-url
/// `AsciiDenyList::new`). It is used only in a constant, so the
/// runtime body never runs.
///
/// Expected: the CTFE body is in `out_of_scope` with reason
/// `ctfe_body`; it is never a candidate from the CTFE body.
pub const fn ctfe_flag(deny: bool, bits: u32) -> u32 {
    let mut out = bits;
    if deny {
        out |= 1;
    }
    out
}

/// The constant that makes `ctfe_flag` a compile-time evaluation.
pub const CTFE_FLAG: u32 = ctfe_flag(true, 4);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_flag_loop_runs() {
        assert_eq!(drop_flag_loop(String::from("x"), 3), 3);
    }

    #[test]
    fn const_entry_runs() {
        assert!(!const_entry(2));
    }

    #[test]
    fn wide_leaf_runs() {
        let w = wide_leaf(0, Wide { hi: 1, lo: 2 });
        assert_eq!((w.hi, w.lo), (1, 2));
    }

    #[test]
    fn struct_leaf_runs() {
        let p = struct_leaf(0, Pair { hi: 1, lo: 2 }, Pair { hi: 3, lo: 4 });
        assert_eq!((p.hi, p.lo), (1, 2));
    }

    #[test]
    fn usize_leaf_runs() {
        assert_eq!(usize_leaf(0, 4096), 4097);
    }

    #[test]
    fn slice_leaf_runs() {
        assert_eq!(slice_leaf(false, b"a", b"b"), b"b");
    }

    #[test]
    fn setup_copy_runs() {
        assert_eq!(setup_copy(3, 3), 6);
    }

    #[test]
    fn ctfe_flag_const() {
        assert_eq!(CTFE_FLAG, 5);
    }
}
