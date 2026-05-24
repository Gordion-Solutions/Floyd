//! Decision-tree decomposition.
//!
//! Reduces Rust logical decisions — short-circuit boolean expressions,
//! `if let`, `match`, match guards, and `?` — into per-decision
//! decision trees in If-Then-Else (ITE) form. Each branching MIR
//! `switchInt` becomes one [`Node::Ite`]; terminal blocks become
//! leaves ([`Node::Condition`] or [`Node::Const`]).
//!
//! ITE form is the standard foundation for binary decision diagrams
//! (BDDs) and is the right internal representation for MC/DC because
//! the analysis is fundamentally about truth tables, not source
//! syntax. `a && b`, `if a then b else false`, and `if !a then false
//! else b` all denote the same boolean function and the same MC/DC
//! independence pairs.
//!
//! Entry point: [`decompose`]. The internal function structure and
//! intra-tool call edges are pinned by
//! `architecture/tools/decomposer.toml`. CI (in a later phase) will
//! `syn`-extract this module's call graph and fail the build on drift.
//!
//! ## Scope
//!
//! Phase 0 recovers arbitrarily-nested combinations of `&&`, `||`,
//! and `!` from rustc-emitted MIR — anything that lowers to a CFG
//! of `switchInt`-on-boolean blocks terminating at `const true /
//! const false / copy <debug-named-local>` leaves. The remaining
//! handlers (`if let`, `match`, match guards, `?`) are stubs that
//! graduate alongside their corpus patterns.

use crate::mir::{BlockId, MirBlock, MirFunction, MirStatement, MirTerminator};
use crate::Mir;

/// A set of per-decision condition trees recovered from a single
/// translation unit's MIR.
///
/// Output of [`decompose`]. Consumed by [`crate::masking::analyze`].
#[derive(Debug, Default, Clone)]
pub struct DecisionTree {
    /// One [`Node`] per logical decision recovered from the MIR.
    pub decisions: Vec<Node>,
}

/// A node in a [`DecisionTree`]. Represented in If-Then-Else form.
///
/// The enum is `#[non_exhaustive]` so future variants (e.g. for
/// `match` arms or `?` desugaring beyond pure boolean logic) are
/// non-breaking additions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Node {
    /// A leaf — a single atomic boolean condition, named as it
    /// appeared in source (via the MIR `debug` annotation).
    Condition {
        /// Source-level name of the condition.
        name: String,
    },
    /// A constant-value leaf (`const true` / `const false`).
    Const {
        /// Constant value.
        value: bool,
    },
    /// `if cond then then_branch else else_branch`.
    ///
    /// Each `switchInt` terminator in MIR maps to one `Ite`.
    /// Composing `Ite`s recovers arbitrarily-nested boolean
    /// expressions: `a && b` is `Ite { a, b, false }`, `a || b` is
    /// `Ite { a, true, b }`, `(a && b) || c` is
    /// `Ite { a, Ite { b, true, c }, c }`, and so on.
    Ite {
        /// The condition being tested.
        cond: String,
        /// Evaluated when `cond` is true.
        then_branch: Box<Node>,
        /// Evaluated when `cond` is false.
        else_branch: Box<Node>,
    },
}

// Internal handles — mirror decomposer.toml's declared handler
// signatures. Not part of Floyd's public API.

struct BoolExpr {
    cond: String,
    then_branch: Node,
    else_branch: Node,
}

#[derive(Debug, Clone)]
struct IfLet;
#[derive(Debug, Clone)]
struct Match;
#[derive(Debug, Clone)]
struct Guard;
#[derive(Debug, Clone)]
struct Try;

/// Evaluate a [`Node`] under a partial condition assignment.
///
/// Returns `None` if the path taken under the assignment needs a
/// condition not present in `inputs`. This is the correct semantics
/// for runtime data: when a condition was short-circuited in a test
/// it is *absent* from the observation, and the evaluator should not
/// need it (the path the test actually took doesn't dereference that
/// condition either).
///
/// Used by [`crate::correlate`] / the `cargo-floyd` runtime path to
/// compute the decision outcome of a single test execution from the
/// runtime-observed condition values, without the caller having to
/// know the test's source-level inputs.
pub fn evaluate_partial(
    node: &Node,
    inputs: &std::collections::BTreeMap<String, bool>,
) -> Option<bool> {
    match node {
        Node::Condition { name } => inputs.get(name).copied(),
        Node::Const { value } => Some(*value),
        Node::Ite {
            cond,
            then_branch,
            else_branch,
        } => {
            let v = inputs.get(cond).copied()?;
            if v {
                evaluate_partial(then_branch, inputs)
            } else {
                evaluate_partial(else_branch, inputs)
            }
        }
    }
}

/// Reduce MIR-level decisions into a [`DecisionTree`].
///
/// Entry point for the decomposer stage. For each function in `mir`,
/// walks the CFG starting from the first block and builds the
/// corresponding ITE tree (or skips the function if its CFG doesn't
/// match a supported decision shape).
///
/// Each function contributes at most one [`Node`] in Phase 0 (one
/// decision per function). Multi-decision functions land alongside
/// the corresponding corpus patterns.
pub fn decompose(mir: &Mir) -> DecisionTree {
    let mut tree = DecisionTree::default();
    for f in &mir.functions {
        if let Some(entry) = f.blocks.first() {
            if let Some(node) = build_decision_from_block(entry.id, f) {
                tree.decisions.push(node);
            }
        }
    }
    tree
}

// Boolean handler: wrap a recognised BoolExpr into a Node::Ite.
// Matches `handle_boolean` in decomposer.toml.
fn handle_boolean(expr: &BoolExpr) -> Node {
    Node::Ite {
        cond: expr.cond.clone(),
        then_branch: Box::new(expr.then_branch.clone()),
        else_branch: Box::new(expr.else_branch.clone()),
    }
}

// Phase 0 stubs. Each gains a real implementation alongside its
// corpus pattern.

#[allow(dead_code)]
fn handle_if_let(_expr: &IfLet) -> Node {
    Node::Const { value: false }
}

#[allow(dead_code)]
fn handle_match(_expr: &Match) -> Node {
    // handle_match -> handle_match_guard, per decomposer.toml.
    let _ = handle_match_guard(&Guard);
    Node::Const { value: false }
}

#[allow(dead_code)]
fn handle_match_guard(_guard: &Guard) -> Node {
    Node::Const { value: false }
}

#[allow(dead_code)]
fn handle_try(_expr: &Try) -> Node {
    Node::Const { value: false }
}

// ---------------------------------------------------------------------------
// CFG walker
// ---------------------------------------------------------------------------

/// Recursively convert one MIR block (and its CFG descendants) into a
/// [`Node`].
///
/// - Branching block (`switchInt` terminator): emit a [`Node::Ite`],
///   recursing into the two successor blocks.
/// - Terminal block (`Goto` / `Return` with a value-setting statement):
///   emit the corresponding [`Node::Const`] or [`Node::Condition`].
///
/// Returns `None` if the block's shape isn't recognised — keeps the
/// engine conservative when corpus patterns exercise new shapes
/// before the engine learns them.
fn build_decision_from_block(block_id: BlockId, f: &MirFunction) -> Option<Node> {
    let block = f.blocks.iter().find(|b| b.id == block_id)?;

    match &block.terminator {
        MirTerminator::SwitchInt {
            discr,
            targets,
            otherwise,
            ..
        } => {
            // Phase 0: single-target boolean switchInt only.
            if targets.len() != 1 || targets[0].0 != 0 {
                return None;
            }
            let cond = f.debug_names.get(discr)?.clone();
            // `0` arm is the `else` branch (condition was false);
            // `otherwise` is the `then` branch (condition was true).
            let else_branch = build_decision_from_block(targets[0].1, f)?;
            let then_branch = build_decision_from_block(*otherwise, f)?;
            Some(handle_boolean(&BoolExpr {
                cond,
                then_branch,
                else_branch,
            }))
        }
        MirTerminator::Goto { .. } | MirTerminator::Return { .. } => {
            extract_terminal_value(block, f)
        }
        MirTerminator::Other { .. } => None,
    }
}

/// Extract a leaf [`Node`] from a terminal block.
///
/// Looks for an `AssignConstBool` or `AssignCopy` statement that sets
/// the return value, in source order. The first match wins (Phase 0
/// blocks have at most one such statement).
fn extract_terminal_value(block: &MirBlock, f: &MirFunction) -> Option<Node> {
    for stmt in &block.statements {
        match stmt {
            MirStatement::AssignConstBool { value, .. } => {
                return Some(Node::Const { value: *value });
            }
            MirStatement::AssignCopy { src, .. } => {
                if let Some(name) = f.debug_names.get(src) {
                    return Some(Node::Condition { name: name.clone() });
                }
            }
            MirStatement::Other { .. } => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir;

    /// `rustc --emit=mir` output for `fn decide(a, b) -> bool { a && b }`.
    const DECIDE_AND_MIR: &str = r#"
fn decide(_1: bool, _2: bool) -> bool {
    debug a => _1;
    debug b => _2;
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _0 = copy _2;
        goto -> bb3;
    }

    bb2: {
        _0 = const false;
        goto -> bb3;
    }

    bb3: {
        return;
    }
}
"#;

    /// `rustc --emit=mir` output for `fn decide(a, b) -> bool { a || b }`.
    const DECIDE_OR_MIR: &str = r#"
fn decide(_1: bool, _2: bool) -> bool {
    debug a => _1;
    debug b => _2;
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _0 = const true;
        goto -> bb3;
    }

    bb2: {
        _0 = copy _2;
        goto -> bb3;
    }

    bb3: {
        return;
    }
}
"#;

    /// `rustc --emit=mir` output for `fn decide(a, b) -> bool { !a && b }`.
    ///
    /// rustc folds the `!` into the switchInt arm swap — the MIR shape
    /// is identical to `a && b` but with bb1/bb2 contents swapped.
    const DECIDE_NOT_AND_MIR: &str = r#"
fn decide(_1: bool, _2: bool) -> bool {
    debug a => _1;
    debug b => _2;
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb1, otherwise: bb2];
    }

    bb1: {
        _0 = copy _2;
        goto -> bb3;
    }

    bb2: {
        _0 = const false;
        goto -> bb3;
    }

    bb3: {
        return;
    }
}
"#;

    /// `rustc --emit=mir` output for `fn decide(a, b, c) -> bool { (a && b) || c }`.
    const DECIDE_NESTED_AND_OR_MIR: &str = r#"
fn decide(_1: bool, _2: bool, _3: bool) -> bool {
    debug a => _1;
    debug b => _2;
    debug c => _3;
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb3, otherwise: bb1];
    }

    bb1: {
        switchInt(copy _2) -> [0: bb3, otherwise: bb2];
    }

    bb2: {
        _0 = const true;
        goto -> bb4;
    }

    bb3: {
        _0 = copy _3;
        goto -> bb4;
    }

    bb4: {
        return;
    }
}
"#;

    fn cond(name: &str) -> Node {
        Node::Condition {
            name: name.to_string(),
        }
    }

    fn const_(value: bool) -> Node {
        Node::Const { value }
    }

    fn ite(c: &str, t: Node, e: Node) -> Node {
        Node::Ite {
            cond: c.to_string(),
            then_branch: Box::new(t),
            else_branch: Box::new(e),
        }
    }

    #[test]
    fn decompose_empty_mir_yields_empty_tree() {
        let mir = Mir::default();
        let tree = decompose(&mir);
        assert!(tree.decisions.is_empty());
    }

    #[test]
    fn decompose_recognises_and_pattern_as_ite() {
        // a && b   <=>   if a then b else false
        let mir = mir::parse_text(DECIDE_AND_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], ite("a", cond("b"), const_(false)));
    }

    #[test]
    fn decompose_recognises_or_pattern_as_ite() {
        // a || b   <=>   if a then true else b
        let mir = mir::parse_text(DECIDE_OR_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], ite("a", const_(true), cond("b")));
    }

    #[test]
    fn decompose_recognises_not_and_pattern_as_ite() {
        // !a && b   <=>   if a then false else b
        // (rustc folds the unary `!` into the switchInt arm swap; the
        // engine produces a correct ITE without needing a Not variant.)
        let mir = mir::parse_text(DECIDE_NOT_AND_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], ite("a", const_(false), cond("b")));
    }

    #[test]
    fn decompose_recognises_nested_and_or_as_ite() {
        // (a && b) || c   <=>   if a then (if b then true else c) else c
        let mir = mir::parse_text(DECIDE_NESTED_AND_OR_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite("a", ite("b", const_(true), cond("c")), cond("c"),)
        );
    }
}
