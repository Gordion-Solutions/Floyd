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

use crate::mir::{
    BlockId, CompareOp, LocalId, MirBlock, MirFunction, MirStatement, MirTerminator, Operand,
};
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

/// Recovered `if let` decision: a single synthetic boolean condition
/// (the pattern match) with its two arms.
struct IfLetExpr {
    cond: String,
    then_branch: Node,
    else_branch: Node,
}

/// Recovered `match` arm: one synthetic literal-equality condition
/// (e.g. `n == 0`) with its then/else arms. Multi-arm matches nest
/// these recursively.
struct MatchExpr {
    cond: String,
    then_branch: Node,
    else_branch: Node,
}

#[derive(Debug, Clone)]
struct Guard;

/// Recovered `?` operator: the Continue arm of the
/// `Try::branch` + `discriminant` + `switchInt` prefix. The early
/// return arm carries no user-level decision (it's just propagation
/// of the residual), so only the Continue arm matters for MC/DC.
struct TryExpr {
    continue_arm: BlockId,
}

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

// `if let` handler: wrap a recovered IfLetExpr into a Node::Ite where
// the condition is a synthetic name like `<scrutinee> is <Variant>`.
// Matches `handle_if_let` in decomposer.toml.
fn handle_if_let(expr: &IfLetExpr) -> Node {
    Node::Ite {
        cond: expr.cond.clone(),
        then_branch: Box::new(expr.then_branch.clone()),
        else_branch: Box::new(expr.else_branch.clone()),
    }
}

// `match` handler: wrap a recovered MatchExpr into a Node::Ite. The
// caller is responsible for unrolling multi-arm matches into nested
// MatchExpr applications (the right-leaning shape mirrors C-style
// `switch/case`). Match guards are out of MVP scope; they delegate
// to a no-op `handle_match_guard` per decomposer.toml's call graph.
fn handle_match(expr: &MatchExpr) -> Node {
    let _ = handle_match_guard(&Guard);
    Node::Ite {
        cond: expr.cond.clone(),
        then_branch: Box::new(expr.then_branch.clone()),
        else_branch: Box::new(expr.else_branch.clone()),
    }
}

// Match-guard handler. MVP scope is literal patterns only; guarded
// arms (`match n { 0 if c => ... }`) yield no decision. Kept as a
// reachable function so decomposer.toml's call graph stays valid
// when the Phase 2 implementation lands.
fn handle_match_guard(_guard: &Guard) -> Node {
    Node::Const { value: false }
}

// `?` handler: decompose the Continue arm. Skip-through semantics —
// the early-return arm is propagation, not a user decision, so the
// MC/DC content of a function with `?` is whatever logic the success
// path contains. Returns `None` if the Continue arm has no recoverable
// decision (e.g. a plain `let x = opt?; Some(x)`).
fn handle_try(expr: &TryExpr, f: &MirFunction) -> Option<Node> {
    build_decision_from_block(expr.continue_arm, f)
}

// ---------------------------------------------------------------------------
// CFG walker
// ---------------------------------------------------------------------------

/// Recursively convert one MIR block (and its CFG descendants) into a
/// [`Node`].
///
/// - Block sets the return value (`_0`) in its statements: emit that
///   value as the block's [`Node`]. The terminator is ignored even if
///   it's a `switchInt` — this is the load-bearing case for
///   coverage-instrumented MIR, which wraps each condition in an
///   extra `switchInt` *after* the value-setting copy.
/// - Branching block (`switchInt` terminator, no value-setting
///   statements): emit a [`Node::Ite`], recursing into the two
///   successor blocks.
/// - Terminal block (`Goto` / `Return`): emit the corresponding
///   [`Node::Const`] or [`Node::Condition`] if statements set `_0`,
///   else `None`.
///
/// Returns `None` if the block's shape isn't recognised — keeps the
/// engine conservative when corpus patterns exercise new shapes
/// before the engine learns them.
fn build_decision_from_block(block_id: BlockId, f: &MirFunction) -> Option<Node> {
    let block = f.blocks.iter().find(|b| b.id == block_id)?;

    // `?` operator: skip the `Try::branch` + `discriminant` prefix
    // and decompose the Continue arm. Chained `?`s recurse through
    // this same path. Must run before `extract_terminal_value` so
    // that a function whose entire body is `let x = a?; <expr>`
    // doesn't get misread as a no-decision block.
    if let Some(expr) = try_match_try_prefix(block_id, f) {
        return handle_try(&expr, f);
    }

    // If the block's statements set the return value, that IS the
    // block's value. We don't need to follow the terminator —
    // anything beyond that point is either a no-op coverage
    // counter or an instrumentation switchInt that doesn't change
    // the result. See the module-level docs for the canonical shapes.
    if let Some(value) = extract_terminal_value(block, f) {
        return Some(value);
    }

    // `if let` shape: a `discriminant(...)` assignment followed by a
    // multi-arm switchInt with one matched arm (identified by a
    // downcast statement) and one unmatched arm.
    if let Some(node) = try_if_let_from_block(block, f) {
        return Some(node);
    }

    match &block.terminator {
        MirTerminator::SwitchInt {
            discr,
            targets,
            otherwise,
            ..
        } => {
            // Bool short-circuit: single-arm `[0: bb_else, otherwise: bb_then]`
            // on a `bool` discriminant. The type check is what
            // distinguishes this case from a literal `match n { 0 =>
            // ..., _ => ... }` on an integer (same MIR shape, different
            // intent).
            let is_bool_discr = local_type(f, *discr) == Some("bool");
            if is_bool_discr && targets.len() == 1 && targets[0].0 == 0 {
                // Cond name: prefer the discriminant's debug name. If
                // none (the discriminant is a synthetic temporary —
                // typical when the source is an inline comparison like
                // `if speed > 50 { ... }`), synthesize one from a
                // preceding `AssignBinaryCompare` setting that local.
                let cond = match f.debug_names.get(discr) {
                    Some(name) => name.clone(),
                    None => synthesize_compare_name(block, *discr, f)?,
                };
                // `0` arm is the `else` branch (condition was false);
                // `otherwise` is the `then` branch (condition was true).
                let else_branch = build_decision_from_block(targets[0].1, f)?;
                let then_branch = build_decision_from_block(*otherwise, f)?;
                return Some(handle_boolean(&BoolExpr {
                    cond,
                    then_branch,
                    else_branch,
                }));
            }
            // Literal `match` on a non-bool integer.
            try_match_from_block(block, f, *discr, targets, *otherwise)
        }
        MirTerminator::Goto { .. }
        | MirTerminator::Return { .. }
        | MirTerminator::Unreachable { .. }
        | MirTerminator::Call { .. }
        | MirTerminator::Other { .. } => None,
    }
}

/// Type of a local as declared in the function header or `let`
/// declarations. Returns `None` for synthetic locals (return place,
/// temporaries) that don't appear in either list.
fn local_type(f: &MirFunction, local: LocalId) -> Option<&str> {
    f.args
        .iter()
        .find(|a| a.local == local)
        .map(|a| a.ty.as_str())
        .or_else(|| {
            f.locals
                .iter()
                .find(|l| l.local == local)
                .map(|l| l.ty.as_str())
        })
}

/// Recover a literal `match` decision rooted at `block`.
///
/// MVP scope is literal patterns only — integer literals on a non-
/// `bool` scrutinee. Enum matches (whose `switchInt` is preceded by
/// an `AssignDiscriminant`) are declined; engineers wanting MC/DC
/// on an enum variant should use `if let` with a binding so the
/// dedicated `if let` handler can recover the variant name.
///
/// Each explicit arm `v: bb_a` becomes one ITE level with condition
/// `<name> == <v>`. Multi-arm matches nest right-leaning, so
/// `match n { 0 => A, 1 => B, _ => C }` becomes
/// `Ite{n==0, A, Ite{n==1, B, C}}`. When the `otherwise` arm is
/// unreachable (exhaustive match), the last explicit arm becomes
/// the unconditional else.
fn try_match_from_block(
    block: &MirBlock,
    f: &MirFunction,
    discr: LocalId,
    targets: &[(u128, BlockId)],
    otherwise: BlockId,
) -> Option<Node> {
    let is_enum_discriminant = block.statements.iter().any(|s| {
        matches!(
            s,
            MirStatement::AssignDiscriminant { dst, .. } if *dst == discr
        )
    });
    if is_enum_discriminant {
        return None;
    }

    let scrutinee_name = f
        .debug_names
        .get(&discr)
        .cloned()
        .unwrap_or_else(|| format!("_{discr}"));

    // Reachable explicit arms, in declared order.
    let arms: Vec<(u128, BlockId)> = targets
        .iter()
        .copied()
        .filter(|(_, t)| !is_unreachable_block(*t, f))
        .collect();
    if arms.is_empty() {
        return None;
    }

    // Build from the bottom up so the produced ITE matches source-
    // order semantics (first arm wraps outermost).
    let other_reachable = !is_unreachable_block(otherwise, f);
    let mut iter = arms.into_iter();
    let (last_v, last_t) = iter.next_back()?;
    let mut tail = if other_reachable {
        let then_branch = build_decision_from_block(last_t, f)?;
        let else_branch = build_decision_from_block(otherwise, f)?;
        handle_match(&MatchExpr {
            cond: format!("{scrutinee_name} == {last_v}"),
            then_branch,
            else_branch,
        })
    } else {
        // Exhaustive match: the last arm is the only remaining
        // possibility, so it's the unconditional fallback. No ITE
        // wrapping needed for the last arm itself.
        build_decision_from_block(last_t, f)?
    };
    for (v, t) in iter.rev() {
        let then_branch = build_decision_from_block(t, f)?;
        tail = handle_match(&MatchExpr {
            cond: format!("{scrutinee_name} == {v}"),
            then_branch,
            else_branch: tail,
        });
    }
    Some(tail)
}

/// Recognise a `?` operator's MIR prefix rooted at `block_id`.
///
/// The shape is:
///
/// ```text
/// bb_call: { _D = <T as Try>::branch(<scrutinee>) -> [return: bb_disc, ...]; }
/// bb_disc: { _E = discriminant(_D); switchInt(move _E) -> [v1: bb_a, v2: bb_b, otherwise: bb_other]; }
/// ```
///
/// where one of the two reachable arms (the Continue arm) begins
/// with `_X = copy ((_D as Continue).0: <Output>);` — that arm is
/// the rest of the user's code; the other arm is the early-return
/// path (a `from_residual` call). Returns a [`TryExpr`] pointing at
/// the Continue arm if all of that lines up, otherwise `None`.
fn try_match_try_prefix(block_id: BlockId, f: &MirFunction) -> Option<TryExpr> {
    let block = f.blocks.iter().find(|b| b.id == block_id)?;
    let (call_dst, call_target) = match &block.terminator {
        MirTerminator::Call { dst, target, .. } => (*dst, *target),
        _ => return None,
    };

    let disc_block = f.blocks.iter().find(|b| b.id == call_target)?;
    let discr_local = disc_block.statements.iter().find_map(|s| match s {
        MirStatement::AssignDiscriminant { dst, src, .. } if *src == call_dst => Some(*dst),
        _ => None,
    })?;
    let (targets, otherwise) = match &disc_block.terminator {
        MirTerminator::SwitchInt {
            discr,
            targets,
            otherwise,
            ..
        } if *discr == discr_local => (targets, *otherwise),
        _ => return None,
    };

    let mut reachable: Vec<BlockId> = Vec::new();
    for &(_, t) in targets {
        if !is_unreachable_block(t, f) && !reachable.contains(&t) {
            reachable.push(t);
        }
    }
    if !is_unreachable_block(otherwise, f) && !reachable.contains(&otherwise) {
        reachable.push(otherwise);
    }
    if reachable.len() != 2 {
        return None;
    }

    let continue_arm = reachable.iter().copied().find(|&id| {
        let Some(b) = f.blocks.iter().find(|b| b.id == id) else {
            return false;
        };
        b.statements.iter().any(|s| {
            matches!(
                s,
                MirStatement::AssignDowncast { src, variant, .. }
                    if *src == call_dst && variant == "Continue"
            )
        })
    })?;
    Some(TryExpr { continue_arm })
}

/// Recover an `if let` decision rooted at `block`, if its shape matches.
///
/// The MIR shape rustc emits for `if let <Pat> = <scrutinee> { A } else { B }`
/// is:
///
/// ```text
/// bb0: {
///     _D = discriminant(_S);
///     switchInt(move _D) -> [v1: bb_a, v2: bb_b, ..., otherwise: bb_other];
/// }
/// ```
///
/// where one of the target blocks (the matched arm) begins with a
/// downcast statement `_X = copy ((_S as <Variant>).<field>: <Ty>);`
/// and the other (the unmatched arm) does not. Any arm whose block
/// has an `unreachable;` terminator is filtered out — the compiler
/// emits one such arm when every enum variant is covered by an
/// explicit value, so it doesn't represent a real path.
///
/// Returns `None` if the block doesn't match this shape, leaving the
/// caller to try the next decoder (e.g. boolean short-circuit).
fn try_if_let_from_block(block: &MirBlock, f: &MirFunction) -> Option<Node> {
    let (discr_local, scrutinee) = block.statements.iter().find_map(|s| match s {
        MirStatement::AssignDiscriminant { dst, src, .. } => Some((*dst, *src)),
        _ => None,
    })?;

    let (targets, otherwise) = match &block.terminator {
        MirTerminator::SwitchInt {
            discr,
            targets,
            otherwise,
            ..
        } if *discr == discr_local => (targets, *otherwise),
        _ => return None,
    };

    // Collect each distinct, reachable target block in switchInt order
    // (explicit arms first, then `otherwise`). Distinctness keeps the
    // matched/unmatched identification well-defined even if two arms
    // share a successor.
    let mut reachable: Vec<BlockId> = Vec::new();
    for &(_, t) in targets {
        if !is_unreachable_block(t, f) && !reachable.contains(&t) {
            reachable.push(t);
        }
    }
    if !is_unreachable_block(otherwise, f) && !reachable.contains(&otherwise) {
        reachable.push(otherwise);
    }
    if reachable.len() != 2 {
        return None;
    }

    let matched_idx = reachable.iter().enumerate().find_map(|(i, &id)| {
        let b = f.blocks.iter().find(|b| b.id == id)?;
        downcast_variant_for(b, scrutinee).map(|_| i)
    })?;
    let matched_id = reachable[matched_idx];
    let unmatched_id = reachable[1 - matched_idx];

    let scrutinee_name = f
        .debug_names
        .get(&scrutinee)
        .cloned()
        .unwrap_or_else(|| format!("_{scrutinee}"));
    let matched_block = f.blocks.iter().find(|b| b.id == matched_id)?;
    let variant_name = downcast_variant_for(matched_block, scrutinee);
    let cond = match variant_name {
        Some(v) => format!("{scrutinee_name} is {v}"),
        None => format!("{scrutinee_name} matches"),
    };

    let then_branch = build_decision_from_block(matched_id, f)?;
    let else_branch = build_decision_from_block(unmatched_id, f)?;
    Some(handle_if_let(&IfLetExpr {
        cond,
        then_branch,
        else_branch,
    }))
}

fn is_unreachable_block(id: BlockId, f: &MirFunction) -> bool {
    f.blocks
        .iter()
        .find(|b| b.id == id)
        .map(|b| matches!(b.terminator, MirTerminator::Unreachable { .. }))
        .unwrap_or(false)
}

fn downcast_variant_for(b: &MirBlock, scrutinee: LocalId) -> Option<String> {
    b.statements.iter().find_map(|s| match s {
        MirStatement::AssignDowncast { src, variant, .. } if *src == scrutinee => {
            Some(variant.clone())
        }
        _ => None,
    })
}

/// Extract a leaf [`Node`] from a terminal block.
///
/// Looks for an `AssignConstBool` or `AssignCopy` statement that sets
/// the return value, in source order. The first match wins (Phase 0
/// blocks have at most one such statement).
fn extract_terminal_value(block: &MirBlock, f: &MirFunction) -> Option<Node> {
    // An assignment is a terminal value only if its destination
    // doesn't feed the block's terminator. Specifically, when the
    // terminator is `switchInt(_N)`, any assignment to `_N` in this
    // block is setup for that branch and must not preempt the
    // terminator-side decoder — for example,
    //   `_3 = Gt(copy _1, const 50_i32);`
    //   `switchInt(move _3) -> [...]`
    // sets up a bool temporary that the terminator branches on,
    // and only the terminator side knows the right name to give the
    // condition. Assignments to other locals (e.g. `_0 = copy _3`,
    // `_6 = const false` inside a `?`-skip-through arm) are still
    // recognised as terminal values.
    let switchint_discr = match &block.terminator {
        MirTerminator::SwitchInt { discr, .. } => Some(*discr),
        _ => None,
    };
    let feeds_switchint = |dst: LocalId| switchint_discr == Some(dst);

    for stmt in &block.statements {
        match stmt {
            MirStatement::AssignConstBool { dst, value, .. } if !feeds_switchint(*dst) => {
                return Some(Node::Const { value: *value });
            }
            MirStatement::AssignCopy { dst, src, .. } if !feeds_switchint(*dst) => {
                if let Some(name) = f.debug_names.get(src) {
                    return Some(Node::Condition { name: name.clone() });
                }
            }
            MirStatement::AssignBinaryCompare {
                dst, op, lhs, rhs, ..
            } if !feeds_switchint(*dst) => {
                return Some(Node::Condition {
                    name: compare_condition_name(*op, lhs, rhs, f),
                });
            }
            // Everything else (temporaries that feed a switchInt,
            // downcasts, discriminants, unrecognised shapes) is
            // informational for downstream decoders.
            _ => {}
        }
    }
    None
}

/// Look for an `AssignBinaryCompare` in `block` that writes to
/// `discr`, and synthesize a condition name from it. Returns `None`
/// if no matching compare is found.
///
/// Used when a `switchInt` discriminates on a bool temporary with
/// no debug name — the canonical shape rustc emits for inline
/// comparisons like `if speed > 50 && brake`.
fn synthesize_compare_name(block: &MirBlock, discr: LocalId, f: &MirFunction) -> Option<String> {
    block.statements.iter().find_map(|s| match s {
        MirStatement::AssignBinaryCompare {
            dst, op, lhs, rhs, ..
        } if *dst == discr => Some(compare_condition_name(*op, lhs, rhs, f)),
        _ => None,
    })
}

/// Synthesize a human-readable condition name for an inline
/// comparison like `speed > 50` or `a < b`.
///
/// Locals render via their debug name (`speed`, `a`); unnamed
/// locals fall back to `_<N>`. Constants render via their type-
/// stripped form (so `50_i32` reads as `50`).
fn compare_condition_name(op: CompareOp, lhs: &Operand, rhs: &Operand, f: &MirFunction) -> String {
    format!(
        "{} {} {}",
        operand_display(lhs, f),
        op.as_source_str(),
        operand_display(rhs, f)
    )
}

fn operand_display(op: &Operand, f: &MirFunction) -> String {
    match op {
        Operand::Copy(id) | Operand::Move(id) => f
            .debug_names
            .get(id)
            .cloned()
            .unwrap_or_else(|| format!("_{id}")),
        Operand::Const(_) => op.const_display_value().to_string(),
    }
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

    /// `rustc --emit=mir` output for
    /// `fn decide(opt: Option<bool>) -> bool { if let Some(x) = opt { x } else { false } }`.
    const DECIDE_IF_LET_SIMPLE_MIR: &str = r#"
fn decide(_1: Option<bool>) -> bool {
    debug opt => _1;
    let mut _0: bool;
    let mut _2: isize;
    scope 1 {
        debug x => _3;
        let _3: bool;
    }

    bb0: {
        _2 = discriminant(_1);
        switchInt(move _2) -> [1: bb1, 0: bb2, otherwise: bb4];
    }

    bb1: {
        _3 = copy ((_1 as Some).0: bool);
        _0 = copy _3;
        goto -> bb3;
    }

    bb2: {
        _0 = const false;
        goto -> bb3;
    }

    bb3: {
        return;
    }

    bb4: {
        unreachable;
    }
}
"#;

    /// `rustc --emit=mir` output for
    /// `fn decide(opt: Option<bool>, b: bool) -> bool { if let Some(x) = opt { x && b } else { false } }`.
    const DECIDE_IF_LET_WITH_AND_MIR: &str = r#"
fn decide(_1: Option<bool>, _2: bool) -> bool {
    debug opt => _1;
    debug b => _2;
    let mut _0: bool;
    let mut _3: isize;
    scope 1 {
        debug x => _4;
        let _4: bool;
    }

    bb0: {
        _3 = discriminant(_1);
        switchInt(move _3) -> [1: bb1, 0: bb4, otherwise: bb6];
    }

    bb1: {
        _4 = copy ((_1 as Some).0: bool);
        switchInt(copy _4) -> [0: bb3, otherwise: bb2];
    }

    bb2: {
        _0 = copy _2;
        goto -> bb5;
    }

    bb3: {
        _0 = const false;
        goto -> bb5;
    }

    bb4: {
        _0 = const false;
        goto -> bb5;
    }

    bb5: {
        return;
    }

    bb6: {
        unreachable;
    }
}
"#;

    /// `rustc --emit=mir` output for
    /// `fn decide(r: Result<bool, ()>) -> bool { if let Ok(v) = r { v } else { false } }`.
    /// Note the arm value ordering is `[0: bb1, 1: bb2]` — Ok = 0, Err = 1.
    /// The decomposer must not rely on a specific variant integer.
    const DECIDE_IF_LET_RESULT_MIR: &str = r#"
fn decide(_1: Result<bool, ()>) -> bool {
    debug r => _1;
    let mut _0: bool;
    let mut _2: isize;
    scope 1 {
        debug v => _3;
        let _3: bool;
    }

    bb0: {
        _2 = discriminant(_1);
        switchInt(move _2) -> [0: bb1, 1: bb2, otherwise: bb4];
    }

    bb1: {
        _3 = copy ((_1 as Ok).0: bool);
        _0 = copy _3;
        goto -> bb3;
    }

    bb2: {
        _0 = const false;
        goto -> bb3;
    }

    bb3: {
        return;
    }

    bb4: {
        unreachable;
    }
}
"#;

    /// `rustc --emit=mir` output for
    /// `fn decide(t: Three) -> bool { if let Three::A(v) = t { v } else { false } }`
    /// with `enum Three { A(bool), B, C }`. The unmatched arm is the
    /// `otherwise` target (rustc collapses the two non-matched variants
    /// into a single fallthrough; there is no `unreachable` arm here).
    const DECIDE_IF_LET_THREE_VARIANT_MIR: &str = r#"
fn decide(_1: Three) -> bool {
    debug t => _1;
    let mut _0: bool;
    let mut _2: isize;
    scope 1 {
        debug v => _3;
        let _3: bool;
    }

    bb0: {
        _2 = discriminant(_1);
        switchInt(move _2) -> [0: bb1, otherwise: bb2];
    }

    bb1: {
        _3 = copy ((_1 as A).0: bool);
        _0 = copy _3;
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

    /// `rustc --emit=mir` output for
    /// `fn decide(opt: Option<bool>) -> bool { if let Some(_) = opt { true } else { false } }`.
    /// No binding => no downcast => the decomposer cannot tell the
    /// matched arm from the unmatched arm by structure alone, so it
    /// declines (returns no decision) rather than guess.
    const DECIDE_IF_LET_NO_BINDING_MIR: &str = r#"
fn decide(_1: Option<bool>) -> bool {
    debug opt => _1;
    let mut _0: bool;
    let mut _2: isize;
    scope 1 {
    }

    bb0: {
        _2 = discriminant(_1);
        switchInt(move _2) -> [1: bb1, 0: bb2, otherwise: bb4];
    }

    bb1: {
        _0 = const true;
        goto -> bb3;
    }

    bb2: {
        _0 = const false;
        goto -> bb3;
    }

    bb3: {
        return;
    }

    bb4: {
        unreachable;
    }
}
"#;

    #[test]
    fn decompose_recognises_if_let_some_as_ite() {
        // if let Some(x) = opt { x } else { false }
        //   <=>   if (opt is Some) then x else false
        let mir = mir::parse_text(DECIDE_IF_LET_SIMPLE_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite("opt is Some", cond("x"), const_(false))
        );
    }

    #[test]
    fn decompose_recognises_if_let_combined_with_and() {
        // if let Some(x) = opt { x && b } else { false }
        //   <=>   if (opt is Some) then (if x then b else false) else false
        // The inner `x && b` is the existing boolean handler; this
        // confirms that an `if let` arm recursively decomposes through
        // the rest of the engine without any special-casing.
        let mir = mir::parse_text(DECIDE_IF_LET_WITH_AND_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite(
                "opt is Some",
                ite("x", cond("b"), const_(false)),
                const_(false),
            )
        );
    }

    #[test]
    fn decompose_handles_result_variant_ordering() {
        // Result swaps the arm-value order (Ok = 0, Err = 1). The
        // decomposer must locate the matched arm by structure (the
        // downcast statement), not by arm index.
        let mir = mir::parse_text(DECIDE_IF_LET_RESULT_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], ite("r is Ok", cond("v"), const_(false)));
    }

    #[test]
    fn decompose_handles_if_let_with_otherwise_unmatched() {
        // Three-variant enum: the unmatched arm is the `otherwise`
        // target (no `unreachable` block emitted by rustc). The
        // matched arm is still identified by the downcast.
        let mir = mir::parse_text(DECIDE_IF_LET_THREE_VARIANT_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], ite("t is A", cond("v"), const_(false)));
    }

    #[test]
    fn decompose_declines_no_binding_if_let() {
        // if let Some(_) = opt { true } else { false }
        // No downcast => the decomposer can't tell matched from
        // unmatched by structure. It declines to guess.
        let mir = mir::parse_text(DECIDE_IF_LET_NO_BINDING_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert!(tree.decisions.is_empty());
    }

    /// `rustc --emit=mir` output for
    /// `fn decide(speed: i32, brake: bool) -> bool { speed > 50 && brake }`.
    /// The dominant inline-comparison + `&&` shape. `_3` is the
    /// synthetic bool temporary holding the comparison result; it
    /// has no debug name, so the decomposer must synthesize
    /// `speed > 50` from the `Gt` statement preceding the
    /// `switchInt`.
    const DECIDE_INLINE_CMP_AND_MIR: &str = r#"
fn decide(_1: i32, _2: bool) -> bool {
    debug speed => _1;
    debug brake => _2;
    let mut _0: bool;
    let mut _3: bool;

    bb0: {
        _3 = Gt(copy _1, const 50_i32);
        switchInt(move _3) -> [0: bb2, otherwise: bb1];
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

    /// `fn decide(code: u8) -> bool { code == 1 }` — the comparison
    /// IS the function's return value. No switchInt at all.
    const DECIDE_DIRECT_EQ_MIR: &str = r#"
fn decide(_1: u8) -> bool {
    debug code => _1;
    let mut _0: bool;

    bb0: {
        _0 = Eq(copy _1, const 1_u8);
        return;
    }
}
"#;

    /// `fn decide(state: u32, ovr: bool) -> bool { state != 0 || ovr }`
    /// — Ne paired with `||`, exercising the OR arm shape.
    const DECIDE_NE_OR_MIR: &str = r#"
fn decide(_1: u32, _2: bool) -> bool {
    debug state => _1;
    debug ovr => _2;
    let mut _0: bool;
    let mut _3: bool;

    bb0: {
        _3 = Ne(copy _1, const 0_u32);
        switchInt(move _3) -> [0: bb2, otherwise: bb1];
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

    /// `fn decide(value: i32) -> bool { value >= 0 && value <= 100 }`
    /// — two comparisons composed with `&&`. The outer switchInt
    /// branches on the Ge result; the matched arm has its own
    /// inline-compare leaf (`value <= 100`).
    const DECIDE_RANGE_MIR: &str = r#"
fn decide(_1: i32) -> bool {
    debug value => _1;
    let mut _0: bool;
    let mut _2: bool;

    bb0: {
        _2 = Ge(copy _1, const 0_i32);
        switchInt(move _2) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _0 = Le(copy _1, const 100_i32);
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

    /// `fn decide(a: i32, b: i32) -> bool { a < b }` — comparison
    /// between two named locals (no const operand).
    const DECIDE_VAR_VAR_MIR: &str = r#"
fn decide(_1: i32, _2: i32) -> bool {
    debug a => _1;
    debug b => _2;
    let mut _0: bool;

    bb0: {
        _0 = Lt(copy _1, copy _2);
        return;
    }
}
"#;

    #[test]
    fn decompose_recovers_inline_comparison_with_and() {
        // speed > 50 && brake
        //   <=>   if (speed > 50) then brake else false
        let mir = mir::parse_text(DECIDE_INLINE_CMP_AND_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite("speed > 50", cond("brake"), const_(false))
        );
    }

    #[test]
    fn decompose_recovers_direct_comparison_as_condition() {
        // code == 1 — the comparison is itself the function's value.
        let mir = mir::parse_text(DECIDE_DIRECT_EQ_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], cond("code == 1"));
    }

    #[test]
    fn decompose_recovers_ne_paired_with_or() {
        // state != 0 || ovr
        //   <=>   if (state != 0) then true else ovr
        let mir = mir::parse_text(DECIDE_NE_OR_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite("state != 0", const_(true), cond("ovr"))
        );
    }

    #[test]
    fn decompose_recovers_two_comparisons_in_one_decision() {
        // value >= 0 && value <= 100
        let mir = mir::parse_text(DECIDE_RANGE_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite("value >= 0", cond("value <= 100"), const_(false))
        );
    }

    #[test]
    fn decompose_recovers_variable_to_variable_comparison() {
        // a < b — both operands are named locals.
        let mir = mir::parse_text(DECIDE_VAR_VAR_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], cond("a < b"));
    }

    /// `rustc --emit=mir` output for
    /// `fn decide(n: i32) -> bool { match n { 0 => false, _ => true } }`.
    const DECIDE_MATCH_INT_TWO_MIR: &str = r#"
fn decide(_1: i32) -> bool {
    debug n => _1;
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _0 = const true;
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

    /// `rustc --emit=mir` output for
    /// `fn decide(n: i32) -> bool { match n { 0 => false, 1 => true, _ => false } }`.
    const DECIDE_MATCH_INT_THREE_MIR: &str = r#"
fn decide(_1: i32) -> bool {
    debug n => _1;
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb3, 1: bb2, otherwise: bb1];
    }

    bb1: {
        _0 = const false;
        goto -> bb4;
    }

    bb2: {
        _0 = const true;
        goto -> bb4;
    }

    bb3: {
        _0 = const false;
        goto -> bb4;
    }

    bb4: {
        return;
    }
}
"#;

    /// `rustc --emit=mir` output for
    /// `fn decide(b: bool) -> bool { match b { true => true, false => false } }`.
    /// Same shape as `match n { 0 => ..., _ => ... }` but `_1` is
    /// `bool` — the engine must keep using the bool short-circuit
    /// handler (condition name is just `b`, not `b == 0`).
    const DECIDE_MATCH_BOOL_MIR: &str = r#"
fn decide(_1: bool) -> bool {
    debug b => _1;
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb1, otherwise: bb2];
    }

    bb1: {
        _0 = const false;
        goto -> bb3;
    }

    bb2: {
        _0 = const true;
        goto -> bb3;
    }

    bb3: {
        return;
    }
}
"#;

    /// `rustc --emit=mir` output for
    /// `fn decide(m: Mode) -> bool { match m { Mode::On => true, Mode::Off => false } }`.
    /// Enum match without binding — declined for MVP. The discriminant
    /// is produced by an `AssignDiscriminant`, which both the if-let
    /// detector and the literal-match handler use as a "skip me" signal.
    const DECIDE_MATCH_ENUM_NO_BINDING_MIR: &str = r#"
fn decide(_1: Mode) -> bool {
    debug m => _1;
    let mut _0: bool;
    let mut _2: isize;

    bb0: {
        _2 = discriminant(_1);
        switchInt(move _2) -> [0: bb3, 1: bb2, otherwise: bb1];
    }

    bb1: {
        unreachable;
    }

    bb2: {
        _0 = const false;
        goto -> bb4;
    }

    bb3: {
        _0 = const true;
        goto -> bb4;
    }

    bb4: {
        return;
    }
}
"#;

    #[test]
    fn decompose_recognises_match_two_arm_int() {
        // `match n { 0 => false, _ => true }`
        //   <=>   if (n == 0) then false else true
        let mir = mir::parse_text(DECIDE_MATCH_INT_TWO_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite("n == 0", const_(false), const_(true))
        );
    }

    #[test]
    fn decompose_recognises_match_three_arm_int_as_nested_ite() {
        // `match n { 0 => false, 1 => true, _ => false }`
        //   <=>   if (n == 0) then false else (if (n == 1) then true else false)
        let mir = mir::parse_text(DECIDE_MATCH_INT_THREE_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite(
                "n == 0",
                const_(false),
                ite("n == 1", const_(true), const_(false)),
            )
        );
    }

    #[test]
    fn decompose_match_bool_keeps_bool_naming() {
        // `match b { true => true, false => false }` shares MIR shape
        // with `match n { 0 => ..., _ => ... }` — the bool handler
        // must still fire (cond name `b`, not `b == 0`).
        let mir = mir::parse_text(DECIDE_MATCH_BOOL_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], ite("b", const_(true), const_(false)));
    }

    #[test]
    fn decompose_declines_enum_match_without_binding() {
        // MVP scope: enum-discriminant matches (no bindings) yield no
        // decision. Engineers wanting MC/DC on an enum variant
        // should use `if let` with a binding.
        let mir = mir::parse_text(DECIDE_MATCH_ENUM_NO_BINDING_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert!(tree.decisions.is_empty());
    }

    /// `rustc --emit=mir` output for
    /// `fn decide(opt: Option<bool>) -> Option<bool> { let x = opt?; Some(x) }`.
    /// Plain `?` followed by a non-decision body — no recoverable
    /// MC/DC content. The engine should decline cleanly, not crash.
    const DECIDE_TRY_PLAIN_MIR: &str = r#"
fn decide(_1: Option<bool>) -> Option<bool> {
    debug opt => _1;
    let mut _0: std::option::Option<bool>;
    let mut _2: std::ops::ControlFlow<std::option::Option<std::convert::Infallible>, bool>;
    let mut _3: isize;
    let _4: bool;
    scope 1 {
        debug x => _4;
    }

    bb0: {
        _2 = <Option<bool> as Try>::branch(copy _1) -> [return: bb1, unwind continue];
    }

    bb1: {
        _3 = discriminant(_2);
        switchInt(move _3) -> [0: bb3, 1: bb4, otherwise: bb2];
    }

    bb2: {
        unreachable;
    }

    bb3: {
        _4 = copy ((_2 as Continue).0: bool);
        _0 = Option::<bool>::Some(copy _4);
        goto -> bb5;
    }

    bb4: {
        _0 = <Option<bool> as FromResidual<Option<Infallible>>>::from_residual(const Option::<Infallible>::None) -> [return: bb5, unwind continue];
    }

    bb5: {
        return;
    }
}
"#;

    /// `rustc --emit=mir` output for
    /// `fn decide(opt: Option<bool>, b: bool) -> Option<bool> { let x = opt?; Some(x && b) }`.
    /// The `?` is plumbing; the real boolean decision is `x && b`
    /// inside the Continue arm. The engine should look through the
    /// `?` and recover the inner `&&`.
    const DECIDE_TRY_WITH_AND_MIR: &str = r#"
fn decide(_1: Option<bool>, _2: bool) -> Option<bool> {
    debug opt => _1;
    debug b => _2;
    let mut _0: std::option::Option<bool>;
    let mut _3: std::ops::ControlFlow<std::option::Option<std::convert::Infallible>, bool>;
    let mut _4: isize;
    let _5: bool;
    let mut _6: bool;
    scope 1 {
        debug x => _5;
    }

    bb0: {
        _3 = <Option<bool> as Try>::branch(copy _1) -> [return: bb1, unwind continue];
    }

    bb1: {
        _4 = discriminant(_3);
        switchInt(move _4) -> [0: bb3, 1: bb4, otherwise: bb2];
    }

    bb2: {
        unreachable;
    }

    bb3: {
        _5 = copy ((_3 as Continue).0: bool);
        switchInt(copy _5) -> [0: bb6, otherwise: bb5];
    }

    bb4: {
        _0 = <Option<bool> as FromResidual<Option<Infallible>>>::from_residual(const Option::<Infallible>::None) -> [return: bb8, unwind continue];
    }

    bb5: {
        _6 = copy _2;
        goto -> bb7;
    }

    bb6: {
        _6 = const false;
        goto -> bb7;
    }

    bb7: {
        _0 = Option::<bool>::Some(move _6);
        goto -> bb8;
    }

    bb8: {
        return;
    }
}
"#;

    #[test]
    fn decompose_declines_plain_try() {
        // `let x = opt?; Some(x)` — no internal boolean decision; the
        // skip-through lands on the Continue arm which has no
        // recoverable structure, so the engine declines (no panic).
        let mir = mir::parse_text(DECIDE_TRY_PLAIN_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert!(tree.decisions.is_empty());
    }

    #[test]
    fn decompose_looks_through_try_to_find_inner_and() {
        // `let x = opt?; Some(x && b)` — the `?` is plumbing; the
        // engine looks through it and recovers `x && b` as the
        // decision.
        let mir = mir::parse_text(DECIDE_TRY_WITH_AND_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], ite("x", cond("b"), const_(false)));
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
