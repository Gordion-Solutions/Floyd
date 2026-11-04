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
//! const false / copy <debug-named-local>` leaves. A copied leaf is a
//! condition only when its local is declared `bool` (or, for a
//! dereference copy, a reference or pointer to `bool`); see
//! [`DeclineReason::NonBoolLeaf`]. The remaining
//! handlers (`if let`, `match`, match guards, `?`) are stubs that
//! graduate alongside their corpus patterns.
//!
//! ## Lifts: conditions whose arms are not Boolean
//!
//! For MC/DC the decision is the Boolean condition expression; what
//! its arms compute does not matter. When the arms of a `bool`
//! `switchInt` produce no Boolean leaf, two lifts take the arm taken
//! as the decision's outcome (`then` arm `true`, `else` arm
//! `false`), and the conditions' `llvm-cov` branch counters supply
//! the observations exactly as for a Boolean-leaf decision. Which
//! lift fired is reported as a [`Lift`].
//!
//! - **Call arms** ([`Lift::CallArms`]): both arms of one `switchInt`
//!   end in non-panic calls or checked-operation `assert`s that
//!   rejoin without diverging (`arms_are_non_panic_calls`).
//! - **Value arms** ([`Lift::ValueArms`]): tried only at the
//!   decomposition root, and only when every other handler declined.
//!   The root's `bool` `switchInt`, and every `bool` `switchInt` it
//!   reaches through coverage-only `goto` blocks, form a
//!   short-circuit condition chain; the chain must end in exactly two
//!   arm blocks that run through `goto`s only (no call, `assert`,
//!   branch or return) into one join block outside the chain, and
//!   both arms must write one common non-`bool` local before the
//!   join (`if upper { 7 } else { 39 }`, `if -86400 < secs && secs <
//!   86400 { Some(..) } else { None }`). A third arm (an `if` nested
//!   in an arm, an `else if`), an arm that loops, calls or returns,
//!   arms writing different locals, an `if` without `else`, and a
//!   `bool` join value all decline. See `lift_value_arms`.
//!
//! Both lifts label outcomes by `switchInt` edge, so for a negated
//! last condition (`if a && !b`) the source `else` arm is the `true`
//! outcome. Independence pairs are the same under either labelling.
//! Leaves stay Boolean: a non-`bool` local is never a condition.

use std::cell::Cell;

use crate::mir::{
    BlockId, CompareOp, LocalId, MirBlock, MirFunction, MirStatement, MirTerminator, Operand,
};
use crate::Mir;

/// Why the decomposer declined a function that has a `switchInt`.
///
/// Serialised in snake case (`unsupported_shape`, `non_bool_leaf`,
/// `no_conditions`, `ctfe_body`)
/// as the `reason` of each `out_of_scope` entry in
/// `cargo floyd test --format=json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclineReason {
    /// The control flow does not match any decision shape the
    /// engine's handlers recognise.
    UnsupportedShape,
    /// An arm's value is a debug-named local that is not `bool`
    /// (for example `if sh == 0 { a } else { .. }` returning a
    /// struct `a`). The value is not a Boolean condition, so the
    /// arm, and with it the decision, is declined instead of
    /// reporting the local as a condition.
    NonBoolLeaf,
    /// The function decomposed, but the tree tests no condition (a
    /// bare `Const`). There is nothing for MC/DC to measure; such a
    /// tree comes from a body whose entry block reaches a constant
    /// without branching, not from a Boolean decision.
    NoConditions,
    /// The body is the `// MIR FOR CTFE` rendering of a `const fn`
    /// ([`MirFunction::is_ctfe`]). It is not decomposed: it never
    /// runs at runtime, and the function's runtime body, parsed
    /// separately under the same name, is the candidate.
    CtfeBody,
}

/// Which lift turned a Boolean condition with non-Boolean arms into
/// a decision whose outcome is the arm taken.
///
/// Recorded per candidate (`lift` in `cargo floyd test
/// --format=json`) for triage. Serialised in snake case
/// (`call_arms`, `value_arms`). A decision with no lift reads its
/// outcome from Boolean leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lift {
    /// Both arms of a `bool` `switchInt` end in non-panic calls or
    /// checked-operation `assert`s (see `arms_are_non_panic_calls`).
    CallArms,
    /// Both arms of a `bool` condition (or a short-circuit chain of
    /// them) are straight-line blocks writing one non-`bool` value
    /// before they join (see `lift_value_arms`).
    ValueArms,
}

thread_local! {
    /// Set when the call-arm lift fires during the current
    /// [`decompose_from_block`] call. Read only when that call
    /// returns a decision.
    static CALL_ARMS_LIFT_SEEN: Cell<bool> = const { Cell::new(false) };
    /// Set when [`extract_terminal_value`] rejects a non-`bool` leaf
    /// during the current [`try_decompose_from_block`] call. Read
    /// only when that call declines, to report
    /// [`DeclineReason::NonBoolLeaf`].
    static NON_BOOL_LEAF_SEEN: Cell<bool> = const { Cell::new(false) };
}

/// Record that a leaf was declined because its local is not `bool`.
fn note_non_bool_leaf() {
    NON_BOOL_LEAF_SEEN.with(|seen| seen.set(true));
}

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
        if let Some(node) = decompose_function(f) {
            tree.decisions.push(node);
        }
    }
    tree
}

/// Decompose a single MIR function into one [`Node`], if the engine
/// recognises its control-flow shape.
///
/// Callers that need to associate a decision with the function it
/// came from (for provenance reporting, per-function analysis, etc.)
/// can iterate `mir.functions` and call this per function, keeping
/// the `&MirFunction` reference alive alongside the resulting node.
///
/// Only the function's entry block is considered. For functions that
/// compose multiple decisions into a non-bool output (tuple returns,
/// struct returns, array returns), use [`decompose_from_block`] with
/// each candidate entry block to recover the remaining decisions —
/// typically a job for a downstream multi-decision pass rather than
/// the engine itself.
pub fn decompose_function(f: &MirFunction) -> Option<Node> {
    let entry = f.blocks.first()?;
    decompose_from_block(f, entry.id)
}

/// Decompose a function starting from a specific basic block.
///
/// Equivalent to [`decompose_function`] when `block_id` is the
/// function's entry block. Different `block_id` values let callers
/// explore alternate decision paths through the same function — the
/// building block for recovering tuple-returning or struct-returning
/// functions where each returned element is produced by its own
/// decision rooted at its own basic block.
///
/// Returns `None` when the shape rooted at `block_id` is not
/// recognised as a decision by any decomposer handler, or when
/// `block_id` does not identify a block in `f`.
pub fn decompose_from_block(f: &MirFunction, block_id: BlockId) -> Option<Node> {
    decompose_from_block_lifted(f, block_id).map(|(node, _)| node)
}

/// [`decompose_from_block`], with the [`Lift`] that produced the
/// decision, if any. The ordinary handlers run first; only when they
/// decline is the value-arm lift tried at `block_id`.
fn decompose_from_block_lifted(f: &MirFunction, block_id: BlockId) -> Option<(Node, Option<Lift>)> {
    if !f.blocks.iter().any(|b| b.id == block_id) {
        return None;
    }
    CALL_ARMS_LIFT_SEEN.with(|seen| seen.set(false));
    let node = build_decision_from_block(block_id, f);
    let call_arms = CALL_ARMS_LIFT_SEEN.with(|seen| seen.replace(false));
    match node {
        Some(node) => Some((node, call_arms.then_some(Lift::CallArms))),
        None => lift_value_arms(block_id, f).map(|node| (node, Some(Lift::ValueArms))),
    }
}

/// [`decompose_function`], with the reason when it declines.
pub fn try_decompose_function(f: &MirFunction) -> Result<Node, DeclineReason> {
    match f.blocks.first() {
        Some(entry) => try_decompose_from_block(f, entry.id),
        None => Err(DeclineReason::UnsupportedShape),
    }
}

/// [`decompose_from_block`], with the reason when it declines.
///
/// The reason is [`DeclineReason::NonBoolLeaf`] when a leaf on the
/// attempted decomposition was rejected because its local is not
/// `bool`, otherwise [`DeclineReason::UnsupportedShape`].
pub fn try_decompose_from_block(f: &MirFunction, block_id: BlockId) -> Result<Node, DeclineReason> {
    try_decompose_from_block_lifted(f, block_id).map(|(node, _)| node)
}

/// [`try_decompose_from_block`], with the [`Lift`] that produced the
/// decision, if any.
fn try_decompose_from_block_lifted(
    f: &MirFunction,
    block_id: BlockId,
) -> Result<(Node, Option<Lift>), DeclineReason> {
    NON_BOOL_LEAF_SEEN.with(|seen| seen.set(false));
    let node = decompose_from_block_lifted(f, block_id);
    let non_bool_leaf = NON_BOOL_LEAF_SEEN.with(|seen| seen.replace(false));
    node.ok_or(if non_bool_leaf {
        DeclineReason::NonBoolLeaf
    } else {
        DeclineReason::UnsupportedShape
    })
}

/// Decompose `f` as an MC/DC candidate: decline a CTFE body
/// ([`DeclineReason::CtfeBody`]) without decomposing it, otherwise
/// [`try_decompose_function`], then decline a tree that tests no
/// condition ([`DeclineReason::NoConditions`]).
pub fn try_decompose_candidate(f: &MirFunction) -> Result<Node, DeclineReason> {
    try_decompose_candidate_lifted(f).map(|(node, _)| node)
}

/// [`try_decompose_candidate`], with the [`Lift`] that produced the
/// decision, if any.
pub fn try_decompose_candidate_lifted(
    f: &MirFunction,
) -> Result<(Node, Option<Lift>), DeclineReason> {
    if f.is_ctfe {
        return Err(DeclineReason::CtfeBody);
    }
    let Some(entry) = f.blocks.first() else {
        return Err(DeclineReason::UnsupportedShape);
    };
    let (node, lift) = try_decompose_from_block_lifted(f, entry.id)?;
    if crate::masking::collect_conditions(&node).is_empty() {
        return Err(DeclineReason::NoConditions);
    }
    Ok((node, lift))
}

/// Guard for the branch-taken-as-result lift: both arms of a
/// bool-switchInt must be `Call`- or `Assert`-terminated, neither
/// Call target may be a panic / noreturn stdlib function, and
/// neither arm's own continuation may run into a panic before it
/// rejoins the other arm.
///
/// Panic blacklist is matched by substring on the Call's `func_text`
/// (the verbatim MIR expression of the called function), see
/// [`is_panic_fn`]. Calls rustc lowers without a return edge
/// ([`MirTerminator::NoReturnCall`]) diverge whatever their name.
///
/// The continuation check closes the `assert!(cond, "msg {}", x)` /
/// `assert_eq!(a, b)` shape: the failure arm first calls the
/// formatting machinery (`Argument::new_display`, `Arguments::new`)
/// and only then the panic, so its first block looks like an
/// ordinary call. An assertion's guard is not a user decision.
///
/// The caller already requires both arms' recursive
/// `build_decision_from_block` to have returned None, so this
/// function only runs on genuinely opaque Call-terminated arms.
fn arms_are_non_panic_calls(f: &MirFunction, else_arm_id: BlockId, then_arm_id: BlockId) -> bool {
    let is_eligible = |id: BlockId| -> bool {
        let Some(block) = f.blocks.iter().find(|b| b.id == id) else {
            return false;
        };
        match &block.terminator {
            MirTerminator::Call { func_text, .. } => !is_panic_fn(func_text),
            // rustc's checked-operation terminator (overflow check
            // on `a + b`, bounds check, divide-by-zero check). Its
            // `success` continuation IS the normal control-flow
            // path, equivalent in effect to a non-panic Call for
            // the branch-taken-as-result reading.
            MirTerminator::Assert { .. } => true,
            _ => false,
        }
    };
    if !(is_eligible(else_arm_id) && is_eligible(then_arm_id)) {
        return false;
    }
    let else_path = arm_continuation(f, else_arm_id);
    let then_path = arm_continuation(f, then_arm_id);
    !private_path_diverges(&else_path, &then_path) && !private_path_diverges(&then_path, &else_path)
}

/// Most condition blocks [`lift_value_arms`] accepts in one decision.
const MAX_VALUE_ARM_CONDITIONS: usize = 16;

/// Most blocks [`value_arm_chain`] and [`skip_inert_gotos`] follow.
const MAX_VALUE_ARM_BLOCKS: usize = 32;

/// Value-arm lift: a Boolean decision whose two outcomes are plain
/// non-`bool` values (`if upper { 7 } else { 39 }`, `if -86400 < secs
/// && secs < 86400 { Some(..) } else { None }`).
///
/// Tried only at the decomposition root, and only after the ordinary
/// handlers (the call-arm lift included) declined it, so it never
/// replaces a decision they recover and its result is never nested
/// inside another decision. The rule, all of which must hold:
///
/// 1. `root` ends in a `switchInt` on a `bool` local with a nameable
///    condition (a debug name or an inline comparison). Following
///    the `switchInt` targets, skipping blocks that hold only
///    coverage / storage markers and a `goto`, every further block
///    that is itself such a `switchInt` is another condition of the
///    same short-circuit chain. Blocks of the chain do not write the
///    return place `_0`, and the chain has at most
///    [`MAX_VALUE_ARM_CONDITIONS`] conditions and no cycle.
/// 2. The chain ends in exactly two distinct arm blocks. A third
///    arm, as `if a { if b { .. } else { .. } } else { .. }` has,
///    means an arm holds a decision of its own: the function is
///    declined, not lifted.
/// 3. Each arm runs straight-line through `goto`s only (no call, no
///    `assert`, no `switchInt`, no return) into a join block that
///    both arms reach, and neither arm, nor the join, is a block of
///    the condition chain (a loop back edge).
/// 4. Both arms write one common local before the join (the return
///    place, or the `let` the `if` initialises), and that local is
///    not `bool`. A `bool` value is the ordinary handlers' job; a
///    `bool` write on both arms is typically a drop flag.
/// 5. Every condition `switchInt` whose two targets are both arms
///    sends its `otherwise` edge to the same arm. That arm is
///    outcome `true`, the other `false`.
///
/// The outcome labels follow the `switchInt` edges, like the
/// call-arm lift: for `if a && !b` the source `else` arm is labelled
/// `true`. MC/DC independence pairs do not depend on which outcome
/// is called `true`, only on the two being told apart, which they
/// are.
fn lift_value_arms(root: BlockId, f: &MirFunction) -> Option<Node> {
    let mut conditions: Vec<(BlockId, String, BlockId, BlockId)> = Vec::new();
    let mut arms: Vec<BlockId> = Vec::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if conditions.iter().any(|c| c.0 == id) {
            continue;
        }
        match value_arm_condition(id, f) {
            Some((name, else_id, then_id)) => {
                if conditions.len() >= MAX_VALUE_ARM_CONDITIONS {
                    return None;
                }
                let else_id = skip_inert_gotos(f, else_id);
                let then_id = skip_inert_gotos(f, then_id);
                conditions.push((id, name, else_id, then_id));
                pending.push(else_id);
                pending.push(then_id);
            }
            None if id == root => return None,
            None => {
                if !arms.contains(&id) {
                    arms.push(id);
                }
            }
        }
    }
    if arms.len() != 2 {
        return None;
    }
    let mut orientation: Option<(BlockId, BlockId)> = None;
    for &(_, _, else_id, then_id) in &conditions {
        if arms.contains(&else_id) && arms.contains(&then_id) {
            if else_id == then_id || orientation.is_some_and(|o| o != (then_id, else_id)) {
                return None;
            }
            orientation = Some((then_id, else_id));
        }
    }
    let (then_arm, else_arm) = orientation?;
    if !arms_join_with_value(f, then_arm, else_arm, &conditions) {
        return None;
    }
    value_arm_tree(root, &conditions, then_arm, &mut Vec::new())
}

/// A block of a value-arm condition chain: its `switchInt` tests a
/// nameable `bool` local (`[0: else, otherwise: then]`) and no
/// statement writes `_0`. Returns the condition name and the
/// `else` and `then` targets.
fn value_arm_condition(id: BlockId, f: &MirFunction) -> Option<(String, BlockId, BlockId)> {
    let block = f.blocks.iter().find(|b| b.id == id)?;
    let MirTerminator::SwitchInt {
        discr,
        targets,
        otherwise,
        ..
    } = &block.terminator
    else {
        return None;
    };
    if local_type(f, *discr) != Some("bool") || targets.len() != 1 || targets[0].0 != 0 {
        return None;
    }
    let writes_return_place = block
        .statements
        .iter()
        .any(|s| statement_dst(s).or_else(|| other_statement_dst(s)) == Some(RETURN_PLACE));
    if writes_return_place {
        return None;
    }
    let name = match f.debug_names.get(discr) {
        Some(name) => name.clone(),
        None => synthesize_compare_name(block, *discr, f)?,
    };
    Some((name, targets[0].1, *otherwise))
}

/// Is `stmt` a marker with no value: a coverage counter,
/// `StorageLive`, `StorageDead`, or any other unparsed statement
/// that is not an assignment?
fn is_inert_statement(stmt: &MirStatement) -> bool {
    matches!(stmt, MirStatement::Other { text, .. } if !text.contains(" = "))
}

/// Follow `goto`s out of `start` through blocks that hold only
/// inert statements ([`is_inert_statement`]), and return the first
/// block that does something else. rustc emits such a block per
/// short-circuit exit (`bb2: { Coverage::VirtualCounter(bcb6); goto
/// -> bb5; }`), so the `else` arm of `a && b` is reached through two
/// of them.
fn skip_inert_gotos(f: &MirFunction, start: BlockId) -> BlockId {
    let mut current = start;
    let mut seen = Vec::new();
    while seen.len() < MAX_VALUE_ARM_BLOCKS && !seen.contains(&current) {
        seen.push(current);
        let Some(block) = f.blocks.iter().find(|b| b.id == current) else {
            break;
        };
        match &block.terminator {
            MirTerminator::Goto { target, .. }
                if block.statements.iter().all(is_inert_statement) =>
            {
                current = *target;
            }
            _ => break,
        }
    }
    current
}

/// The blocks control runs through from `start` along `goto`s: each
/// `goto`-terminated block and then the first block that is not one
/// (or the first block seen twice).
fn value_arm_chain(f: &MirFunction, start: BlockId) -> Vec<BlockId> {
    let mut chain = Vec::new();
    let mut current = start;
    while chain.len() < MAX_VALUE_ARM_BLOCKS && !chain.contains(&current) {
        chain.push(current);
        match goto_target(f, current) {
            Some(target) => current = target,
            None => break,
        }
    }
    chain
}

/// Rules 3 and 4 of [`lift_value_arms`]: the two arms run through
/// `goto`s into a common join block outside the condition chain and
/// both write a common non-`bool` local on the way.
fn arms_join_with_value(
    f: &MirFunction,
    then_arm: BlockId,
    else_arm: BlockId,
    conditions: &[(BlockId, String, BlockId, BlockId)],
) -> bool {
    let then_chain = value_arm_chain(f, then_arm);
    let else_chain = value_arm_chain(f, else_arm);
    let Some(else_join) = else_chain.iter().position(|b| then_chain.contains(b)) else {
        return false;
    };
    let join = else_chain[else_join];
    let then_join = then_chain
        .iter()
        .position(|b| *b == join)
        .expect("join is on the then chain");
    if then_join == 0 || else_join == 0 {
        // One arm is the join itself: an `if` without `else`.
        return false;
    }
    let then_private = &then_chain[..then_join];
    let else_private = &else_chain[..else_join];
    let in_chain = |b: &BlockId| conditions.iter().any(|c| c.0 == *b);
    if then_private.iter().chain(else_private).any(in_chain) || in_chain(&join) {
        return false;
    }
    let writes = |blocks: &[BlockId]| -> Vec<LocalId> {
        blocks
            .iter()
            .filter_map(|id| f.blocks.iter().find(|b| b.id == *id))
            .flat_map(|b| b.statements.iter())
            .filter_map(|s| statement_dst(s).or_else(|| other_statement_dst(s)))
            .collect()
    };
    let else_writes = writes(else_private);
    writes(then_private).iter().any(|local| {
        else_writes.contains(local) && local_type(f, *local).is_some_and(|ty| !is_bool_type(ty))
    })
}

/// Build the lifted tree from condition block `id`: an [`Node::Ite`]
/// per condition, [`Node::Const`] `true` at `then_arm` and `false`
/// at the other arm. `None` on a cycle through `path`.
fn value_arm_tree(
    id: BlockId,
    conditions: &[(BlockId, String, BlockId, BlockId)],
    then_arm: BlockId,
    path: &mut Vec<BlockId>,
) -> Option<Node> {
    let Some((_, name, else_id, then_id)) = conditions.iter().find(|c| c.0 == id) else {
        return Some(Node::Const {
            value: id == then_arm,
        });
    };
    if path.contains(&id) {
        return None;
    }
    path.push(id);
    let then_branch = value_arm_tree(*then_id, conditions, then_arm, path)?;
    let else_branch = value_arm_tree(*else_id, conditions, then_arm, path)?;
    path.pop();
    Some(Node::Ite {
        cond: name.clone(),
        then_branch: Box::new(then_branch),
        else_branch: Box::new(else_branch),
    })
}

/// Is `func_text` (a call's function expression) one of rustc's
/// panic / assertion-failure / unwrap-failure entry points?
///
/// Catches `core::panicking::panic`, `core::panicking::panic_fmt`,
/// `std::rt::panic_fmt`, `core::option::expect_failed`,
/// `core::result::unwrap_failed`, `::unwrap_failed`,
/// `::expect_failed`, `::assert_failed`, and the `unreachable`
/// family.
fn is_panic_fn(func_text: &str) -> bool {
    let t = func_text;
    t.contains("core::panicking::")
        || t.contains("::unwrap_failed")
        || t.contains("::expect_failed")
        || t.contains("::assert_failed")
        || t.contains("::assert_eq_failed")
        || t.contains("::assert_ne_failed")
        || t.contains("::panic_fmt")
        || t.contains("::begin_panic")
        || t.contains("::unreachable")
}

/// Maximum number of blocks [`arm_continuation`] follows.
const MAX_ARM_CONTINUATION: usize = 64;

/// One block on an arm's straight-line continuation.
struct ContinuationStep {
    block: BlockId,
    /// Control cannot get past this block: it calls a panic function,
    /// is a no-return call, or is `unreachable`.
    diverges: bool,
}

/// Follow the straight-line continuation of `start`: the `return`
/// edge of each non-panic `Call`, the `success` edge of each
/// `Assert`, and each `Goto`. Stops at a block that branches
/// (`switchInt`), returns, diverges, or whose terminator the parser
/// keeps as `Other`; at a block already visited; or after
/// [`MAX_ARM_CONTINUATION`] blocks.
fn arm_continuation(f: &MirFunction, start: BlockId) -> Vec<ContinuationStep> {
    let mut steps: Vec<ContinuationStep> = Vec::new();
    let mut current = Some(start);
    while let Some(id) = current.take() {
        if steps.len() >= MAX_ARM_CONTINUATION || steps.iter().any(|s| s.block == id) {
            break;
        }
        let Some(block) = f.blocks.iter().find(|b| b.id == id) else {
            break;
        };
        let (diverges, next) = match &block.terminator {
            MirTerminator::Call {
                func_text, target, ..
            } => {
                let panics = is_panic_fn(func_text);
                (panics, (!panics).then_some(*target))
            }
            MirTerminator::Assert { success, .. } => (false, Some(*success)),
            MirTerminator::Goto { target, .. } => (false, Some(*target)),
            MirTerminator::NoReturnCall { .. } | MirTerminator::Unreachable { .. } => (true, None),
            MirTerminator::Return { .. }
            | MirTerminator::SwitchInt { .. }
            | MirTerminator::Other { .. } => (false, None),
        };
        steps.push(ContinuationStep {
            block: id,
            diverges,
        });
        current = next;
    }
    steps
}

/// Does `path` diverge before it reaches any block of `other` (the
/// point where the two arms rejoin)?
fn private_path_diverges(path: &[ContinuationStep], other: &[ContinuationStep]) -> bool {
    path.iter()
        .take_while(|s| !other.iter().any(|o| o.block == s.block))
        .any(|s| s.diverges)
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

    // Intermediate-propagation: when this block writes a constant to
    // a local and then `goto`s a successor that `switchInt`s on that
    // local, the constant determines which arm we'd take. Fold the
    // assignment into the chosen arm's decomposition. Closes the
    // `match n { ... } && b` shape, where the match writes an
    // intermediate `_3 = const true/false` per arm and the `&&`
    // downstream branches on it.
    if let Some(node) = try_fold_into_successor_switchint(block, f) {
        return Some(node);
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
                let else_arm_id = targets[0].1;
                let then_arm_id = *otherwise;
                let else_branch = build_decision_from_block(else_arm_id, f);
                let then_branch = build_decision_from_block(then_arm_id, f);
                match (else_branch, then_branch) {
                    (Some(else_branch), Some(then_branch)) => {
                        return Some(handle_boolean(&BoolExpr {
                            cond,
                            then_branch,
                            else_branch,
                        }));
                    }
                    (None, None) if arms_are_non_panic_calls(f, else_arm_id, then_arm_id) => {
                        // Branch-taken-as-result lift: when both arms
                        // terminate in non-panic Calls that Floyd
                        // cannot decompose further (opaque function
                        // calls returning non-boolean types), treat
                        // "which arm was taken" as the decision's
                        // observable outcome. The condition is still a
                        // real Boolean; its MC/DC pair gets scored
                        // against the branch-counter observations.
                        //
                        // This is a legitimate reading of CAST-10
                        // MC/DC — the pair requires only that the
                        // decision's OBSERVABLE outcome differ when
                        // the condition flips. Branch-taken is
                        // observable via coverage; the arm's actual
                        // return value is not needed.
                        //
                        // Guards: inside the bool-switchInt handler
                        // only (so match-with-Call-arms stays
                        // enterprise), only when the normal
                        // recursive path returned None (we never
                        // steal cases that already decompose), and
                        // panic/noreturn Calls are rejected via
                        // `arms_are_non_panic_calls` so assert /
                        // unwrap / expect patterns do not fire.
                        CALL_ARMS_LIFT_SEEN.with(|seen| seen.set(true));
                        return Some(handle_boolean(&BoolExpr {
                            cond,
                            then_branch: Node::Const { value: true },
                            else_branch: Node::Const { value: false },
                        }));
                    }
                    _ => return None,
                }
            }
            // Literal `match` on a non-bool integer.
            try_match_from_block(block, f, *discr, targets, *otherwise)
        }
        MirTerminator::Goto { .. }
        | MirTerminator::Return { .. }
        | MirTerminator::Unreachable { .. }
        | MirTerminator::Call { .. }
        | MirTerminator::NoReturnCall { .. }
        | MirTerminator::Assert { .. }
        | MirTerminator::Other { .. } => None,
    }
}

/// Type of a local as declared in the function header or `let`
/// declarations. rustc declares every local it uses, the return
/// place `_0` and temporaries included, so `None` means the local
/// is not part of `f`.
pub(crate) fn local_type(f: &MirFunction, local: LocalId) -> Option<&str> {
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

/// Is the MIR type text `ty` exactly `bool`?
fn is_bool_type(ty: &str) -> bool {
    ty.trim() == "bool"
}

/// The pointee of a reference or raw-pointer type as MIR prints it
/// (`&T`, `&mut T`, `&'a T`, `&'a mut T`, `*const T`, `*mut T`), or
/// `None` for any other type. Only one level is removed, so the
/// pointee of `&&bool` is `&bool`.
fn pointee_type(ty: &str) -> Option<&str> {
    let ty = ty.trim();
    let pointee = if let Some(rest) = ty.strip_prefix('&') {
        let rest = rest.trim_start();
        let rest = match rest.strip_prefix('\'') {
            Some(lifetime) => lifetime.split_once(' ')?.1,
            None => rest,
        };
        rest.strip_prefix("mut ").unwrap_or(rest)
    } else if let Some(rest) = ty.strip_prefix("*const ") {
        rest
    } else {
        ty.strip_prefix("*mut ")?
    };
    Some(pointee.trim())
}

/// Does reading `local` (its pointee when `deref`) produce a `bool`?
/// False when the local's type is not declared in `f`: a leaf of
/// unknown type is never assumed to be Boolean.
fn reads_bool(f: &MirFunction, local: LocalId, deref: bool) -> bool {
    match local_type(f, local) {
        Some(ty) if deref => pointee_type(ty).is_some_and(is_bool_type),
        Some(ty) => is_bool_type(ty),
        None => false,
    }
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
///
/// Two naming styles, selected by structure:
///
/// - **Literal integer match**: when `switchInt` is on the discriminant
///   local directly (no `AssignDiscriminant` setting it), the condition
///   names use raw literal values — `n == 0`, `n == 1`.
/// - **Enum match without binding**: when the discriminant local was
///   set by `_D = discriminant(_S);` in this block, the names use the
///   scrutinee local's debug name and (if the scrutinee's type is a
///   simple identifier) the type — `state == SystemState::variant_0`.
///   The `variant_<N>` form names the rustc discriminant index, not
///   the source variant identifier, because MIR text doesn't carry
///   variant identifiers when no downcast is emitted. Engineers
///   wanting source-level variant names should add a binding
///   (`if let Variant(_) = ...`) which routes through the `if let`
///   handler instead.
fn try_match_from_block(
    block: &MirBlock,
    f: &MirFunction,
    discr: LocalId,
    targets: &[(u128, BlockId)],
    otherwise: BlockId,
) -> Option<Node> {
    // Naming style: enum or literal-integer. An `AssignDiscriminant`
    // setting `discr` in this block means we're branching on an enum
    // tag; otherwise this is a plain integer `match`.
    let enum_scrutinee = block.statements.iter().find_map(|s| match s {
        MirStatement::AssignDiscriminant { dst, src, .. } if *dst == discr => Some(*src),
        _ => None,
    });
    let style = match enum_scrutinee {
        Some(scrutinee) => {
            let scrutinee_name = f
                .debug_names
                .get(&scrutinee)
                .cloned()
                .unwrap_or_else(|| format!("_{scrutinee}"));
            let ty = local_type(f, scrutinee).map(str::to_string);
            MatchNamingStyle::Enum {
                scrutinee_name,
                scrutinee_type: ty,
            }
        }
        None => {
            let scrutinee_name = f
                .debug_names
                .get(&discr)
                .cloned()
                .unwrap_or_else(|| format!("_{discr}"));
            MatchNamingStyle::IntegerLiteral { scrutinee_name }
        }
    };

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
            cond: style.cond_for_value(last_v),
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
            cond: style.cond_for_value(v),
            then_branch,
            else_branch: tail,
        });
    }
    Some(tail)
}

/// How `try_match_from_block` names its synthetic conditions.
pub(crate) enum MatchNamingStyle {
    IntegerLiteral {
        scrutinee_name: String,
    },
    Enum {
        scrutinee_name: String,
        scrutinee_type: Option<String>,
    },
}

impl MatchNamingStyle {
    pub(crate) fn cond_for_value(&self, value: u128) -> String {
        match self {
            MatchNamingStyle::IntegerLiteral { scrutinee_name } => {
                format!("{scrutinee_name} == {value}")
            }
            MatchNamingStyle::Enum {
                scrutinee_name,
                scrutinee_type,
            } => match scrutinee_type {
                Some(t) if is_simple_type_identifier(t) => {
                    format!("{scrutinee_name} == {t}::variant_{value}")
                }
                _ => format!("{scrutinee_name} == variant_{value}"),
            },
        }
    }
}

/// True if `s` is a single-segment type identifier — only ASCII
/// letters, digits, and underscores. Generic / qualified / tuple /
/// reference types fail this check and the namer falls back to a
/// bare `variant_<N>` to avoid producing ugly conditions like
/// `r == Result<bool, ()>::variant_0`.
/// Return the synthetic condition names a `switchInt` on an integer
/// or enum discriminant would surface through the match / if-let
/// decomposer, in the order those names appear as `cond` fields of
/// the resulting ITE tree (outermost arm first).
///
/// Used by [`crate::correlate::collect_condition_spans`] to tie
/// synthesised names like `x == 0` and `x == Option::variant_1` to
/// the switchInt's source span, so runtime branch counts for the
/// `match` / `if let` land on the right condition. Mirrors
/// [`try_match_from_block`]'s naming exactly — if the two drift,
/// correlation silently misses matches.
///
/// Returns `None` when `block` is not a match/if-let-shaped
/// switchInt (bool short-circuits, closures that read captured
/// bools, and `?`-style terminators are not this handler's
/// responsibility).
pub(crate) fn match_switchint_condition_names(
    block: &MirBlock,
    f: &MirFunction,
    discr: LocalId,
    targets: &[(u128, BlockId)],
    otherwise: BlockId,
) -> Option<Vec<String>> {
    // Bool discriminants go through the handle_boolean path; their
    // condition name is the discr's debug name or a synthesised
    // compare name, not a `scrutinee == value` string.
    if local_type(f, discr) == Some("bool") {
        return None;
    }
    let enum_scrutinee = block.statements.iter().find_map(|s| match s {
        MirStatement::AssignDiscriminant { dst, src, .. } if *dst == discr => Some(*src),
        _ => None,
    });
    let style = match enum_scrutinee {
        Some(scrutinee) => {
            let scrutinee_name = f
                .debug_names
                .get(&scrutinee)
                .cloned()
                .unwrap_or_else(|| format!("_{scrutinee}"));
            let scrutinee_type = local_type(f, scrutinee).map(str::to_string);
            MatchNamingStyle::Enum {
                scrutinee_name,
                scrutinee_type,
            }
        }
        None => {
            let scrutinee_name = f
                .debug_names
                .get(&discr)
                .cloned()
                .unwrap_or_else(|| format!("_{discr}"));
            MatchNamingStyle::IntegerLiteral { scrutinee_name }
        }
    };

    // Same arm filter the decomposer uses: unreachable targets drop
    // out before naming so correlator and decomposer stay in sync.
    let reachable_arms: Vec<(u128, BlockId)> = targets
        .iter()
        .copied()
        .filter(|(_, t)| !is_unreachable_block(*t, f))
        .collect();
    if reachable_arms.is_empty() {
        return None;
    }
    let other_reachable = !is_unreachable_block(otherwise, f);

    // The decomposer emits one `cond_for_value` per arm EXCEPT the
    // last when `otherwise` is unreachable (that arm becomes the
    // unconditional fallthrough — no condition in the tree).
    let n_named = if other_reachable {
        reachable_arms.len()
    } else {
        reachable_arms.len().saturating_sub(1)
    };
    Some(
        reachable_arms
            .iter()
            .take(n_named)
            .map(|(v, _)| style.cond_for_value(*v))
            .collect(),
    )
}

pub(crate) fn is_simple_type_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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

/// Does this function contain at least one `switchInt` terminator?
///
/// Used by the honest-scope report in `cargo-floyd` to tell "the
/// function has no decision at all" (pure straight-line code, pure
/// Call delegation, etc.) apart from "the function HAS a decision
/// but the engine's current patterns did not match it". The first
/// case is uninteresting; the second is what the report surfaces.
pub fn has_switchint(f: &MirFunction) -> bool {
    f.blocks
        .iter()
        .any(|b| matches!(b.terminator, MirTerminator::SwitchInt { .. }))
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

/// Does this block's terminator look like a rustc
/// `-Zcoverage-options=branch,condition` branch counter rather than a
/// logical boolean switchInt?
///
/// The marker shape is `switchInt(_X) -> [0: A, otherwise: B]` where
/// `A` and `B` both have terminator `goto <common>`, and `_X` is
/// assigned by a statement in the current block. When this holds,
/// the switchInt is instrumentation emitted by rustc to count the
/// two outcomes of `_X` for coverage purposes — it does not
/// discriminate real control flow, so decomposition should treat
/// the assignment to `_X` as the block's terminal value.
fn is_coverage_branch_counter(
    block: &MirBlock,
    discr: LocalId,
    targets: &[(u128, BlockId)],
    otherwise: BlockId,
    f: &MirFunction,
) -> bool {
    // Boolean shape: a single explicit `0` arm plus `otherwise`.
    if targets.len() != 1 || targets[0].0 != 0 {
        return false;
    }
    // Both arms immediately `goto` the same successor.
    let then_succ = goto_target(f, otherwise);
    let else_succ = goto_target(f, targets[0].1);
    if then_succ.is_none() || then_succ != else_succ {
        return false;
    }
    // Both arms must be pure instrumentation — no value-producing
    // assignments. In rustc's branch-counter emission the arms
    // contain only a `Coverage::VirtualCounter` statement plus the
    // `goto`. Without this guard, legitimate shapes like
    // `_3 = Gt(_1, const 50); switchInt(_3) -> [0: {_0 = const
    // false; goto X}, otherwise: {_0 = copy _2; goto X}]` — a real
    // inline-comparison `&&` — would be absorbed, hiding the
    // decision from the decomposer.
    for &arm_id in &[otherwise, targets[0].1] {
        if let Some(arm) = f.blocks.iter().find(|b| b.id == arm_id) {
            if arm.statements.iter().any(is_value_producing_assign) {
                return false;
            }
        }
    }
    // The discriminant is assigned by a statement in THIS block.
    // Without this check we would absorb switchInts on inherited
    // values (where the counter interpretation does not apply).
    block
        .statements
        .iter()
        .any(|stmt| statement_writes(stmt, discr))
}

/// Does `stmt` write to any local via a value-producing assignment?
/// Used to distinguish coverage-instrumentation arms (which only
/// contain `Coverage::VirtualCounter` + `goto`) from legitimate
/// branch arms (which assign to `_0` or another local).
fn is_value_producing_assign(stmt: &MirStatement) -> bool {
    matches!(
        stmt,
        MirStatement::AssignConstBool { .. }
            | MirStatement::AssignCopy { .. }
            | MirStatement::AssignBinaryCompare { .. }
            | MirStatement::AssignCaptureRead { .. }
            | MirStatement::AssignDiscriminant { .. }
            | MirStatement::AssignDowncast { .. }
            | MirStatement::AssignUnaryNot { .. }
    )
}

/// Return the `goto` target of `block_id` if its terminator is a
/// plain `Goto`, else `None`.
fn goto_target(f: &MirFunction, block_id: BlockId) -> Option<BlockId> {
    f.blocks.iter().find(|b| b.id == block_id).and_then(|b| {
        if let MirTerminator::Goto { target, .. } = &b.terminator {
            Some(*target)
        } else {
            None
        }
    })
}

/// Destination of a value-producing assignment, or `None` for any
/// other statement. Covers the same kinds as [`statement_writes`].
fn statement_dst(stmt: &MirStatement) -> Option<LocalId> {
    match stmt {
        MirStatement::AssignConstBool { dst, .. }
        | MirStatement::AssignCopy { dst, .. }
        | MirStatement::AssignBinaryCompare { dst, .. }
        | MirStatement::AssignCaptureRead { dst, .. }
        | MirStatement::AssignDiscriminant { dst, .. }
        | MirStatement::AssignDowncast { dst, .. }
        | MirStatement::AssignUnaryNot { dst, .. } => Some(*dst),
        _ => None,
    }
}

/// Does `stmt` read local `local`? Parsed statements are checked by
/// their operand fields; an unparsed [`MirStatement::Other`] is
/// checked conservatively for any `_<local>` token in its text.
fn statement_reads(stmt: &MirStatement, local: LocalId) -> bool {
    let operand_is =
        |op: &Operand| matches!(op, Operand::Copy(l) | Operand::Move(l) if *l == local);
    match stmt {
        MirStatement::AssignConstBool { .. } => false,
        MirStatement::AssignCopy { src, .. }
        | MirStatement::AssignDiscriminant { src, .. }
        | MirStatement::AssignDowncast { src, .. }
        | MirStatement::AssignUnaryNot { src, .. } => *src == local,
        MirStatement::AssignBinaryCompare { lhs, rhs, .. } => operand_is(lhs) || operand_is(rhs),
        MirStatement::AssignCaptureRead { env, .. } => *env == local,
        MirStatement::Other { text, .. } => text_mentions_local(text, local),
    }
}

/// Does `text` contain the local token `_<local>` (not as part of a
/// longer identifier or number, so `_1` does not match `_12`)?
fn text_mentions_local(text: &str, local: LocalId) -> bool {
    let needle = format!("_{local}");
    let bytes = text.as_bytes();
    text.match_indices(&needle).any(|(i, _)| {
        let before_ok = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        let after = i + needle.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_digit();
        before_ok && after_ok
    })
}

/// Does `stmt` write to local `dst_local`? Covers every statement
/// kind the decomposer treats as a value-producing assignment.
fn statement_writes(stmt: &MirStatement, dst_local: LocalId) -> bool {
    match stmt {
        MirStatement::AssignConstBool { dst, .. }
        | MirStatement::AssignCopy { dst, .. }
        | MirStatement::AssignBinaryCompare { dst, .. }
        | MirStatement::AssignCaptureRead { dst, .. }
        | MirStatement::AssignDiscriminant { dst, .. }
        | MirStatement::AssignDowncast { dst, .. }
        | MirStatement::AssignUnaryNot { dst, .. } => *dst == dst_local,
        _ => false,
    }
}

/// MIR's return place: the local `_0` that holds a function's result.
const RETURN_PLACE: LocalId = 0;

/// Extract a leaf [`Node`] from a terminal block.
///
/// Looks for a value-producing statement (`AssignConstBool`,
/// `AssignCopy`, `AssignUnaryNot`, `AssignBinaryCompare`,
/// `AssignCaptureRead`) that is the block's *outgoing* value, in
/// source order. The first match wins (Phase 0 blocks have at most
/// one such statement).
///
/// A write is the block's outgoing value only if it leaves the block:
///
/// - nothing later in the same block reads it. A write consumed in
///   the block is operand set-up, e.g. the `assert_eq!` expansion
///   `_8 = copy (*_19); _7 = Eq(copy _2, move _8); switchInt(move _7)`
///   or the loop-condition alias `_4 = copy _2; _3 = Gt(move _4, ..)`.
///   Taking it as the value would report its source's name
///   (`left_val`, `ms`) as a one-condition decision that is not in
///   the source.
/// - when the block ends in a real (non-coverage-counter) `switchInt`,
///   it writes the return place `_0`. Such a block branches on to
///   further code, so a write to any other local is not a result.
///
/// Writes to an intermediate local that flow out through a `goto`
/// (`_6 = copy _2; goto -> bb7` with `bb7: _0 = Some(move _6)`)
/// remain values.
///
/// A leaf read from a debug-named local (a copy, a dereference copy,
/// a `Not`, a by-value closure capture) is a [`Node::Condition`]
/// only when the value read is declared `bool`. Otherwise the block's
/// value is not Boolean (`_0 = copy _1` with `_1: I256`, `_0 =
/// Not(copy _2)` on an integer, a call result of type `u32`): the
/// block yields `None`, so the arm and the decision decline, and the
/// decline is recorded as [`DeclineReason::NonBoolLeaf`]. Comparisons
/// produce `bool` by construction and need no check.
fn extract_terminal_value(block: &MirBlock, f: &MirFunction) -> Option<Node> {
    // An assignment is a terminal value only if its destination
    // doesn't feed a `switchInt` — either this block's own terminator
    // or the immediately-following block's terminator reached via a
    // `goto`. The downstream case closes the `match n { ... } && b`
    // shape, where a per-arm block sets an intermediate `_3 = const
    // true/false` and `goto`s a successor that `switchInt(_3)` for
    // the `&&`. Without the downstream check, the per-arm assignment
    // would short-circuit as a terminal value, hiding the `&&` from
    // the decomposer.
    let in_block_switchint = match &block.terminator {
        MirTerminator::SwitchInt {
            discr,
            targets,
            otherwise,
            ..
        } => {
            // Coverage-instrumentation branch counter:
            // `switchInt(_X) -> [0: A, otherwise: B]` where both A
            // and B `goto` the same successor. Rustc inserts this
            // when `-Cinstrument-coverage` runs with
            // `-Zcoverage-options=branch,condition` to count the
            // two outcomes of a boolean sub-expression — it has no
            // effect on logical control flow. Classify it as "not
            // a real switchInt on `_X`" so the preceding
            // assignment to `_X` in this block (`_0 = copy _4;`
            // on the terminal leaf of an `&&`/`||` chain) is
            // accepted as the terminal value.
            if is_coverage_branch_counter(block, *discr, targets, *otherwise, f) {
                None
            } else {
                Some(*discr)
            }
        }
        _ => None,
    };
    let downstream_switchint = match &block.terminator {
        MirTerminator::Goto { target, .. } => {
            f.blocks
                .iter()
                .find(|b| b.id == *target)
                .and_then(|succ| match &succ.terminator {
                    // Resolve through any `_X = copy _Y` aliases in the
                    // succ block — the effective discriminant is the
                    // local our caller wrote to, not the fresh temporary.
                    MirTerminator::SwitchInt { discr, .. } => {
                        Some(resolve_switchint_discr_alias(*discr, succ))
                    }
                    _ => None,
                })
        }
        _ => None,
    };
    let feeds_switchint =
        |dst: LocalId| in_block_switchint == Some(dst) || downstream_switchint == Some(dst);

    // Terminal-block gate: an assignment is only a "terminal
    // value" when control flow does not continue past this block
    // via a function call. `Call` terminators write the call's
    // return into their destination AFTER running — any
    // `AssignCopy` earlier in the block is argument prep, not the
    // function's own return value. Without this gate, a block like
    // `bb0: { _3 = copy (*_1); _2 = f(move _3) -> bb1; }` is
    // misread as returning `_1`'s debug-name as a Condition leaf
    // (semver's `ptr_as_str` regression: `debug repr => _1` was
    // false-positively surfaced as a 1-condition decision).
    //
    // `Assert` is gated the same way: the statements before it set
    // up the checked operation (`_5 = AddWithOverflow(..)`) and
    // control continues to its `success` block, which produces the
    // value.
    //
    // `Goto`, `Return`, `Unreachable`, and `SwitchInt` all keep
    // the current-block assignments as either the terminal value
    // (`Goto`/`Return`) or as discriminant setup (`SwitchInt`,
    // handled via `feeds_switchint`), so they stay in scope.
    if matches!(
        &block.terminator,
        MirTerminator::Call { .. }
            | MirTerminator::NoReturnCall { .. }
            | MirTerminator::Assert { .. }
            | MirTerminator::Other { .. }
    ) {
        return None;
    }

    let is_outgoing_write = |idx: usize, stmt: &MirStatement| -> bool {
        let Some(dst) = statement_dst(stmt) else {
            return false;
        };
        if in_block_switchint.is_some() && dst != RETURN_PLACE {
            return false;
        }
        // A `goto` block's write to a local other than `_0` is the
        // block's value only when it reaches `_0` downstream (the
        // join block reads it into the return place). Otherwise it is
        // bookkeeping: a drop flag (`_25 = const false; goto -> bb1`
        // at the top of a loop), or a local the rest of the body
        // reads for something else.
        if let MirTerminator::Goto { target, .. } = &block.terminator {
            if dst != RETURN_PLACE && !feeds_switchint(dst) && !flows_to_return(f, *target, dst) {
                return false;
            }
        }
        !block.statements[idx + 1..]
            .iter()
            .any(|later| statement_reads(later, dst))
    };

    let outgoing_writes = block
        .statements
        .iter()
        .enumerate()
        .filter(|(i, s)| is_outgoing_write(*i, s))
        .map(|(_, s)| s);
    for stmt in outgoing_writes {
        match stmt {
            MirStatement::AssignConstBool { dst, value, .. } if !feeds_switchint(*dst) => {
                return Some(Node::Const { value: *value });
            }
            MirStatement::AssignCopy {
                dst, src, deref, ..
            } if !feeds_switchint(*dst) => {
                if let Some(name) = f.debug_names.get(src) {
                    if !reads_bool(f, *src, *deref) {
                        note_non_bool_leaf();
                        return None;
                    }
                    return Some(Node::Condition { name: name.clone() });
                }
            }
            MirStatement::AssignUnaryNot { dst, src, .. } if !feeds_switchint(*dst) => {
                // `_dst = Not(copy _src)` as a terminal value: the
                // decision's result is `!<src_name>`. Emit a
                // `Node::Condition` naming the complement
                // (`!<src_name>`), so runtime branch counts coming
                // from the Not's own branch counter — which track
                // the NEGATED value — map cleanly onto the
                // condition name shown in the report. (If instead
                // we named the condition `<src_name>` with an ITE
                // inversion, per-test observations would display
                // the Not's output under the raw-input name,
                // confusing the reader.)
                // On an integer, `Not` is bitwise complement, not a
                // Boolean negation.
                if let Some(name) = f.debug_names.get(src) {
                    if !reads_bool(f, *src, false) {
                        note_non_bool_leaf();
                        return None;
                    }
                    return Some(Node::Condition {
                        name: format!("!{name}"),
                    });
                }
            }
            MirStatement::AssignBinaryCompare {
                dst, op, lhs, rhs, ..
            } if !feeds_switchint(*dst) => {
                // Issue #18: if the compare has an unnamed-local
                // operand, compare_condition_name returns None and we
                // decline the function. (Returning None from an
                // extract_terminal_value branch declines the whole
                // extraction path.)
                return Some(Node::Condition {
                    name: compare_condition_name(*op, lhs, rhs, f)?,
                });
            }
            MirStatement::AssignCaptureRead { dst, holds_ref, .. }
                if !holds_ref && !feeds_switchint(*dst) =>
            {
                // By-value capture read: the propagation pass set
                // debug_names[dst] to the captured variable's
                // source-level name, so this dst is the terminal
                // Condition leaf.
                if let Some(name) = f.debug_names.get(dst) {
                    if !reads_bool(f, *dst, false) {
                        note_non_bool_leaf();
                        return None;
                    }
                    return Some(Node::Condition { name: name.clone() });
                }
            }
            // Everything else (temporaries that feed a switchInt,
            // downcasts, discriminants, capture-by-ref aliases,
            // unrecognised shapes) is informational for downstream
            // decoders.
            _ => {}
        }
    }
    None
}

/// Most blocks [`flows_to_return`] follows before giving up.
const FLOW_STEP_CAP: usize = 64;

/// Does the value of `local`, as written just before a `goto` to
/// `start`, reach the return place `_0`?
///
/// Follows the straight-line path from `start` (through `goto`, the
/// `success` edge of an `assert` and the return edge of a call whose
/// destination is not `_0`) and tracks `local` and any local it is
/// copied or moved into. The answer is `true` at the first
/// statement that writes `_0` and reads a tracked local
/// (`_0 = copy _6`, `_0 = Option::<bool>::Some(move _6)`), and
/// `false` when `_0` is written from something else, every tracked
/// local is overwritten, or the path branches, returns or leaves
/// the shapes above. A drop flag never reaches `_0`: it is read
/// only by the drop-elaboration `switchInt`s that guard a `drop`.
fn flows_to_return(f: &MirFunction, start: BlockId, local: LocalId) -> bool {
    let mut tracked: Vec<LocalId> = vec![local];
    let mut seen: Vec<BlockId> = Vec::new();
    let mut cur = start;
    for _ in 0..FLOW_STEP_CAP {
        if seen.contains(&cur) {
            return false;
        }
        seen.push(cur);
        let Some(block) = f.blocks.iter().find(|b| b.id == cur) else {
            return false;
        };
        for stmt in &block.statements {
            let reads_tracked = tracked.iter().any(|l| statement_reads(stmt, *l));
            let Some(dst) = statement_dst(stmt).or_else(|| other_statement_dst(stmt)) else {
                continue;
            };
            if dst == RETURN_PLACE {
                return reads_tracked;
            }
            if reads_tracked {
                if !tracked.contains(&dst) {
                    tracked.push(dst);
                }
            } else {
                tracked.retain(|l| *l != dst);
                if tracked.is_empty() {
                    return false;
                }
            }
        }
        cur = match &block.terminator {
            MirTerminator::Goto { target, .. } => *target,
            MirTerminator::Assert { success, .. } => *success,
            MirTerminator::Call { dst, target, .. } if *dst != RETURN_PLACE => {
                tracked.retain(|l| l != dst);
                if tracked.is_empty() {
                    return false;
                }
                *target
            }
            _ => return false,
        };
    }
    false
}

/// Destination of an unparsed [`MirStatement::Other`] assignment
/// (`_0 = Option::<bool>::Some(move _6)`, `_4 = const 0_u64`), read
/// from its leading `_<N> = `. Projections (`(_1.0: u32) = ..`) and
/// non-assignments yield `None`.
fn other_statement_dst(stmt: &MirStatement) -> Option<LocalId> {
    let MirStatement::Other { text, .. } = stmt else {
        return None;
    };
    let (lhs, _) = text.split_once(" = ")?;
    parse_plain_local(lhs.trim())
}

/// Fold a `<intermediate> = ...; goto <succ>` block into the arm of
/// the successor's `switchInt(<intermediate>)` that the constant
/// assignment selects.
///
/// Closes the `match n { 0 => true, _ => false } && b` shape — and
/// any analogous "compute an intermediate bool, then branch on it"
/// pattern — by letting decomposition continue past the intermediate
/// rather than terminating at it. Returns `None` for any block whose
/// terminator isn't `goto`, whose successor doesn't `switchInt` on
/// the local the block writes, or whose write isn't a constant.
///
/// Non-constant writes (e.g. `_3 = copy _N`) would need value
/// substitution beyond what this MVP attempts; they fall through to
/// the next decoder.
fn try_fold_into_successor_switchint(block: &MirBlock, f: &MirFunction) -> Option<Node> {
    let MirTerminator::Goto { target, .. } = &block.terminator else {
        return None;
    };
    let succ = f.blocks.iter().find(|b| b.id == *target)?;
    let (succ_discr, succ_targets, succ_otherwise) = match &succ.terminator {
        MirTerminator::SwitchInt {
            discr,
            targets,
            otherwise,
            ..
        } => (*discr, targets, *otherwise),
        _ => return None,
    };
    // Resolve through any `_X = copy _Y` aliases in the succ block.
    // For `match n { ... } && b` the succ block typically looks like
    //   `_4 = copy _3; switchInt(move _4) -> [...]`
    // and the effective discriminant is `_3` (the intermediate the
    // caller's `_3 = const ...` writes to), not the freshly-copied
    // `_4`.
    let effective_discr = resolve_switchint_discr_alias(succ_discr, succ);
    for stmt in &block.statements {
        if let MirStatement::AssignConstBool { dst, value, .. } = stmt {
            if *dst != effective_discr {
                continue;
            }
            let arm_value: u128 = if *value { 1 } else { 0 };
            let arm_target = succ_targets
                .iter()
                .find(|(v, _)| *v == arm_value)
                .map(|(_, t)| *t)
                .unwrap_or(succ_otherwise);
            return build_decision_from_block(arm_target, f);
        }
    }
    None
}

/// Follow `_X = copy _Y` aliases backwards from `local` within
/// `block`. Used by [`try_fold_into_successor_switchint`] when the
/// successor block copies a local into a fresh temporary before
/// `switchInt`-ing on it.
///
/// Iterates with a step cap to defend against pathological cycles
/// the MIR grammar shouldn't admit but that a malformed input
/// might.
fn resolve_switchint_discr_alias(local: LocalId, block: &MirBlock) -> LocalId {
    let mut current = local;
    let max_iter = block.statements.len() + 1;
    for _ in 0..max_iter {
        let next = block.statements.iter().rev().find_map(|s| match s {
            MirStatement::AssignCopy { dst, src, .. } if *dst == current => Some(*src),
            _ => None,
        });
        match next {
            Some(src) if src != current => current = src,
            _ => return current,
        }
    }
    current
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
        } if *dst == discr => compare_condition_name(*op, lhs, rhs, f),
        _ => None,
    })
}

/// Synthesize a human-readable condition name for an inline
/// comparison like `speed > 50` or `a < b`.
///
/// Returns `None` if either operand is an un-named MIR local
/// (Issue #18): surfacing `_3 == _4`-style names in reports is
/// unanalysable and would mislead a user. The None propagates up
/// through `synthesize_compare_name` to the decomposer, which
/// then declines the function — "no decision recovered" is the
/// correct output here.
pub(crate) fn compare_condition_name(
    op: CompareOp,
    lhs: &Operand,
    rhs: &Operand,
    f: &MirFunction,
) -> Option<String> {
    Some(format!(
        "{} {} {}",
        operand_display(lhs, f)?,
        op.as_source_str(),
        operand_display(rhs, f)?,
    ))
}

/// Render a MIR operand as a human-readable string, or return `None`
/// if it would be a MIR-internal `_N` placeholder (unnamed local).
/// See [`compare_condition_name`] for why the None short-circuits.
///
/// An unnamed local that is a pure alias of a debug-named one
/// (`_4 = copy _2; _3 = Gt(move _4, const ..)`, the shape rustc
/// emits for `while ms > N` on a `mut` argument) renders as the
/// ancestor's name; see [`resolve_alias_debug_name`].
fn operand_display(op: &Operand, f: &MirFunction) -> Option<String> {
    match op {
        Operand::Copy(id) | Operand::Move(id) => resolve_alias_debug_name(*id, f),
        Operand::Const(_) => Some(op.const_display_value().to_string()),
    }
}

/// Maximum number of `_N = copy _M` / `_N = move _M` hops
/// [`resolve_alias_debug_name`] follows before giving up.
const MAX_ALIAS_HOPS: usize = 4;

/// Return the debug name of `local`, or of the debug-named local it
/// is a pure alias of.
///
/// When `local` has no `debug` annotation, walk backwards through
/// plain value aliases (`_N = copy _M`, `_N = move _M`) to the first
/// debug-named ancestor, at most [`MAX_ALIAS_HOPS`] hops and never
/// revisiting a local. A hop is taken only when every write to the
/// local in the whole function is a plain alias of one and the same
/// source (see [`unique_alias_source`]); anything else (a deref
/// copy, a cast, arithmetic, a field projection, a call result, an
/// argument's implicit entry value, or two different sources) stops
/// the walk with `None`, so ambiguity never produces a wrong name.
fn resolve_alias_debug_name(local: LocalId, f: &MirFunction) -> Option<String> {
    let mut current = local;
    let mut visited = vec![local];
    for _ in 0..MAX_ALIAS_HOPS {
        if let Some(name) = f.debug_names.get(&current) {
            return Some(name.clone());
        }
        current = unique_alias_source(current, f)?;
        if visited.contains(&current) {
            return None;
        }
        visited.push(current);
    }
    f.debug_names.get(&current).cloned()
}

/// If every write to `local` in `f` is a plain value alias of the
/// same source local, return that source. Returns `None` when
/// `local` is never written, is a function argument (written
/// implicitly on entry), is written by anything other than a plain
/// alias, or is aliased from more than one distinct source.
fn unique_alias_source(local: LocalId, f: &MirFunction) -> Option<LocalId> {
    if f.args.iter().any(|a| a.local == local) {
        return None;
    }
    let mut source: Option<LocalId> = None;
    for block in &f.blocks {
        for stmt in &block.statements {
            let alias_src = match stmt {
                MirStatement::AssignCopy {
                    dst,
                    src,
                    deref: false,
                    ..
                } if *dst == local => *src,
                MirStatement::Other { text, .. } => match plain_move_alias(text) {
                    Some((dst, src)) if dst == local => src,
                    _ if other_text_writes(text, local) => return None,
                    _ => continue,
                },
                other if statement_writes(other, local) => return None,
                _ => continue,
            };
            match source {
                None => source = Some(alias_src),
                Some(prev) if prev == alias_src => {}
                Some(_) => return None,
            }
        }
        let terminator_writes = match &block.terminator {
            MirTerminator::Call { dst, .. } | MirTerminator::NoReturnCall { dst, .. } => {
                *dst == local
            }
            MirTerminator::Other { text, .. } => other_text_writes(text, local),
            _ => false,
        };
        if terminator_writes {
            return None;
        }
    }
    source
}

/// Parse an unparsed statement of the exact form `_N = move _M;`
/// into `(N, M)`. Any other shape (projections, derefs, casts,
/// operations) returns `None`.
fn plain_move_alias(text: &str) -> Option<(LocalId, LocalId)> {
    let s = text.trim();
    let s = s.strip_suffix(';').unwrap_or(s);
    let (lhs, rhs) = s.split_once(" = ")?;
    let dst = parse_plain_local(lhs.trim())?;
    let src = parse_plain_local(rhs.trim().strip_prefix("move ")?)?;
    Some((dst, src))
}

/// Parse exactly `_<digits>` into a [`LocalId`].
fn parse_plain_local(s: &str) -> Option<LocalId> {
    let digits = s.strip_prefix('_')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Conservatively decide whether an unparsed statement / terminator
/// text assigns to `local`: true when its left-hand side (before
/// ` = `) is `local` itself or a place rooted at it, such as
/// `(_N.0: T)` or `(*_N)`.
fn other_text_writes(text: &str, local: LocalId) -> bool {
    let Some((lhs, _)) = text.trim().split_once(" = ") else {
        return false;
    };
    let root = lhs.trim_start_matches(['(', '*', ' ']);
    let Some(digits) = root.strip_prefix('_') else {
        return false;
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<LocalId>().ok() == Some(local)
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

    /// `rustc --emit=mir` output for `fn decide(a, b) -> bool { a && b }`
    /// as emitted with `-Zcoverage-options=branch,condition` — the
    /// terminal `b` leaf has an extra `switchInt(_0)` branch counter
    /// whose arms both `goto` a common successor without touching
    /// `_0`. The engine should treat it as pure instrumentation and
    /// recover the same ITE as the un-instrumented shape.
    const DECIDE_COVERAGE_COUNTER_AND_MIR: &str = r#"
fn decide(_1: bool, _2: bool) -> bool {
    debug a => _1;
    debug b => _2;
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _0 = copy _2;
        switchInt(copy _0) -> [0: bb4, otherwise: bb3];
    }

    bb2: {
        _0 = const false;
        goto -> bb5;
    }

    bb3: {
        goto -> bb5;
    }

    bb4: {
        goto -> bb5;
    }

    bb5: {
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

    /// `rustc --emit=mir` output for `fn decide(a) -> bool { !a }`
    /// as emitted with `-Zcoverage-options=branch,condition` — the
    /// `!` becomes an explicit `_0 = Not(copy _1)` statement
    /// followed by a coverage branch counter switchInt.
    const DECIDE_NOT_WITH_COVERAGE_COUNTER_MIR: &str = r#"
fn decide(_1: bool) -> bool {
    debug a => _1;
    let mut _0: bool;

    bb0: {
        _0 = Not(copy _1);
        switchInt(copy _0) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        goto -> bb3;
    }

    bb2: {
        goto -> bb3;
    }

    bb3: {
        return;
    }
}
"#;

    #[test]
    fn decompose_recovers_unary_not_as_negated_condition() {
        // `!a` compiled with `-Zcoverage-options=branch,condition`:
        // the Not is materialised as `_0 = Not(copy _1)` followed
        // by a coverage branch counter switchInt(_0). The
        // decomposer treats the terminal value as `Condition{"!a"}`
        // so the branch counter's true/false counts (which track
        // the Not's OUTPUT value) map onto the correct condition
        // name in the report. Alternative naming (`Condition{"a"}`
        // with an ITE inversion) would display the Not's output
        // under the raw-input name — confusing to readers.
        let mir = mir::parse_text(DECIDE_NOT_WITH_COVERAGE_COUNTER_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], cond("!a"));
    }

    #[test]
    fn decompose_sees_through_coverage_branch_counter_on_terminal_leaf() {
        // `a && b` compiled with `-Zcoverage-options=branch,condition`.
        // The terminal `b` leaf becomes `_0 = copy _2; switchInt(_0)
        // -> [0: bb4, otherwise: bb3]` with `bb3` and `bb4` both just
        // `goto bb5`. That inner switchInt is a rustc coverage branch
        // counter, not a logical switch — the decomposer treats it
        // as if the block ended `goto bb5` and recovers the same ITE
        // as the un-instrumented shape. Without this handling,
        // coverage-instrumented MIR for any pure-boolean function
        // whose body IS the returned expression declines to
        // decompose (production-visible: no user running `cargo
        // floyd test` on `pub fn foo(...) -> bool { ... }` would
        // see MC/DC output).
        let mir = mir::parse_text(DECIDE_COVERAGE_COUNTER_AND_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], ite("a", cond("b"), const_(false)));
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
    /// No binding => no downcast => the `if let` detector can't tell
    /// matched from unmatched by structure. The match handler picks
    /// it up instead and names the condition by discriminant index
    /// rather than variant name.
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
    fn decompose_recovers_no_binding_if_let_via_match_handler() {
        // if let Some(_) = opt { true } else { false }
        // No downcast => the if-let detector declines, but the match
        // handler recovers it as an enum-discriminant match. Option's
        // type prints with generics so the namer drops the type
        // prefix, leaving the variant index (Some = 1) bare.
        let mir = mir::parse_text(DECIDE_IF_LET_NO_BINDING_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite("opt == variant_1", const_(true), const_(false))
        );
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
    /// `fn classify(n: i32, b: bool) -> bool {
    ///     let lit = match n { 0 => true, _ => false };
    ///     lit && b
    /// }`.
    /// Two decisions in source — the match produces an intermediate
    /// `lit`, then `lit && b` branches on it. The intermediate is
    /// realised in MIR as `_3 = const true/false; goto bb3; ...; _4 =
    /// copy _3; switchInt(move _4)`. Before the fold helper landed,
    /// the engine stopped at the per-arm `_3 = const ...` and missed
    /// the `&& b` downstream.
    const DECIDE_MULTI_DECISION_MATCH_AND_MIR: &str = r#"
fn classify(_1: i32, _2: bool) -> bool {
    debug n => _1;
    debug b => _2;
    let mut _0: bool;
    let _3: bool;
    let mut _4: bool;
    scope 1 {
        debug lit => _3;
    }

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _3 = const false;
        goto -> bb3;
    }

    bb2: {
        _3 = const true;
        goto -> bb3;
    }

    bb3: {
        _4 = copy _3;
        switchInt(move _4) -> [0: bb5, otherwise: bb4];
    }

    bb4: {
        _0 = copy _2;
        goto -> bb6;
    }

    bb5: {
        _0 = const false;
        goto -> bb6;
    }

    bb6: {
        return;
    }
}
"#;

    #[test]
    fn decompose_folds_match_intermediate_into_downstream_and() {
        // (n == 0) && b
        //   <=>   if (n == 0) then b else false
        // The match produces an intermediate `lit`; the `&&` then
        // branches on `lit`. The fold helper propagates the const
        // arms (`_3 = const true` and `_3 = const false`) into the
        // downstream `switchInt(_4)` (aliased from `_3` via the
        // `_4 = copy _3` in bb3), letting the engine recover the
        // full decision.
        let mir = mir::parse_text(DECIDE_MULTI_DECISION_MATCH_AND_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(tree.decisions[0], ite("n == 0", cond("b"), const_(false)));
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
    /// 2-variant exhaustive enum match without binding. The
    /// decomposer synthesizes condition names from the scrutinee's
    /// type and rustc's discriminant index — `m == Mode::variant_0`.
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
    fn decompose_recovers_enum_match_without_binding() {
        // 2-variant exhaustive: `match m { Mode::On => true, Mode::Off => false }`
        //   <=>   if (m == Mode::variant_0) then true else false
        let mir = mir::parse_text(DECIDE_MATCH_ENUM_NO_BINDING_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite("m == Mode::variant_0", const_(true), const_(false))
        );
    }

    /// 3-variant exhaustive enum match: all arms explicit, otherwise
    /// is unreachable.
    const DECIDE_MATCH_ENUM_THREE_EXHAUSTIVE_MIR: &str = r#"
fn decide(_1: SystemState) -> bool {
    debug state => _1;
    let mut _0: bool;
    let mut _2: isize;

    bb0: {
        _2 = discriminant(_1);
        switchInt(move _2) -> [0: bb4, 1: bb3, 2: bb2, otherwise: bb1];
    }

    bb1: {
        unreachable;
    }

    bb2: {
        _0 = const true;
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
}
"#;

    #[test]
    fn decompose_recovers_enum_match_three_variant_exhaustive() {
        // match state { Operational => false, Degraded => false, Fault => true }
        //   <=> Ite{state == SystemState::variant_0, false,
        //          Ite{state == SystemState::variant_1, false, true}}
        let mir = mir::parse_text(DECIDE_MATCH_ENUM_THREE_EXHAUSTIVE_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite(
                "state == SystemState::variant_0",
                const_(false),
                ite(
                    "state == SystemState::variant_1",
                    const_(false),
                    const_(true),
                ),
            )
        );
    }

    /// Enum match with explicit variants + wildcard: otherwise is
    /// reachable and represents the wildcard arm.
    const DECIDE_MATCH_ENUM_WITH_WILDCARD_MIR: &str = r#"
fn decide(_1: SystemState) -> bool {
    debug state => _1;
    let mut _0: bool;
    let mut _2: isize;

    bb0: {
        _2 = discriminant(_1);
        switchInt(move _2) -> [2: bb2, otherwise: bb1];
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

    #[test]
    fn decompose_recovers_enum_match_with_wildcard() {
        // match state { Fault => true, _ => false }
        //   <=> Ite{state == SystemState::variant_2, true, false}
        let mir = mir::parse_text(DECIDE_MATCH_ENUM_WITH_WILDCARD_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite(
                "state == SystemState::variant_2",
                const_(true),
                const_(false)
            )
        );
    }

    /// Enum match where the scrutinee's type isn't a simple identifier
    /// — falls back to bare `variant_<N>` naming.
    const DECIDE_MATCH_ENUM_GENERIC_TYPE_MIR: &str = r#"
fn decide(_1: Result<bool, ()>) -> bool {
    debug r => _1;
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
    fn decompose_falls_back_to_bare_variant_name_for_generic_types() {
        // Result<bool, ()> isn't a simple identifier — the namer drops
        // the type prefix to avoid producing ugly conditions like
        // `r == Result<bool, ()>::variant_0`.
        let mir = mir::parse_text(DECIDE_MATCH_ENUM_GENERIC_TYPE_MIR).expect("MIR parses");
        let tree = decompose(&mir);
        assert_eq!(tree.decisions.len(), 1);
        assert_eq!(
            tree.decisions[0],
            ite("r == variant_0", const_(true), const_(false))
        );
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

    #[test]
    fn decompose_from_block_at_entry_matches_decompose_function() {
        // Calling decompose_from_block with the function's entry
        // block id must produce exactly the same node as
        // decompose_function (the latter is a thin wrapper).
        let mir = mir::parse_text(DECIDE_AND_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        let entry_id = f.blocks.first().expect("has entry block").id;
        let via_wrapper = decompose_function(f);
        let via_direct = decompose_from_block(f, entry_id);
        assert!(via_wrapper.is_some());
        assert_eq!(via_wrapper, via_direct);
    }

    #[test]
    fn operand_display_returns_name_for_debug_named_local() {
        // Named local renders via debug_names.
        let mut f = MirFunction::default();
        f.debug_names.insert(5, "speed".to_string());
        let op = Operand::Copy(5);
        assert_eq!(operand_display(&op, &f), Some("speed".to_string()));
    }

    #[test]
    fn operand_display_returns_none_for_unnamed_local() {
        // Issue #18: unnamed local (no debug_names entry) must
        // return None rather than falling back to .
        let f = MirFunction::default();
        let op = Operand::Copy(3);
        assert_eq!(operand_display(&op, &f), None);
    }

    /// `rustc --emit=mir` output for cortex-m's `Delay::delay_ms`
    /// shape: `while ms > 4294967 { ...; ms -= 4294967; }` on a
    /// `mut` argument. rustc copies the debug-named `_2` into the
    /// unnamed temporary `_4` before the comparison.
    const DELAY_MS_ALIAS_MIR: &str = r#"
fn <impl at a.rs:2:1: 2:7>::delay_ms(_1: &mut D, _2: u32) -> () {
    debug self => _1;
    debug ms => _2;
    let mut _0: ();
    let mut _3: bool;
    let mut _4: u32;
    let _5: ();
    let mut _6: (u32, bool);
    let _7: ();
    let mut _8: u32;
    let mut _9: u32;
    let mut _10: (u32, bool);

    bb0: {
        goto -> bb1;
    }

    bb1: {
        _4 = copy _2;
        _3 = Gt(move _4, const 4294967_u32);
        switchInt(move _3) -> [0: bb5, otherwise: bb2];
    }

    bb2: {
        _5 = D::delay_us(copy _1, const 4294967000_u32) -> [return: bb3, unwind continue];
    }

    bb3: {
        _6 = SubWithOverflow(copy _2, const 4294967_u32);
        assert(!move (_6.1: bool), "attempt to compute `{} - {}`, which would overflow", copy _2, const 4294967_u32) -> [success: bb4, unwind continue];
    }

    bb4: {
        _2 = move (_6.0: u32);
        goto -> bb1;
    }

    bb5: {
        _9 = copy _2;
        _10 = MulWithOverflow(copy _9, const 1000_u32);
        assert(!move (_10.1: bool), "attempt to compute `{} * {}`, which would overflow", move _9, const 1000_u32) -> [success: bb6, unwind continue];
    }

    bb6: {
        _8 = move (_10.0: u32);
        _7 = D::delay_us(copy _1, move _8) -> [return: bb7, unwind continue];
    }

    bb7: {
        return;
    }
}
"#;

    /// `rustc --emit=mir` output for
    /// `let x; if c { x = a } else { x = b } x > 5`, with the
    /// `debug x => _5;` annotation absent (as when debuginfo for the
    /// binding is not emitted). `_6` aliases `_5`, but `_5` is
    /// assigned from two different named locals, so no single name
    /// is correct.
    const ASSIGNED_TWICE_MIR: &str = r#"
fn <impl at a.rs:2:1: 2:7>::twice(_1: &mut D, _2: u32, _3: u32, _4: bool) -> bool {
    debug self => _1;
    debug a => _2;
    debug b => _3;
    debug c => _4;
    let mut _0: bool;
    let _5: u32;
    let mut _6: u32;

    bb0: {
        switchInt(copy _4) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _5 = copy _2;
        goto -> bb3;
    }

    bb2: {
        _5 = copy _3;
        goto -> bb3;
    }

    bb3: {
        _6 = copy _5;
        _0 = Gt(move _6, const 5_u32);
        return;
    }
}
"#;

    /// Find the single `AssignBinaryCompare` in `f` and render its
    /// condition name.
    fn first_compare_name(f: &MirFunction) -> Option<String> {
        let (op, lhs, rhs) = f
            .blocks
            .iter()
            .flat_map(|b| b.statements.iter())
            .find_map(|s| match s {
                MirStatement::AssignBinaryCompare { op, lhs, rhs, .. } => Some((*op, lhs, rhs)),
                _ => None,
            })
            .expect("fixture has a compare");
        compare_condition_name(op, lhs, rhs, f)
    }

    #[test]
    fn operand_display_follows_one_hop_copy_alias() {
        let mir = mir::parse_text(DELAY_MS_ALIAS_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        assert!(!f.debug_names.contains_key(&4), "_4 is unnamed");
        assert_eq!(
            operand_display(&Operand::Move(4), f),
            Some("ms".to_string())
        );
        assert_eq!(first_compare_name(f).as_deref(), Some("ms > 4294967"));
    }

    #[test]
    fn loop_condition_alias_is_not_the_block_value() {
        // `bb1: _4 = copy _2; _3 = Gt(move _4, ..); switchInt(move _3)`.
        // `_4` is compare-operand set-up consumed in the block; it
        // must not surface as a `ms` condition leaf. The block's
        // real decision is `ms > 4294967`, whose arms are a
        // non-panic Call and an overflow Assert.
        let mir = mir::parse_text(DELAY_MS_ALIAS_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        assert_eq!(
            decompose_from_block(f, 1),
            Some(Node::Ite {
                cond: "ms > 4294967".to_string(),
                then_branch: Box::new(Node::Const { value: true }),
                else_branch: Box::new(Node::Const { value: false }),
            })
        );
    }

    #[test]
    fn operand_display_declines_alias_assigned_from_two_sources() {
        let mir = mir::parse_text(ASSIGNED_TWICE_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        // `_6 = copy _5` is a unique alias, but `_5` is written from
        // both `a` and `b`: ambiguous, so no name.
        assert_eq!(operand_display(&Operand::Move(6), f), None);
        assert_eq!(operand_display(&Operand::Copy(5), f), None);
        assert_eq!(first_compare_name(f), None);
    }

    #[test]
    fn operand_display_does_not_follow_deref_or_projection() {
        let mut f = MirFunction::default();
        f.debug_names.insert(2, "p".to_string());
        f.debug_names.insert(7, "pair".to_string());
        f.blocks.push(MirBlock {
            id: 0,
            statements: vec![
                // `_4 = copy (*_2)` reads the pointee, not `p`.
                MirStatement::AssignCopy {
                    dst: 4,
                    src: 2,
                    deref: true,
                    span: None,
                },
                // `_5 = move (_7.0: u32)` is a field projection.
                MirStatement::Other {
                    text: "_5 = move (_7.0: u32);".to_string(),
                    span: None,
                },
                // `_6 = move _2` is a plain move alias.
                MirStatement::Other {
                    text: "_6 = move _2;".to_string(),
                    span: None,
                },
            ],
            terminator: MirTerminator::Return { span: None },
        });
        assert_eq!(operand_display(&Operand::Copy(4), &f), None);
        assert_eq!(operand_display(&Operand::Copy(5), &f), None);
        assert_eq!(
            operand_display(&Operand::Copy(6), &f),
            Some("p".to_string())
        );
    }

    #[test]
    fn operand_display_alias_walk_terminates_on_cycle() {
        let mut f = MirFunction::default();
        let copy = |dst, src| MirStatement::AssignCopy {
            dst,
            src,
            deref: false,
            span: None,
        };
        f.blocks.push(MirBlock {
            id: 0,
            statements: vec![copy(3, 4), copy(4, 3)],
            terminator: MirTerminator::Return { span: None },
        });
        assert_eq!(operand_display(&Operand::Copy(3), &f), None);
    }

    #[test]
    fn compare_condition_name_declines_on_unnamed_operand() {
        // Issue #18: if either operand lacks a debug name,
        // compare_condition_name returns None so the decomposer
        // declines the whole function.
        let mut f = MirFunction::default();
        f.debug_names.insert(1, "a".to_string());
        // One named, one unnamed.
        let got = compare_condition_name(
            CompareOp::Eq,
            &Operand::Copy(1),
            &Operand::Copy(2), // not in debug_names
            &f,
        );
        assert_eq!(got, None);
        // Both named — succeeds.
        f.debug_names.insert(2, "b".to_string());
        let got = compare_condition_name(CompareOp::Eq, &Operand::Copy(1), &Operand::Copy(2), &f);
        assert_eq!(got.as_deref(), Some("a == b"));
    }

    /// `rustc +nightly --emit=mir -Zmir-include-spans -C overflow-checks=on
    /// -Cinstrument-coverage -Zcoverage-options=branch,condition` output
    /// (nightly 1.101, 2026-10-28; the flags cargo-floyd builds with)
    /// for
    ///
    /// ```text
    /// pub fn pick(c: bool, a: u32, b: u32) -> u32 {
    ///     if c { a + 1 } else { b * 2 }
    /// }
    /// ```
    ///
    /// Both arms of `if c` end in an overflow-check `assert`.
    const PICK_ASSERT_ARMS_MIR: &str = r#"
fn pick(_1: bool, _2: u32, _3: u32) -> u32 {
    debug c => _1;                       // in scope 0 at pick.rs:1:13: 1:14
    debug a => _2;                       // in scope 0 at pick.rs:1:22: 1:23
    debug b => _3;                       // in scope 0 at pick.rs:1:30: 1:31
    let mut _0: u32;                     // return place in scope 0 at pick.rs:1:41: 1:44
    let mut _4: (u32, bool);             // in scope 0 at pick.rs:2:12: 2:17
    let mut _5: (u32, bool);             // in scope 0 at pick.rs:2:27: 2:32

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => pick.rs:2:8: 2:9 (#0)

    coverage Code { bcb: bcb0 } => pick.rs:1:1: 1:44 (#0);
    coverage Code { bcb: bcb0 } => pick.rs:2:8: 2:9 (#0);
    coverage Code { bcb: bcb1 } => pick.rs:2:12: 2:17 (#0);
    coverage Code { bcb: bcb3 } => pick.rs:2:27: 2:32 (#0);
    coverage Code { bcb: bcb2 } => pick.rs:3:1: 3:2 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => pick.rs:2:8: 2:9 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at pick.rs:2:8: 2:9
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at pick.rs:2:8: 2:9
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at pick.rs:2:12: 2:17
        _4 = AddWithOverflow(copy _2, const 1_u32); // scope 0 at pick.rs:2:12: 2:17
        assert(!move (_4.1: bool), "attempt to compute `{} + {}`, which would overflow", copy _2, const 1_u32) -> [success: bb3, unwind continue]; // scope 0 at pick.rs:2:12: 2:17
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at pick.rs:2:27: 2:32
        _5 = MulWithOverflow(copy _3, const 2_u32); // scope 0 at pick.rs:2:27: 2:32
        assert(!move (_5.1: bool), "attempt to compute `{} * {}`, which would overflow", copy _3, const 2_u32) -> [success: bb4, unwind continue]; // scope 0 at pick.rs:2:27: 2:32
    }

    bb3: {
        _0 = move (_4.0: u32);           // scope 0 at pick.rs:2:12: 2:17
        goto -> bb5;                     // scope 0 at pick.rs:2:5: 2:34
    }

    bb4: {
        _0 = move (_5.0: u32);           // scope 0 at pick.rs:2:27: 2:32
        goto -> bb5;                     // scope 0 at pick.rs:2:5: 2:34
    }

    bb5: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at pick.rs:3:2: 3:2
        return;                          // scope 0 at pick.rs:3:2: 3:2
    }
}
"#;

    /// Same toolchain, `-C overflow-checks=on -C debug-assertions=on`,
    /// for
    ///
    /// ```text
    /// pub fn add_checked(a: u32, b: u32) -> u32 {
    ///     debug_assert!(a < 1000);
    ///     a + b
    /// }
    /// ```
    ///
    /// The `debug_assert!` guard's false arm diverges into
    /// `core::panicking::panic`; its true arm is an overflow assert.
    const DEBUG_ASSERT_GUARD_MIR: &str = r#"
fn add_checked(_1: u32, _2: u32) -> u32 {
    debug a => _1;                       // in scope 0 at ov.rs:1:20: 1:21
    debug b => _2;                       // in scope 0 at ov.rs:1:28: 1:29
    let mut _0: u32;                     // return place in scope 0 at ov.rs:1:39: 1:42
    let mut _3: bool;                    // in scope 0 at ov.rs:2:19: 2:27
    let mut _4: !;                       // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:293:13: 293:38
    let mut _5: (u32, bool);             // in scope 0 at ov.rs:3:5: 3:10

    bb0: {
        _3 = Lt(copy _1, const 1000_u32); // scope 0 at ov.rs:2:19: 2:27
        switchInt(move _3) -> [0: bb1, otherwise: bb2]; // scope 0 at ov.rs:2:19: 2:27
    }

    bb1: {
        _4 = core::panicking::panic(const "assertion failed: a < 1000") -> unwind continue; // scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:293:13: 293:38
                                         // mir::ConstOperand
                                         // + span: /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:293:13: 293:38
                                         // + const_: Const { ty: fn(&'static str) -> ! {core::panicking::panic}, val: Value(core::panicking::panic) }
                                         // mir::ConstOperand
                                         // + span: no-location
                                         // + const_: Const { ty: &str, val: Value("assertion failed: a < 1000") }
    }

    bb2: {
        _5 = AddWithOverflow(copy _1, copy _2); // scope 0 at ov.rs:3:5: 3:10
        assert(!move (_5.1: bool), "attempt to compute `{} + {}`, which would overflow", copy _1, copy _2) -> [success: bb3, unwind continue]; // scope 0 at ov.rs:3:5: 3:10
    }

    bb3: {
        _0 = move (_5.0: u32);           // scope 0 at ov.rs:3:5: 3:10
        return;                          // scope 0 at ov.rs:4:2: 4:2
    }
}
"#;

    #[test]
    fn branch_taken_lift_accepts_parsed_assert_arms() {
        let mir = mir::parse_text(PICK_ASSERT_ARMS_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        assert!(matches!(
            f.blocks[1].terminator,
            MirTerminator::Assert { success: 3, .. }
        ));
        let node = decompose_function(f).expect("`if c` with assert arms is a decision");
        assert_eq!(
            node,
            Node::Ite {
                cond: "c".to_string(),
                then_branch: Box::new(Node::Const { value: true }),
                else_branch: Box::new(Node::Const { value: false }),
            }
        );
    }

    #[test]
    fn debug_assert_guard_is_not_a_decision() {
        // One arm is a parsed overflow `Assert`, the other the
        // diverging `core::panicking::panic` call: the guard of a
        // `debug_assert!` is not a user decision.
        let mir = mir::parse_text(DEBUG_ASSERT_GUARD_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        assert!(matches!(
            f.blocks[2].terminator,
            MirTerminator::Assert { success: 3, .. }
        ));
        assert_eq!(decompose_function(f), None);
    }

    /// `cargo floyd test` MIR (rustc nightly 1.101, coverage flags,
    /// comment lines stripped) for regex 1.x `Captures::extract` in
    /// `src/regex/bytes.rs`:
    ///
    /// ```text
    /// pub fn extract<const N: usize>(&self) -> (&'h [u8], [&'h [u8]; N]) {
    ///     let len = self.static_captures_len.expect(..).checked_sub(1).expect(..);
    ///     assert_eq!(N, len, "asked for {N} groups, but must ask for {len}", ..);
    ///     self.caps.extract_bytes(self.haystack)
    /// }
    /// ```
    ///
    /// `bb3` is the `assert_eq!` expansion: `_8 = copy (*_19)` reads
    /// through the `left_val` reference, `_7 = Eq(copy _2, move _8)`
    /// and `switchInt(move _7)` branches to the `assert_failed` panic
    /// path (`bb5`..`bb8`) or the real body (`bb4`).
    const REGEX_EXTRACT_ASSERT_EQ_MIR: &str = r#"
fn regex::bytes::<impl at src/regex/bytes.rs:1691:1: 1691:22>::extract(_1: &regex::bytes::Captures<'_>) -> (&[u8], [&[u8]; N]) {
    debug self => _1;
    let mut _0: (&[u8], [&[u8]; N]);
    let _2: usize;
    let mut _3: core::option::Option<usize>;
    let mut _4: usize;
    let mut _5: core::option::Option<usize>;
    let mut _6: &usize;
    let mut _7: bool;
    let mut _8: usize;
    let _9: !;
    let mut _10: core::option::Option<core::fmt::Arguments<'_>>;
    let mut _11: core::fmt::Arguments<'_>;
    let mut _12: &usize;
    let mut _14: core::fmt::rt::Argument<'_>;
    let mut _15: core::fmt::rt::Argument<'_>;
    let _16: &[core::fmt::rt::Argument<'_>; 2];
    let mut _17: &regex_automata::util::captures::Captures;
    let mut _20: &[u8];
    scope 1 {
        debug len => _2;
        let mut _19: &usize;
        scope 2 {
            debug left_val => _19;
            debug right_val => _6;
            scope 3 {
                debug kind => const core::panicking::AssertKind::Eq;
                let mut _18: &usize;
                scope 4 {
                    debug ((args: (&usize, &usize)).0: &usize) => _18;
                    debug ((args: (&usize, &usize)).1: &usize) => _12;
                    let _13: [core::fmt::rt::Argument<'_>; 2];
                    scope 5 {
                        debug args => _13;
                    }
                }
            }
        }
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:60:20: 60:46 (#39)

    coverage Code { bcb: bcb0 } => src/regex/bytes.rs:1857:5: 1857:71 (#0);
    coverage Code { bcb: bcb0 } => src/regex/bytes.rs:1858:19: 1862:68 (#0);
    coverage Code { bcb: bcb0 } => src/regex/bytes.rs:1863:9: 1863:19 (#0);
    coverage Code { bcb: bcb0 } => src/regex/bytes.rs:1863:20: 1863:21 (#0);
    coverage Code { bcb: bcb0 } => src/regex/bytes.rs:1863:23: 1863:26 (#0);
    coverage Code { bcb: bcb1 } => src/regex/bytes.rs:1870:9: 1870:47 (#0);
    coverage Code { bcb: bcb1 } => src/regex/bytes.rs:1871:5: 1871:6 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);
        StorageLive(_2);
        StorageLive(_3);
        StorageLive(_4);
        StorageLive(_5);
        _5 = copy ((*_1).2: core::option::Option<usize>);
        _4 = Option::<usize>::expect(move _5, const "number of capture groups can vary in a match") -> [return: bb1, unwind continue];
    }

    bb1: {
        StorageDead(_5);
        _3 = core::num::<impl usize>::checked_sub(move _4, const 1_usize) -> [return: bb2, unwind continue];
    }

    bb2: {
        StorageDead(_4);
        _2 = Option::<usize>::expect(move _3, const "number of groups is always greater than zero") -> [return: bb3, unwind continue];
    }

    bb3: {
        StorageDead(_3);
        _19 = const regex::bytes::Captures::<'_>::extract::<N>::promoted[1];
        _6 = &_2;
        StorageLive(_7);
        StorageLive(_8);
        _8 = copy (*_19);
        _7 = Eq(copy _2, move _8);
        switchInt(move _7) -> [0: bb5, otherwise: bb4];
    }

    bb4: {
        Coverage::VirtualCounter(bcb1);
        StorageDead(_8);
        StorageDead(_7);
        StorageLive(_17);
        _17 = &((*_1).1: regex_automata::util::captures::Captures);
        _20 = no_retag copy ((*_1).0: &[u8]);
        _0 = regex_automata::util::captures::Captures::extract_bytes::<N>(move _17, move _20) -> [return: bb9, unwind continue];
    }

    bb5: {
        Coverage::VirtualCounter(bcb2);
        StorageDead(_8);
        StorageLive(_10);
        StorageLive(_11);
        _18 = const regex::bytes::Captures::<'_>::extract::<N>::promoted[0];
        _12 = &_2;
        StorageLive(_13);
        StorageLive(_14);
        _14 = core::fmt::rt::Argument::<'_>::new_display::<usize>(move _18) -> [return: bb6, unwind continue];
    }

    bb6: {
        StorageLive(_15);
        _15 = core::fmt::rt::Argument::<'_>::new_display::<usize>(move _12) -> [return: bb7, unwind continue];
    }

    bb7: {
        _13 = [move _14, move _15];
        StorageDead(_15);
        StorageDead(_14);
        StorageLive(_16);
        _16 = &_13;
        _11 = Arguments::<'_>::new::<41, 2>(const b"\nasked for \xc0\x1a groups, but must ask for \xc0\x00", move _16) -> [return: bb8, unwind continue];
    }

    bb8: {
        StorageDead(_16);
        _10 = Option::<Arguments<'_>>::Some(move _11);
        StorageDead(_11);
        _9 = core::panicking::assert_failed::<usize, usize>(const core::panicking::AssertKind::Eq, move _19, move _6, move _10) -> unwind continue;
    }

    bb9: {
        StorageDead(_17);
        StorageDead(_2);
        return;
    }
}
"#;

    /// rustc nightly 1.101 MIR (coverage flags, comments stripped) for
    ///
    /// ```text
    /// pub fn check(x: usize, v: &[u8]) -> Vec<u8> {
    ///     assert!(x < 3, "bad {}", x);
    ///     v.to_vec()
    /// }
    /// ```
    ///
    /// The condition is nameable (`x < 3`) and both arms' first
    /// blocks end in non-panic Calls (`to_vec` / `new_display`), but
    /// the `0` arm continues into `std::rt::panic_fmt`.
    const ASSERT_MSG_GUARD_MIR: &str = r#"
fn check(_1: usize, _2: &[u8]) -> Vec<u8> {
    debug x => _1;
    debug v => _2;
    let mut _0: std::vec::Vec<u8>;
    let mut _3: bool;
    let _4: !;
    let mut _5: std::fmt::Arguments<'_>;
    let _6: (&usize,);
    let mut _7: &usize;
    let mut _9: core::fmt::rt::Argument<'_>;
    let mut _10: &[u8; 7];
    let _11: &[core::fmt::rt::Argument<'_>; 1];
    let mut _12: &usize;
    scope 1 {
        debug args => _6;
        let _8: [core::fmt::rt::Argument<'_>; 1];
        scope 2 {
            debug args => _8;
        }
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => chk2.rs:2:5: 2:32 (#4)

    coverage Code { bcb: bcb0 } => chk2.rs:1:1: 1:44 (#0);
    coverage Code { bcb: bcb0 } => chk2.rs:2:5: 2:12 (#0);
    coverage Code { bcb: bcb0 } => chk2.rs:2:13: 2:18 (#0);
    coverage Code { bcb: bcb1 } => chk2.rs:3:5: 3:15 (#0);
    coverage Code { bcb: bcb1 } => chk2.rs:4:1: 4:2 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);
        _3 = Lt(copy _1, const 3_usize);
        switchInt(move _3) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);
        _0 = slice::<impl [u8]>::to_vec(copy _2) -> [return: bb5, unwind continue];
    }

    bb2: {
        Coverage::VirtualCounter(bcb2);
        _7 = &_1;
        _6 = (move _7,);
        _12 = no_retag copy (_6.0: &usize);
        _9 = core::fmt::rt::Argument::<'_>::new_display::<usize>(copy _12) -> [return: bb3, unwind continue];
    }

    bb3: {
        _8 = [move _9];
        _10 = const b"\x04bad \xc0\x00";
        _11 = &_8;
        _5 = Arguments::<'_>::new::<7, 1>(move _10, copy _11) -> [return: bb4, unwind continue];
    }

    bb4: {
        _4 = std::rt::panic_fmt(move _5) -> unwind continue;
    }

    bb5: {
        return;
    }
}
"#;

    #[test]
    fn assert_eq_block_is_not_a_condition_leaf() {
        let mir = mir::parse_text(REGEX_EXTRACT_ASSERT_EQ_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        // Not a candidate at entry (bb0 is a Call) ...
        assert_eq!(decompose_function(f), None);
        // ... and the assert_eq! block itself must not decompose:
        // `_8 = copy (*_19)` is compare-operand prep, not the
        // function's value, and the switch guards a panic.
        assert_eq!(decompose_from_block(f, 3), None);
        for b in &f.blocks {
            assert_eq!(
                decompose_from_block(f, b.id),
                None,
                "bb{} of extract decomposed",
                b.id
            );
        }
    }

    #[test]
    fn assert_with_message_guard_is_not_a_decision() {
        let mir = mir::parse_text(ASSERT_MSG_GUARD_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        assert_eq!(decompose_function(f), None);
    }

    /// `cargo floyd test` MIR (rustc nightly 1.101, coverage flags,
    /// comment-only lines stripped) for `fixed` 1.x
    /// `src/int256.rs`:
    ///
    /// ```text
    /// pub const fn shl_i256_max_128(a: I256, sh: u32) -> I256 {
    ///     if sh == 0 {
    ///         a
    ///     } else if sh == 128 {
    ///         I256 { lo: a.hi as u128, hi: a.hi >> 127 }
    ///     } else {
    ///         I256 {
    ///             lo: (a.lo >> sh) | (a.hi << (128 - sh)) as u128,
    ///             hi: a.hi >> sh,
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// Both renderings rustc prints for a `const fn`: the runtime
    /// body, then the `// MIR FOR CTFE` body. The `sh == 0` arm is
    /// `_0 = copy _1` with `_1: int256::I256`.
    const SHL_I256_MAX_128_MIR: &str = r#"
fn shl_i256_max_128(_1: int256::I256, _2: u32) -> int256::I256 {
    debug a => _1;                       // in scope 0 at src/int256.rs:211:31: 211:32
    debug sh => _2;                      // in scope 0 at src/int256.rs:211:40: 211:42
    let mut _0: int256::I256;            // return place in scope 0 at src/int256.rs:211:52: 211:56
    let mut _3: bool;                    // in scope 0 at src/int256.rs:212:8: 212:15
    let mut _4: bool;                    // in scope 0 at src/int256.rs:214:15: 214:24
    let mut _5: u128;                    // in scope 0 at src/int256.rs:216:17: 216:29
    let mut _6: i128;                    // in scope 0 at src/int256.rs:216:17: 216:21
    let mut _7: i128;                    // in scope 0 at src/int256.rs:217:17: 217:28
    let mut _8: i128;                    // in scope 0 at src/int256.rs:217:17: 217:21
    let mut _9: u32;                     // in scope 0 at src/int256.rs:217:17: 217:28
    let mut _10: bool;                   // in scope 0 at src/int256.rs:217:17: 217:28
    let mut _11: u128;                   // in scope 0 at src/int256.rs:221:17: 221:60
    let mut _12: u128;                   // in scope 0 at src/int256.rs:221:17: 221:29
    let mut _13: u128;                   // in scope 0 at src/int256.rs:221:18: 221:22
    let mut _14: bool;                   // in scope 0 at src/int256.rs:221:17: 221:29
    let mut _15: u128;                   // in scope 0 at src/int256.rs:221:32: 221:60
    let mut _16: i128;                   // in scope 0 at src/int256.rs:221:32: 221:52
    let mut _17: i128;                   // in scope 0 at src/int256.rs:221:33: 221:37
    let mut _18: u32;                    // in scope 0 at src/int256.rs:221:41: 221:51
    let mut _19: (u32, bool);            // in scope 0 at src/int256.rs:221:41: 221:51
    let mut _20: bool;                   // in scope 0 at src/int256.rs:221:32: 221:52
    let mut _21: i128;                   // in scope 0 at src/int256.rs:222:17: 222:27
    let mut _22: i128;                   // in scope 0 at src/int256.rs:222:17: 222:21
    let mut _23: bool;                   // in scope 0 at src/int256.rs:222:17: 222:27

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/int256.rs:212:8: 212:15 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => src/int256.rs:214:15: 214:24 (#0)

    coverage Code { bcb: bcb0 } => src/int256.rs:211:1: 211:56 (#0);
    coverage Code { bcb: bcb0 } => src/int256.rs:212:8: 212:15 (#0);
    coverage Code { bcb: bcb1 } => src/int256.rs:213:9: 213:10 (#0);
    coverage Code { bcb: bcb3 } => src/int256.rs:214:15: 214:24 (#0);
    coverage Code { bcb: bcb4 } => src/int256.rs:216:17: 216:29 (#0);
    coverage Code { bcb: bcb4 } => src/int256.rs:215:9: 218:10 (#0);
    coverage Code { bcb: bcb6 } => src/int256.rs:221:17: 221:29 (#0);
    coverage Code { bcb: bcb6 } => src/int256.rs:221:33: 221:37 (#0);
    coverage Code { bcb: bcb6 } => src/int256.rs:221:17: 221:60 (#0);
    coverage Code { bcb: bcb6 } => src/int256.rs:220:9: 223:10 (#0);
    coverage Code { bcb: bcb2 } => src/int256.rs:225:1: 225:2 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => src/int256.rs:212:8: 212:15 (#0);
    coverage Branch { true_bcb: bcb4, false_bcb: bcb6 } => src/int256.rs:214:15: 214:24 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/int256.rs:212:8: 212:15
        _3 = Eq(copy _2, const 0_u32);   // scope 0 at src/int256.rs:212:8: 212:15
        switchInt(move _3) -> [0: bb2, otherwise: bb1]; // scope 0 at src/int256.rs:212:8: 212:15
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/int256.rs:212:5: 224:6
        _0 = copy _1;                    // scope 0 at src/int256.rs:213:9: 213:10
        goto -> bb11;                    // scope 0 at src/int256.rs:212:5: 224:6
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at src/int256.rs:214:15: 214:24
        _4 = Eq(copy _2, const 128_u32); // scope 0 at src/int256.rs:214:15: 214:24
        switchInt(move _4) -> [0: bb4, otherwise: bb3]; // scope 0 at src/int256.rs:214:15: 214:24
    }

    bb3: {
        Coverage::VirtualCounter(bcb4);  // scope 0 at src/int256.rs:217:17: 217:28
        _6 = copy (_1.1: i128);          // scope 0 at src/int256.rs:216:17: 216:21
        _5 = move _6 as u128 (IntToInt); // scope 0 at src/int256.rs:216:17: 216:29
        _8 = copy (_1.1: i128);          // scope 0 at src/int256.rs:217:17: 217:21
        _9 = const 127_i32 as u32 (IntToInt); // scope 0 at src/int256.rs:217:17: 217:28
        _10 = Lt(move _9, const 128_u32); // scope 0 at src/int256.rs:217:17: 217:28
        assert(move _10, "attempt to shift right by `{}`, which would overflow", const 127_i32) -> [success: bb5, unwind continue]; // scope 0 at src/int256.rs:217:17: 217:28
    }

    bb4: {
        Coverage::VirtualCounter(bcb6);  // scope 0 at src/int256.rs:221:17: 221:29
        _13 = copy (_1.0: u128);         // scope 0 at src/int256.rs:221:18: 221:22
        _14 = Lt(copy _2, const 128_u32); // scope 0 at src/int256.rs:221:17: 221:29
        assert(move _14, "attempt to shift right by `{}`, which would overflow", copy _2) -> [success: bb6, unwind continue]; // scope 0 at src/int256.rs:221:17: 221:29
    }

    bb5: {
        _7 = Shr(move _8, const 127_i32); // scope 0 at src/int256.rs:217:17: 217:28
        _0 = int256::I256 { lo: move _5, hi: move _7 }; // scope 0 at src/int256.rs:215:9: 218:10
        goto -> bb10;                    // scope 0 at src/int256.rs:214:12: 224:6
    }

    bb6: {
        _12 = Shr(move _13, copy _2);    // scope 0 at src/int256.rs:221:17: 221:29
        _17 = copy (_1.1: i128);         // scope 0 at src/int256.rs:221:33: 221:37
        _19 = SubWithOverflow(const 128_u32, copy _2); // scope 0 at src/int256.rs:221:41: 221:51
        assert(!move (_19.1: bool), "attempt to compute `{} - {}`, which would overflow", const 128_u32, copy _2) -> [success: bb7, unwind continue]; // scope 0 at src/int256.rs:221:41: 221:51
    }

    bb7: {
        _18 = move (_19.0: u32);         // scope 0 at src/int256.rs:221:41: 221:51
        _20 = Lt(copy _18, const 128_u32); // scope 0 at src/int256.rs:221:32: 221:52
        assert(move _20, "attempt to shift left by `{}`, which would overflow", copy _18) -> [success: bb8, unwind continue]; // scope 0 at src/int256.rs:221:32: 221:52
    }

    bb8: {
        _16 = Shl(move _17, move _18);   // scope 0 at src/int256.rs:221:32: 221:52
        _15 = move _16 as u128 (IntToInt); // scope 0 at src/int256.rs:221:32: 221:60
        _11 = BitOr(move _12, move _15); // scope 0 at src/int256.rs:221:17: 221:60
        _22 = copy (_1.1: i128);         // scope 0 at src/int256.rs:222:17: 222:21
        _23 = Lt(copy _2, const 128_u32); // scope 0 at src/int256.rs:222:17: 222:27
        assert(move _23, "attempt to shift right by `{}`, which would overflow", copy _2) -> [success: bb9, unwind continue]; // scope 0 at src/int256.rs:222:17: 222:27
    }

    bb9: {
        _21 = Shr(move _22, copy _2);    // scope 0 at src/int256.rs:222:17: 222:27
        _0 = int256::I256 { lo: move _11, hi: move _21 }; // scope 0 at src/int256.rs:220:9: 223:10
        goto -> bb10;                    // scope 0 at src/int256.rs:214:12: 224:6
    }

    bb10: {
        Coverage::VirtualCounter(bcb5);  // scope 0 at src/int256.rs:212:5: 224:6
        goto -> bb11;                    // scope 0 at src/int256.rs:212:5: 224:6
    }

    bb11: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at src/int256.rs:225:2: 225:2
        return;                          // scope 0 at src/int256.rs:225:2: 225:2
    }
}
fn shl_i256_max_128(_1: int256::I256, _2: u32) -> int256::I256 {
    debug a => _1;                       // in scope 0 at src/int256.rs:211:31: 211:32
    debug sh => _2;                      // in scope 0 at src/int256.rs:211:40: 211:42
    let mut _0: int256::I256;            // return place in scope 0 at src/int256.rs:211:52: 211:56
    let mut _3: bool;                    // in scope 0 at src/int256.rs:212:8: 212:15
    let mut _4: u32;                     // in scope 0 at src/int256.rs:212:8: 212:10
    let mut _5: bool;                    // in scope 0 at src/int256.rs:214:15: 214:24
    let mut _6: u32;                     // in scope 0 at src/int256.rs:214:15: 214:17
    let mut _7: u128;                    // in scope 0 at src/int256.rs:216:17: 216:29
    let mut _8: i128;                    // in scope 0 at src/int256.rs:216:17: 216:21
    let mut _9: i128;                    // in scope 0 at src/int256.rs:217:17: 217:28
    let mut _10: i128;                   // in scope 0 at src/int256.rs:217:17: 217:21
    let mut _11: u32;                    // in scope 0 at src/int256.rs:217:17: 217:28
    let mut _12: bool;                   // in scope 0 at src/int256.rs:217:17: 217:28
    let mut _13: u128;                   // in scope 0 at src/int256.rs:221:17: 221:60
    let mut _14: u128;                   // in scope 0 at src/int256.rs:221:17: 221:29
    let mut _15: u128;                   // in scope 0 at src/int256.rs:221:18: 221:22
    let mut _16: u32;                    // in scope 0 at src/int256.rs:221:26: 221:28
    let mut _17: bool;                   // in scope 0 at src/int256.rs:221:17: 221:29
    let mut _18: u128;                   // in scope 0 at src/int256.rs:221:32: 221:60
    let mut _19: i128;                   // in scope 0 at src/int256.rs:221:32: 221:52
    let mut _20: i128;                   // in scope 0 at src/int256.rs:221:33: 221:37
    let mut _21: u32;                    // in scope 0 at src/int256.rs:221:41: 221:51
    let mut _22: u32;                    // in scope 0 at src/int256.rs:221:48: 221:50
    let mut _23: (u32, bool);            // in scope 0 at src/int256.rs:221:41: 221:51
    let mut _24: bool;                   // in scope 0 at src/int256.rs:221:32: 221:52
    let mut _25: i128;                   // in scope 0 at src/int256.rs:222:17: 222:27
    let mut _26: i128;                   // in scope 0 at src/int256.rs:222:17: 222:21
    let mut _27: u32;                    // in scope 0 at src/int256.rs:222:25: 222:27
    let mut _28: bool;                   // in scope 0 at src/int256.rs:222:17: 222:27

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/int256.rs:212:8: 212:15 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => src/int256.rs:214:15: 214:24 (#0)

    coverage Code { bcb: bcb0 } => src/int256.rs:211:1: 211:56 (#0);
    coverage Code { bcb: bcb0 } => src/int256.rs:212:8: 212:15 (#0);
    coverage Code { bcb: bcb1 } => src/int256.rs:213:9: 213:10 (#0);
    coverage Code { bcb: bcb3 } => src/int256.rs:214:15: 214:24 (#0);
    coverage Code { bcb: bcb4 } => src/int256.rs:216:17: 216:29 (#0);
    coverage Code { bcb: bcb4 } => src/int256.rs:215:9: 218:10 (#0);
    coverage Code { bcb: bcb6 } => src/int256.rs:221:17: 221:29 (#0);
    coverage Code { bcb: bcb6 } => src/int256.rs:221:33: 221:37 (#0);
    coverage Code { bcb: bcb6 } => src/int256.rs:221:17: 221:60 (#0);
    coverage Code { bcb: bcb6 } => src/int256.rs:220:9: 223:10 (#0);
    coverage Code { bcb: bcb2 } => src/int256.rs:225:1: 225:2 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => src/int256.rs:212:8: 212:15 (#0);
    coverage Branch { true_bcb: bcb4, false_bcb: bcb6 } => src/int256.rs:214:15: 214:24 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/int256.rs:212:8: 212:15
        StorageLive(_3);                 // scope 0 at src/int256.rs:212:8: 212:15
        StorageLive(_4);                 // scope 0 at src/int256.rs:212:8: 212:10
        _4 = copy _2;                    // scope 0 at src/int256.rs:212:8: 212:10
        _3 = Eq(move _4, const 0_u32);   // scope 0 at src/int256.rs:212:8: 212:15
        switchInt(move _3) -> [0: bb2, otherwise: bb1]; // scope 0 at src/int256.rs:212:8: 212:15
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/int256.rs:212:5: 224:6
        StorageDead(_4);                 // scope 0 at src/int256.rs:212:14: 212:15
        _0 = copy _1;                    // scope 0 at src/int256.rs:213:9: 213:10
        goto -> bb11;                    // scope 0 at src/int256.rs:212:5: 224:6
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at src/int256.rs:214:15: 214:24
        StorageDead(_4);                 // scope 0 at src/int256.rs:212:14: 212:15
        StorageLive(_5);                 // scope 0 at src/int256.rs:214:15: 214:24
        StorageLive(_6);                 // scope 0 at src/int256.rs:214:15: 214:17
        _6 = copy _2;                    // scope 0 at src/int256.rs:214:15: 214:17
        _5 = Eq(move _6, const 128_u32); // scope 0 at src/int256.rs:214:15: 214:24
        switchInt(move _5) -> [0: bb4, otherwise: bb3]; // scope 0 at src/int256.rs:214:15: 214:24
    }

    bb3: {
        Coverage::VirtualCounter(bcb4);  // scope 0 at src/int256.rs:217:17: 217:28
        StorageDead(_6);                 // scope 0 at src/int256.rs:214:23: 214:24
        StorageLive(_7);                 // scope 0 at src/int256.rs:216:17: 216:29
        StorageLive(_8);                 // scope 0 at src/int256.rs:216:17: 216:21
        _8 = copy (_1.1: i128);          // scope 0 at src/int256.rs:216:17: 216:21
        _7 = move _8 as u128 (IntToInt); // scope 0 at src/int256.rs:216:17: 216:29
        StorageDead(_8);                 // scope 0 at src/int256.rs:216:28: 216:29
        StorageLive(_9);                 // scope 0 at src/int256.rs:217:17: 217:28
        StorageLive(_10);                // scope 0 at src/int256.rs:217:17: 217:21
        _10 = copy (_1.1: i128);         // scope 0 at src/int256.rs:217:17: 217:21
        _11 = const 127_i32 as u32 (IntToInt); // scope 0 at src/int256.rs:217:17: 217:28
        _12 = Lt(move _11, const 128_u32); // scope 0 at src/int256.rs:217:17: 217:28
        assert(move _12, "attempt to shift right by `{}`, which would overflow", const 127_i32) -> [success: bb5, unwind continue]; // scope 0 at src/int256.rs:217:17: 217:28
    }

    bb4: {
        Coverage::VirtualCounter(bcb6);  // scope 0 at src/int256.rs:221:17: 221:29
        StorageDead(_6);                 // scope 0 at src/int256.rs:214:23: 214:24
        StorageLive(_13);                // scope 0 at src/int256.rs:221:17: 221:60
        StorageLive(_14);                // scope 0 at src/int256.rs:221:17: 221:29
        StorageLive(_15);                // scope 0 at src/int256.rs:221:18: 221:22
        _15 = copy (_1.0: u128);         // scope 0 at src/int256.rs:221:18: 221:22
        StorageLive(_16);                // scope 0 at src/int256.rs:221:26: 221:28
        _16 = copy _2;                   // scope 0 at src/int256.rs:221:26: 221:28
        _17 = Lt(copy _16, const 128_u32); // scope 0 at src/int256.rs:221:17: 221:29
        assert(move _17, "attempt to shift right by `{}`, which would overflow", copy _16) -> [success: bb6, unwind continue]; // scope 0 at src/int256.rs:221:17: 221:29
    }

    bb5: {
        _9 = Shr(move _10, const 127_i32); // scope 0 at src/int256.rs:217:17: 217:28
        StorageDead(_10);                // scope 0 at src/int256.rs:217:27: 217:28
        _0 = int256::I256 { lo: move _7, hi: move _9 }; // scope 0 at src/int256.rs:215:9: 218:10
        StorageDead(_9);                 // scope 0 at src/int256.rs:218:9: 218:10
        StorageDead(_7);                 // scope 0 at src/int256.rs:218:9: 218:10
        goto -> bb10;                    // scope 0 at src/int256.rs:214:12: 224:6
    }

    bb6: {
        _14 = Shr(move _15, move _16);   // scope 0 at src/int256.rs:221:17: 221:29
        StorageDead(_16);                // scope 0 at src/int256.rs:221:28: 221:29
        StorageDead(_15);                // scope 0 at src/int256.rs:221:28: 221:29
        StorageLive(_18);                // scope 0 at src/int256.rs:221:32: 221:60
        StorageLive(_19);                // scope 0 at src/int256.rs:221:32: 221:52
        StorageLive(_20);                // scope 0 at src/int256.rs:221:33: 221:37
        _20 = copy (_1.1: i128);         // scope 0 at src/int256.rs:221:33: 221:37
        StorageLive(_21);                // scope 0 at src/int256.rs:221:41: 221:51
        StorageLive(_22);                // scope 0 at src/int256.rs:221:48: 221:50
        _22 = copy _2;                   // scope 0 at src/int256.rs:221:48: 221:50
        _23 = SubWithOverflow(const 128_u32, copy _22); // scope 0 at src/int256.rs:221:41: 221:51
        assert(!move (_23.1: bool), "attempt to compute `{} - {}`, which would overflow", const 128_u32, move _22) -> [success: bb7, unwind continue]; // scope 0 at src/int256.rs:221:41: 221:51
    }

    bb7: {
        _21 = move (_23.0: u32);         // scope 0 at src/int256.rs:221:41: 221:51
        StorageDead(_22);                // scope 0 at src/int256.rs:221:50: 221:51
        _24 = Lt(copy _21, const 128_u32); // scope 0 at src/int256.rs:221:32: 221:52
        assert(move _24, "attempt to shift left by `{}`, which would overflow", copy _21) -> [success: bb8, unwind continue]; // scope 0 at src/int256.rs:221:32: 221:52
    }

    bb8: {
        _19 = Shl(move _20, move _21);   // scope 0 at src/int256.rs:221:32: 221:52
        StorageDead(_21);                // scope 0 at src/int256.rs:221:51: 221:52
        StorageDead(_20);                // scope 0 at src/int256.rs:221:51: 221:52
        _18 = move _19 as u128 (IntToInt); // scope 0 at src/int256.rs:221:32: 221:60
        StorageDead(_19);                // scope 0 at src/int256.rs:221:59: 221:60
        _13 = BitOr(move _14, move _18); // scope 0 at src/int256.rs:221:17: 221:60
        StorageDead(_18);                // scope 0 at src/int256.rs:221:59: 221:60
        StorageDead(_14);                // scope 0 at src/int256.rs:221:59: 221:60
        StorageLive(_25);                // scope 0 at src/int256.rs:222:17: 222:27
        StorageLive(_26);                // scope 0 at src/int256.rs:222:17: 222:21
        _26 = copy (_1.1: i128);         // scope 0 at src/int256.rs:222:17: 222:21
        StorageLive(_27);                // scope 0 at src/int256.rs:222:25: 222:27
        _27 = copy _2;                   // scope 0 at src/int256.rs:222:25: 222:27
        _28 = Lt(copy _27, const 128_u32); // scope 0 at src/int256.rs:222:17: 222:27
        assert(move _28, "attempt to shift right by `{}`, which would overflow", copy _27) -> [success: bb9, unwind continue]; // scope 0 at src/int256.rs:222:17: 222:27
    }

    bb9: {
        _25 = Shr(move _26, move _27);   // scope 0 at src/int256.rs:222:17: 222:27
        StorageDead(_27);                // scope 0 at src/int256.rs:222:26: 222:27
        StorageDead(_26);                // scope 0 at src/int256.rs:222:26: 222:27
        _0 = int256::I256 { lo: move _13, hi: move _25 }; // scope 0 at src/int256.rs:220:9: 223:10
        StorageDead(_25);                // scope 0 at src/int256.rs:223:9: 223:10
        StorageDead(_13);                // scope 0 at src/int256.rs:223:9: 223:10
        goto -> bb10;                    // scope 0 at src/int256.rs:214:12: 224:6
    }

    bb10: {
        Coverage::VirtualCounter(bcb5);  // scope 0 at src/int256.rs:212:5: 224:6
        StorageDead(_5);                 // scope 0 at src/int256.rs:224:5: 224:6
        goto -> bb11;                    // scope 0 at src/int256.rs:212:5: 224:6
    }

    bb11: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at src/int256.rs:225:2: 225:2
        StorageDead(_3);                 // scope 0 at src/int256.rs:224:5: 224:6
        return;                          // scope 0 at src/int256.rs:225:2: 225:2
    }
}
"#;

    #[test]
    fn non_bool_copy_leaf_declines_shl_i256_max_128() {
        let mir = mir::parse_text(SHL_I256_MAX_128_MIR).expect("MIR parses");
        assert_eq!(mir.functions.len(), 2, "runtime and CTFE bodies");
        for f in &mir.functions {
            assert_eq!(local_type(f, 1), Some("int256::I256"));
            assert_eq!(local_type(f, 0), Some("int256::I256"));
            // Not the spurious 3-condition `a`, `sh == 0`, `sh == 128`.
            assert_eq!(decompose_function(f), None);
            assert_eq!(try_decompose_function(f), Err(DeclineReason::NonBoolLeaf));
        }
    }

    /// Same toolchain and flags as [`SHL_I256_MAX_128_MIR`] (plain
    /// `rustc --crate-type=lib`, comment-only lines stripped) for
    ///
    /// ```text
    /// #[inline(never)]
    /// pub fn helper(x: u32) -> u32 { x.rotate_left(3) }
    /// #[inline(never)]
    /// pub fn is_even(x: u32) -> bool { x % 2 == 0 }
    /// pub fn sel(c: bool, a: bool) -> bool { if c { a } else { false } }
    /// pub fn both(a: &bool, b: bool) -> bool { b && *a }
    /// pub fn pick_ref(a: &u8, b: &u8, c: bool) -> u8 { if c { *a } else { *b } }
    /// pub fn call_u32(c: bool, x: u32) -> u32 { let v = helper(x); if c { v } else { x } }
    /// pub fn call_bool(c: bool, x: u32) -> bool { let v = is_even(x); if c { v } else { false } }
    /// pub fn not_u32(c: bool, x: u32) -> u32 { if c { !x } else { x } }
    /// ```
    const LEAF_TYPES_MIR: &str = r#"
fn sel(_1: bool, _2: bool) -> bool {
    debug c => _1;                       // in scope 0 at leaf.rs:5:12: 5:13
    debug a => _2;                       // in scope 0 at leaf.rs:5:21: 5:22
    let mut _0: bool;                    // return place in scope 0 at leaf.rs:5:33: 5:37

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => leaf.rs:5:43: 5:44 (#0)

    coverage Code { bcb: bcb0 } => leaf.rs:5:1: 5:37 (#0);
    coverage Code { bcb: bcb0 } => leaf.rs:5:43: 5:44 (#0);
    coverage Code { bcb: bcb1 } => leaf.rs:5:47: 5:48 (#0);
    coverage Code { bcb: bcb3 } => leaf.rs:5:58: 5:63 (#0);
    coverage Code { bcb: bcb2 } => leaf.rs:5:66: 5:67 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => leaf.rs:5:43: 5:44 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at leaf.rs:5:43: 5:44
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at leaf.rs:5:43: 5:44
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at leaf.rs:5:40: 5:65
        _0 = copy _2;                    // scope 0 at leaf.rs:5:47: 5:48
        goto -> bb3;                     // scope 0 at leaf.rs:5:40: 5:65
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at leaf.rs:5:40: 5:65
        _0 = const false;                // scope 0 at leaf.rs:5:58: 5:63
        goto -> bb3;                     // scope 0 at leaf.rs:5:40: 5:65
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at leaf.rs:5:67: 5:67
        return;                          // scope 0 at leaf.rs:5:67: 5:67
    }
}

fn both(_1: &bool, _2: bool) -> bool {
    debug a => _1;                       // in scope 0 at leaf.rs:6:13: 6:14
    debug b => _2;                       // in scope 0 at leaf.rs:6:23: 6:24
    let mut _0: bool;                    // return place in scope 0 at leaf.rs:6:35: 6:39

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => leaf.rs:6:42: 6:43 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => leaf.rs:6:47: 6:49 (#0)

    coverage Code { bcb: bcb0 } => leaf.rs:6:1: 6:39 (#0);
    coverage Code { bcb: bcb0 } => leaf.rs:6:42: 6:43 (#0);
    coverage Code { bcb: bcb1 } => leaf.rs:6:47: 6:49 (#0);
    coverage Code { bcb: bcb3 } => leaf.rs:6:50: 6:51 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb5 } => leaf.rs:6:42: 6:43 (#0);
    coverage Branch { true_bcb: bcb2, false_bcb: bcb4 } => leaf.rs:6:47: 6:49 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at leaf.rs:6:42: 6:43
        switchInt(copy _2) -> [0: bb2, otherwise: bb1]; // scope 0 at leaf.rs:6:42: 6:43
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at leaf.rs:6:47: 6:49
        _0 = copy (*_1);                 // scope 0 at leaf.rs:6:47: 6:49
        switchInt(copy _0) -> [0: bb4, otherwise: bb3]; // scope 0 at leaf.rs:6:47: 6:49
    }

    bb2: {
        Coverage::VirtualCounter(bcb5);  // scope 0 at leaf.rs:6:42: 6:49
        _0 = const false;                // scope 0 at leaf.rs:6:42: 6:49
        goto -> bb5;                     // scope 0 at leaf.rs:6:42: 6:49
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at leaf.rs:6:47: 6:49
        goto -> bb5;                     // scope 0 at leaf.rs:6:47: 6:49
    }

    bb4: {
        Coverage::VirtualCounter(bcb4);  // scope 0 at leaf.rs:6:47: 6:49
        goto -> bb5;                     // scope 0 at leaf.rs:6:47: 6:49
    }

    bb5: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at leaf.rs:6:51: 6:51
        return;                          // scope 0 at leaf.rs:6:51: 6:51
    }
}

fn pick_ref(_1: &u8, _2: &u8, _3: bool) -> u8 {
    debug a => _1;                       // in scope 0 at leaf.rs:7:17: 7:18
    debug b => _2;                       // in scope 0 at leaf.rs:7:25: 7:26
    debug c => _3;                       // in scope 0 at leaf.rs:7:33: 7:34
    let mut _0: u8;                      // return place in scope 0 at leaf.rs:7:45: 7:47

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => leaf.rs:7:53: 7:54 (#0)

    coverage Code { bcb: bcb0 } => leaf.rs:7:1: 7:47 (#0);
    coverage Code { bcb: bcb0 } => leaf.rs:7:53: 7:54 (#0);
    coverage Code { bcb: bcb1 } => leaf.rs:7:57: 7:59 (#0);
    coverage Code { bcb: bcb3 } => leaf.rs:7:69: 7:71 (#0);
    coverage Code { bcb: bcb2 } => leaf.rs:7:74: 7:75 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => leaf.rs:7:53: 7:54 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at leaf.rs:7:53: 7:54
        switchInt(copy _3) -> [0: bb2, otherwise: bb1]; // scope 0 at leaf.rs:7:53: 7:54
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at leaf.rs:7:50: 7:73
        _0 = copy (*_1);                 // scope 0 at leaf.rs:7:57: 7:59
        goto -> bb3;                     // scope 0 at leaf.rs:7:50: 7:73
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at leaf.rs:7:50: 7:73
        _0 = copy (*_2);                 // scope 0 at leaf.rs:7:69: 7:71
        goto -> bb3;                     // scope 0 at leaf.rs:7:50: 7:73
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at leaf.rs:7:75: 7:75
        return;                          // scope 0 at leaf.rs:7:75: 7:75
    }
}

fn call_u32(_1: bool, _2: u32) -> u32 {
    debug c => _1;                       // in scope 0 at leaf.rs:8:17: 8:18
    debug x => _2;                       // in scope 0 at leaf.rs:8:26: 8:27
    let mut _0: u32;                     // return place in scope 0 at leaf.rs:8:37: 8:40
    let _3: u32;                         // in scope 0 at leaf.rs:8:47: 8:48
    scope 1 {
        debug v => _3;                   // in scope 1 at leaf.rs:8:47: 8:48
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => leaf.rs:8:65: 8:66 (#0)

    coverage Code { bcb: bcb0 } => leaf.rs:8:1: 8:40 (#0);
    coverage Code { bcb: bcb0 } => leaf.rs:8:51: 8:60 (#0);
    coverage Code { bcb: bcb0 } => leaf.rs:8:65: 8:66 (#0);
    coverage Code { bcb: bcb1 } => leaf.rs:8:69: 8:70 (#0);
    coverage Code { bcb: bcb3 } => leaf.rs:8:80: 8:81 (#0);
    coverage Code { bcb: bcb2 } => leaf.rs:8:84: 8:85 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => leaf.rs:8:65: 8:66 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at leaf.rs:8:51: 8:60
        _3 = helper(copy _2) -> [return: bb1, unwind continue]; // scope 0 at leaf.rs:8:51: 8:60
    }

    bb1: {
        switchInt(copy _1) -> [0: bb3, otherwise: bb2]; // scope 1 at leaf.rs:8:65: 8:66
    }

    bb2: {
        Coverage::VirtualCounter(bcb1);  // scope 1 at leaf.rs:8:62: 8:83
        _0 = copy _3;                    // scope 1 at leaf.rs:8:69: 8:70
        goto -> bb4;                     // scope 1 at leaf.rs:8:62: 8:83
    }

    bb3: {
        Coverage::VirtualCounter(bcb3);  // scope 1 at leaf.rs:8:62: 8:83
        _0 = copy _2;                    // scope 1 at leaf.rs:8:80: 8:81
        goto -> bb4;                     // scope 1 at leaf.rs:8:62: 8:83
    }

    bb4: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at leaf.rs:8:85: 8:85
        return;                          // scope 0 at leaf.rs:8:85: 8:85
    }
}

fn call_bool(_1: bool, _2: u32) -> bool {
    debug c => _1;                       // in scope 0 at leaf.rs:9:18: 9:19
    debug x => _2;                       // in scope 0 at leaf.rs:9:27: 9:28
    let mut _0: bool;                    // return place in scope 0 at leaf.rs:9:38: 9:42
    let _3: bool;                        // in scope 0 at leaf.rs:9:49: 9:50
    scope 1 {
        debug v => _3;                   // in scope 1 at leaf.rs:9:49: 9:50
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => leaf.rs:9:68: 9:69 (#0)

    coverage Code { bcb: bcb0 } => leaf.rs:9:1: 9:42 (#0);
    coverage Code { bcb: bcb0 } => leaf.rs:9:53: 9:63 (#0);
    coverage Code { bcb: bcb0 } => leaf.rs:9:68: 9:69 (#0);
    coverage Code { bcb: bcb1 } => leaf.rs:9:72: 9:73 (#0);
    coverage Code { bcb: bcb3 } => leaf.rs:9:83: 9:88 (#0);
    coverage Code { bcb: bcb2 } => leaf.rs:9:91: 9:92 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => leaf.rs:9:68: 9:69 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at leaf.rs:9:53: 9:63
        _3 = is_even(copy _2) -> [return: bb1, unwind continue]; // scope 0 at leaf.rs:9:53: 9:63
    }

    bb1: {
        switchInt(copy _1) -> [0: bb3, otherwise: bb2]; // scope 1 at leaf.rs:9:68: 9:69
    }

    bb2: {
        Coverage::VirtualCounter(bcb1);  // scope 1 at leaf.rs:9:65: 9:90
        _0 = copy _3;                    // scope 1 at leaf.rs:9:72: 9:73
        goto -> bb4;                     // scope 1 at leaf.rs:9:65: 9:90
    }

    bb3: {
        Coverage::VirtualCounter(bcb3);  // scope 1 at leaf.rs:9:65: 9:90
        _0 = const false;                // scope 1 at leaf.rs:9:83: 9:88
        goto -> bb4;                     // scope 1 at leaf.rs:9:65: 9:90
    }

    bb4: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at leaf.rs:9:92: 9:92
        return;                          // scope 0 at leaf.rs:9:92: 9:92
    }
}

fn not_u32(_1: bool, _2: u32) -> u32 {
    debug c => _1;                       // in scope 0 at leaf.rs:10:16: 10:17
    debug x => _2;                       // in scope 0 at leaf.rs:10:25: 10:26
    let mut _0: u32;                     // return place in scope 0 at leaf.rs:10:36: 10:39

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => leaf.rs:10:45: 10:46 (#0)

    coverage Code { bcb: bcb0 } => leaf.rs:10:1: 10:39 (#0);
    coverage Code { bcb: bcb0 } => leaf.rs:10:45: 10:46 (#0);
    coverage Code { bcb: bcb1 } => leaf.rs:10:49: 10:51 (#0);
    coverage Code { bcb: bcb3 } => leaf.rs:10:61: 10:62 (#0);
    coverage Code { bcb: bcb2 } => leaf.rs:10:65: 10:66 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => leaf.rs:10:45: 10:46 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at leaf.rs:10:45: 10:46
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at leaf.rs:10:45: 10:46
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at leaf.rs:10:42: 10:64
        _0 = Not(copy _2);               // scope 0 at leaf.rs:10:49: 10:51
        goto -> bb3;                     // scope 0 at leaf.rs:10:42: 10:64
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at leaf.rs:10:42: 10:64
        _0 = copy _2;                    // scope 0 at leaf.rs:10:61: 10:62
        goto -> bb3;                     // scope 0 at leaf.rs:10:42: 10:64
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at leaf.rs:10:66: 10:66
        return;                          // scope 0 at leaf.rs:10:66: 10:66
    }
}
"#;

    fn leaf_fn<'a>(mir: &'a Mir, name: &str) -> &'a MirFunction {
        mir.functions
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("fixture has `{name}`"))
    }

    #[test]
    fn bool_copy_leaf_is_a_condition() {
        let mir = mir::parse_text(LEAF_TYPES_MIR).expect("MIR parses");
        let f = leaf_fn(&mir, "sel");
        assert_eq!(
            try_decompose_function(f),
            Ok(ite("c", cond("a"), const_(false)))
        );
    }

    #[test]
    fn deref_bool_copy_leaf_is_a_condition() {
        // `b && *a` with `a: &bool`: the leaf is `_0 = copy (*_1)`.
        let mir = mir::parse_text(LEAF_TYPES_MIR).expect("MIR parses");
        let f = leaf_fn(&mir, "both");
        assert!(matches!(
            f.blocks[1].statements[1],
            MirStatement::AssignCopy {
                dst: 0,
                src: 1,
                deref: true,
                ..
            }
        ));
        assert_eq!(
            try_decompose_function(f),
            Ok(ite("b", cond("a"), const_(false)))
        );
    }

    #[test]
    fn deref_non_bool_copy_leaf_is_not_a_condition() {
        // `if c { *a } else { *b }` with `a, b: &u8`. The `u8` arms
        // are not conditions; the ordinary handlers decline, and the
        // decision is `c` with the arm taken as its outcome.
        let mir = mir::parse_text(LEAF_TYPES_MIR).expect("MIR parses");
        let f = leaf_fn(&mir, "pick_ref");
        assert_eq!(build_decision_from_block(0, f), None);
        assert_eq!(
            try_decompose_candidate_lifted(f),
            Ok((ite("c", const_(true), const_(false)), Some(Lift::ValueArms)))
        );
    }

    #[test]
    fn call_result_leaf_is_typed_by_its_destination_local() {
        // `let v = helper(x)` (`-> u32`) then `if c { v } else { x }`.
        // bb0 is the call; the decision is rooted at bb1.
        let mir = mir::parse_text(LEAF_TYPES_MIR).expect("MIR parses");
        let f = leaf_fn(&mir, "call_u32");
        assert_eq!(local_type(f, 3), Some("u32"));
        assert_eq!(decompose_from_block(f, 2), None, "`v` arm is not a leaf");
        assert_eq!(build_decision_from_block(1, f), None);
        // The `u32` arms are lifted as values, not read as `v`.
        assert_eq!(
            try_decompose_from_block(f, 1),
            Ok(ite("c", const_(true), const_(false)))
        );

        // `let v = is_even(x)` (`-> bool`): the same shape decomposes.
        let f = leaf_fn(&mir, "call_bool");
        assert_eq!(local_type(f, 3), Some("bool"));
        assert_eq!(
            try_decompose_from_block(f, 1),
            Ok(ite("c", cond("v"), const_(false)))
        );
    }

    #[test]
    fn not_of_integer_leaf_is_not_a_condition() {
        // `if c { !x } else { x }` with `x: u32`: bitwise complement,
        // not a condition `!x`. The arms are lifted as values.
        let mir = mir::parse_text(LEAF_TYPES_MIR).expect("MIR parses");
        let f = leaf_fn(&mir, "not_u32");
        assert!(matches!(
            f.blocks[1].statements[1],
            MirStatement::AssignUnaryNot { dst: 0, src: 2, .. }
        ));
        assert_eq!(build_decision_from_block(0, f), None);
        assert_eq!(
            try_decompose_candidate_lifted(f),
            Ok((ite("c", const_(true), const_(false)), Some(Lift::ValueArms)))
        );
    }

    #[test]
    fn other_declines_report_unsupported_shape() {
        let mir = mir::parse_text(DEBUG_ASSERT_GUARD_MIR).expect("MIR parses");
        assert_eq!(
            try_decompose_function(&mir.functions[0]),
            Err(DeclineReason::UnsupportedShape)
        );
    }

    #[test]
    fn leaf_type_check_reads_declared_types_only() {
        assert_eq!(pointee_type("&bool"), Some("bool"));
        assert_eq!(pointee_type("&mut bool"), Some("bool"));
        assert_eq!(pointee_type("&'a bool"), Some("bool"));
        assert_eq!(pointee_type("&'a mut bool"), Some("bool"));
        assert_eq!(pointee_type("*const bool"), Some("bool"));
        assert_eq!(pointee_type("*mut bool"), Some("bool"));
        assert_eq!(pointee_type("&&bool"), Some("&bool"));
        assert_eq!(pointee_type("bool"), None);
        assert_eq!(pointee_type("Box<bool>"), None);

        // A local the function does not declare is never assumed bool.
        let mir = mir::parse_text(LEAF_TYPES_MIR).expect("MIR parses");
        let f = leaf_fn(&mir, "sel");
        assert!(reads_bool(f, 2, false));
        assert!(!reads_bool(f, 2, true));
        assert!(!reads_bool(f, 99, false));
    }

    #[test]
    fn decompose_from_block_with_unknown_id_returns_none() {
        // A block id that doesn't identify any block in the function
        // must return None rather than panicking or scanning a
        // bogus block.
        let mir = mir::parse_text(DECIDE_AND_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        let max_id = f.blocks.iter().map(|b| b.id).max().unwrap_or(0);
        assert!(decompose_from_block(f, max_id + 999).is_none());
    }

    /// BLAKE3 `src/io.rs`, `cargo floyd test` MIR (rustc nightly,
    /// coverage flags, comment-only lines stripped):
    ///
    /// ```text
    /// pub(crate) fn copy_wide(mut reader: impl io::Read, hasher: &mut crate::Hasher) -> io::Result<u64> {
    ///     let mut buffer = [0; 65536];
    ///     let mut total = 0;
    ///     loop {
    ///         match reader.read(&mut buffer) {
    ///             Ok(0) => return Ok(total),
    ///             Ok(n) => {
    ///                 hasher.update(&buffer[..n]);
    ///                 total += n as u64;
    ///             }
    ///             Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
    ///             Err(e) => return Err(e),
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// bb0 is `_25 = const false; .. goto -> bb1`: `_25` is the drop
    /// flag of the `Err(e)` binding, and bb1 is the loop head.
    const COPY_WIDE_MIR: &str = r#"
fn copy_wide(_1: impl io::Read, _2: &mut Hasher) -> Result<u64, std::io::Error> {
    debug reader => _1;                  // in scope 0 at src/io.rs:10:25: 10:35
    debug hasher => _2;                  // in scope 0 at src/io.rs:10:52: 10:58
    let mut _0: std::result::Result<u64, std::io::Error>; // return place in scope 0 at src/io.rs:10:83: 10:98
    let mut _3: [u8; 65536];             // in scope 0 at src/io.rs:11:9: 11:19
    let mut _5: std::result::Result<usize, std::io::Error>; // in scope 0 at src/io.rs:14:15: 14:39
    let mut _6: &mut impl io::Read;      // in scope 0 at src/io.rs:14:15: 14:21
    let mut _7: &mut [u8];               // in scope 0 at src/io.rs:14:27: 14:38
    let mut _8: &mut [u8; 65536];        // in scope 0 at src/io.rs:14:27: 14:38
    let mut _9: isize;                   // in scope 0 at src/io.rs:15:13: 15:18
    let mut _10: u64;                    // in scope 0 at src/io.rs:15:32: 15:37
    let _12: &mut Hasher;                // in scope 0 at src/io.rs:17:17: 17:44
    let _13: &[u8];                      // in scope 0 at src/io.rs:17:38: 17:43
    let mut _14: &[u8; 65536];           // in scope 0 at src/io.rs:17:32: 17:38
    let mut _15: std::ops::RangeTo<usize>; // in scope 0 at src/io.rs:17:39: 17:42
    let mut _16: u64;                    // in scope 0 at src/io.rs:18:26: 18:34
    let mut _17: (u64, bool);            // in scope 0 at src/io.rs:18:17: 18:34
    let mut _20: bool;                   // in scope 0 at src/io.rs:21:23: 21:61
    let mut _21: &std::io::ErrorKind;    // in scope 0 at src/io.rs:21:23: 21:31
    let _22: std::io::ErrorKind;         // in scope 0 at src/io.rs:21:23: 21:31
    let mut _23: &std::io::ErrorKind;    // in scope 0 at src/io.rs:21:35: 21:61
    let mut _25: bool;                   // in scope 0 at src/io.rs:23:9: 23:10
    let mut _26: isize;                  // in scope 0 at src/io.rs:23:9: 23:10
    scope 1 {
        debug buffer => _3;              // in scope 1 at src/io.rs:11:9: 11:19
        let mut _4: u64;                 // in scope 1 at src/io.rs:12:9: 12:18
        scope 2 {
            debug total => _4;           // in scope 2 at src/io.rs:12:9: 12:18
            let _11: usize;              // in scope 2 at src/io.rs:16:16: 16:17
            let _18: std::io::Error;     // in scope 2 at src/io.rs:21:17: 21:18
            let _19: &std::io::Error;    // in scope 2 at src/io.rs:21:17: 21:18
            let _24: std::io::Error;     // in scope 2 at src/io.rs:22:17: 22:18
            scope 3 {
                debug n => _11;          // in scope 3 at src/io.rs:16:16: 16:17
            }
            scope 4 {
                debug e => _18;          // in scope 4 at src/io.rs:21:17: 21:18
                debug e => _19;          // in scope 4 at src/io.rs:21:17: 21:18
            }
            scope 5 {
                debug e => _24;          // in scope 5 at src/io.rs:22:17: 22:18
            }
        }
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/io.rs:21:23: 21:61 (#0)

    coverage Code { bcb: bcb0 } => src/io.rs:10:1: 10:98 (#0);
    coverage Code { bcb: bcb0 } => src/io.rs:11:22: 11:32 (#0);
    coverage Code { bcb: bcb0 } => src/io.rs:12:21: 12:22 (#0);
    coverage Code { bcb: bcb1 } => src/io.rs:14:15: 14:39 (#0);
    coverage Code { bcb: bcb9 } => src/io.rs:15:22: 15:38 (#0);
    coverage Code { bcb: bcb8 } => src/io.rs:17:17: 17:23 (#0);
    coverage Code { bcb: bcb8 } => src/io.rs:17:17: 17:44 (#0);
    coverage Code { bcb: bcb8 } => src/io.rs:18:17: 18:34 (#0);
    coverage Code { bcb: bcb2 } => src/io.rs:21:23: 21:61 (#0);
    coverage Code { bcb: bcb3 } => src/io.rs:21:65: 21:73 (#0);
    coverage Code { bcb: bcb5 } => src/io.rs:22:23: 22:36 (#0);
    coverage Code { bcb: bcb6 } => src/io.rs:25:1: 25:2 (#0);
    coverage Branch { true_bcb: bcb3, false_bcb: bcb4 } => src/io.rs:21:23: 21:61 (#0);

    bb0: {
        _25 = const false;               // scope 2 at src/io.rs:13:5: 24:6
        Coverage::VirtualCounter(bcb0);  // scope 2 at src/io.rs:13:5: 24:6
        _3 = [const 0_u8; 65536];        // scope 0 at src/io.rs:11:22: 11:32
        _4 = const 0_u64;                // scope 1 at src/io.rs:12:21: 12:22
        goto -> bb1;                     // scope 2 at src/io.rs:13:5: 24:6
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 2 at src/io.rs:13:5: 24:6
        _6 = &mut _1;                    // scope 2 at src/io.rs:14:15: 14:21
        _8 = &mut _3;                    // scope 2 at src/io.rs:14:27: 14:38
        _7 = copy _8 as &mut [u8] (PointerCoercion(Unsize, Implicit)); // scope 2 at src/io.rs:14:27: 14:38
        _5 = <impl io::Read as std::io::Read>::read(move _6, move _7) -> [return: bb2, unwind: bb18]; // scope 2 at src/io.rs:14:15: 14:39
    }

    bb2: {
        _25 = const true;                // scope 2 at src/io.rs:14:38: 14:39
        _9 = discriminant(_5);           // scope 2 at src/io.rs:14:15: 14:39
        switchInt(move _9) -> [0: bb4, 1: bb7, otherwise: bb3]; // scope 2 at src/io.rs:14:9: 14:39
    }

    bb3: {
        unreachable;                     // scope 2 at src/io.rs:14:15: 14:39
    }

    bb4: {
        Coverage::VirtualCounter(bcb7);  // scope 2 at src/io.rs:14:9: 14:39
        switchInt(copy ((_5 as Ok).0: usize)) -> [0: bb6, otherwise: bb5]; // scope 2 at src/io.rs:14:9: 14:39
    }

    bb5: {
        Coverage::VirtualCounter(bcb8);  // scope 2 at src/io.rs:16:13: 16:18
        _11 = copy ((_5 as Ok).0: usize); // scope 2 at src/io.rs:16:16: 16:17
        _14 = &_3;                       // scope 3 at src/io.rs:17:32: 17:38
        _15 = RangeTo::<usize> { end: copy _11 }; // scope 3 at src/io.rs:17:39: 17:42
        _13 = <[u8; 65536] as Index<RangeTo<usize>>>::index(move _14, move _15) -> [return: bb8, unwind: bb22]; // scope 3 at src/io.rs:17:38: 17:43
    }

    bb6: {
        Coverage::VirtualCounter(bcb9);  // scope 2 at src/io.rs:15:13: 15:18
        _10 = copy _4;                   // scope 2 at src/io.rs:15:32: 15:37
        _0 = Result::<u64, std::io::Error>::Ok(move _10); // scope 2 at src/io.rs:15:29: 15:38
        goto -> bb16;                    // scope 2 at src/io.rs:15:22: 15:38
    }

    bb7: {
        Coverage::VirtualCounter(bcb2);  // scope 2 at src/io.rs:21:13: 21:19
        _19 = &((_5 as Err).0: std::io::Error); // scope 2 at src/io.rs:21:17: 21:18
        _22 = std::io::Error::kind(copy _19) -> [return: bb11, unwind: bb22]; // scope 2 at src/io.rs:21:23: 21:31
    }

    bb8: {
        _12 = Hasher::update(copy _2, copy _13) -> [return: bb9, unwind: bb22]; // scope 3 at src/io.rs:17:17: 17:44
    }

    bb9: {
        _16 = copy _11 as u64 (IntToInt); // scope 3 at src/io.rs:18:26: 18:34
        _17 = AddWithOverflow(copy _4, copy _16); // scope 3 at src/io.rs:18:17: 18:34
        assert(!move (_17.1: bool), "attempt to compute `{} + {}`, which would overflow", copy _4, move _16) -> [success: bb10, unwind: bb22]; // scope 3 at src/io.rs:18:17: 18:34
    }

    bb10: {
        _4 = move (_17.0: u64);          // scope 3 at src/io.rs:18:17: 18:34
        _25 = const false;               // scope 2 at src/io.rs:23:9: 23:10
        goto -> bb1;                     // scope 2 at src/io.rs:13:5: 24:6
    }

    bb11: {
        _21 = &_22;                      // scope 2 at src/io.rs:21:23: 21:31
        _23 = const io::copy_wide::<impl io::Read>::promoted[0]; // scope 2 at src/io.rs:21:35: 21:61
        _20 = <ErrorKind as PartialEq>::eq(move _21, move _23) -> [return: bb12, unwind: bb22]; // scope 2 at src/io.rs:21:23: 21:61
    }

    bb12: {
        switchInt(move _20) -> [0: bb14, otherwise: bb13]; // scope 2 at src/io.rs:21:23: 21:61
    }

    bb13: {
        Coverage::VirtualCounter(bcb3);  // scope 2 at src/io.rs:21:72: 21:73
        _25 = const false;               // scope 2 at src/io.rs:21:17: 21:18
        _18 = move ((_5 as Err).0: std::io::Error); // scope 2 at src/io.rs:21:17: 21:18
        drop(_18) -> [return: bb15, unwind: bb22]; // scope 2 at src/io.rs:21:72: 21:73
    }

    bb14: {
        Coverage::VirtualCounter(bcb4);  // scope 2 at src/io.rs:21:13: 21:19
        Coverage::VirtualCounter(bcb5);  // scope 5 at src/io.rs:22:35: 22:36
        _25 = const false;               // scope 2 at src/io.rs:22:17: 22:18
        _24 = move ((_5 as Err).0: std::io::Error); // scope 2 at src/io.rs:22:17: 22:18
        _0 = Result::<u64, std::io::Error>::Err(copy _24); // scope 5 at src/io.rs:22:30: 22:36
        goto -> bb16;                    // scope 2 at no-location
    }

    bb15: {
        _25 = const false;               // scope 2 at src/io.rs:23:9: 23:10
        goto -> bb1;                     // scope 2 at no-location
    }

    bb16: {
        Coverage::VirtualCounter(bcb6);  // scope 2 at src/io.rs:23:9: 23:10
        _25 = const false;               // scope 2 at src/io.rs:23:9: 23:10
        drop(_1) -> [return: bb17, unwind continue]; // scope 0 at src/io.rs:25:1: 25:2
    }

    bb17: {
        return;                          // scope 0 at src/io.rs:25:2: 25:2
    }

    bb18 (cleanup): {
        drop(_1) -> [return: bb19, unwind terminate(cleanup)]; // scope 0 at src/io.rs:25:1: 25:2
    }

    bb19 (cleanup): {
        resume;                          // scope 0 at src/io.rs:10:1: 25:2
    }

    bb20 (cleanup): {
        switchInt(copy _25) -> [0: bb18, otherwise: bb21]; // scope 2 at src/io.rs:23:9: 23:10
    }

    bb21 (cleanup): {
        drop(((_5 as Err).0: std::io::Error)) -> [return: bb18, unwind terminate(cleanup)]; // scope 2 at src/io.rs:23:9: 23:10
    }

    bb22 (cleanup): {
        _26 = discriminant(_5);          // scope 2 at src/io.rs:23:9: 23:10
        switchInt(move _26) -> [1: bb20, otherwise: bb18]; // scope 2 at src/io.rs:23:9: 23:10
    }
}
"#;

    /// hashbrown `src/raw.rs`, `RawIterRange::fold_impl`, same
    /// compilation settings. bb0 is `_31 = const false; _31 = const
    /// true; goto -> bb1`: two writes of a drop flag.
    const FOLD_IMPL_MIR: &str = r#"
fn raw::<impl at src/raw.rs:3556:1: 3556:24>::fold_impl(_1: RawIterRange<T>, _2: usize, _3: B, _4: F) -> B {
    debug self => _1;                    // in scope 0 at src/raw.rs:3706:31: 3706:39
    debug n => _2;                       // in scope 0 at src/raw.rs:3706:41: 3706:46
    debug acc => _3;                     // in scope 0 at src/raw.rs:3706:55: 3706:62
    debug f => _4;                       // in scope 0 at src/raw.rs:3706:67: 3706:72
    let mut _0: B;                       // return place in scope 0 at src/raw.rs:3706:80: 3706:81
    let mut _5: core::option::Option<usize>; // in scope 0 at src/raw.rs:3711:37: 3711:62
    let mut _6: &mut control::bitmask::BitMaskIter; // in scope 0 at src/raw.rs:3711:37: 3711:55
    let mut _7: isize;                   // in scope 0 at src/raw.rs:3711:23: 3711:34
    let mut _9: bool;                    // in scope 0 at src/raw.rs:3714:31: 3714:37
    let mut _10: usize;                  // in scope 0 at src/raw.rs:3714:31: 3714:32
    let mut _11: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:293:13: 293:38
    let mut _13: &raw::Bucket<T>;        // in scope 0 at src/raw.rs:3715:39: 3715:48
    let mut _14: B;                      // in scope 0 at src/raw.rs:3716:23: 3716:37
    let mut _15: &mut F;                 // in scope 0 at src/raw.rs:3716:23: 3716:24
    let mut _16: (B, raw::Bucket<T>);    // in scope 0 at src/raw.rs:3716:23: 3716:37
    let mut _17: B;                      // in scope 0 at src/raw.rs:3716:25: 3716:28
    let mut _18: (usize, bool);          // in scope 0 at src/raw.rs:3717:17: 3717:23
    let mut _19: bool;                   // in scope 0 at src/raw.rs:3720:16: 3720:22
    let mut _20: usize;                  // in scope 0 at src/raw.rs:3720:16: 3720:17
    let mut _21: control::bitmask::BitMaskIter; // in scope 0 at src/raw.rs:3753:38: 3755:33
    let mut _22: control::bitmask::BitMask; // in scope 0 at src/raw.rs:3753:38: 3754:34
    let mut _23: &control::group::sse2::Group; // in scope 0 at src/raw.rs:3753:38: 3753:80
    let _24: control::group::sse2::Group; // in scope 0 at src/raw.rs:3753:38: 3753:80
    let mut _25: *const control::tag::Tag; // in scope 0 at src/raw.rs:3753:58: 3753:79
    let mut _26: *const u8;              // in scope 0 at src/raw.rs:3753:58: 3753:72
    let mut _27: raw::Bucket<T>;         // in scope 0 at src/raw.rs:3756:29: 3756:59
    let mut _28: &raw::Bucket<T>;        // in scope 0 at src/raw.rs:3756:29: 3756:38
    let mut _29: *const u8;              // in scope 0 at src/raw.rs:3757:34: 3757:66
    let mut _30: *const u8;              // in scope 0 at src/raw.rs:3757:34: 3757:48
    let mut _31: bool;                   // in scope 0 at src/raw.rs:3760:5: 3760:6
    scope 1 {
        debug index => _8;               // in scope 1 at src/raw.rs:3711:28: 3711:33
        let _8: usize;                   // in scope 1 at src/raw.rs:3711:28: 3711:33
        let _12: raw::Bucket<T>;         // in scope 1 at src/raw.rs:3715:21: 3715:27
        scope 2 {
            debug bucket => _12;         // in scope 2 at src/raw.rs:3715:21: 3715:27
        }
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/raw.rs:3711:23: 3711:34 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:292:12: 292:42 (#158)
    coverage branch { true: BlockMarkerId(4), false: BlockMarkerId(5) } => /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:293:13: 293:38 (#159)
    coverage branch { true: BlockMarkerId(6), false: BlockMarkerId(7) } => src/raw.rs:3720:16: 3720:22 (#0)

    coverage Code { bcb: bcb0 } => src/raw.rs:3706:5: 3708:37 (#0);
    coverage Code { bcb: bcb2 } => src/raw.rs:3711:19: 3711:62 (#0);
    coverage Code { bcb: bcb6 } => src/raw.rs:3714:17: 3714:30 (#0);
    coverage Code { bcb: bcb7 } => src/raw.rs:3714:31: 3714:37 (#0);
    coverage Code { bcb: bcb9 } => src/raw.rs:3715:39: 3715:62 (#0);
    coverage Code { bcb: bcb9 } => src/raw.rs:3716:17: 3716:20 (#0);
    coverage Code { bcb: bcb9 } => src/raw.rs:3716:23: 3716:24 (#0);
    coverage Code { bcb: bcb9 } => src/raw.rs:3716:17: 3716:37 (#0);
    coverage Code { bcb: bcb9 } => src/raw.rs:3717:17: 3717:23 (#0);
    coverage Code { bcb: bcb3 } => src/raw.rs:3720:16: 3720:22 (#0);
    coverage Code { bcb: bcb4 } => src/raw.rs:3721:17: 3721:27 (#0);
    coverage Code { bcb: bcb5 } => src/raw.rs:3722:13: 3722:14 (#0);
    coverage Code { bcb: bcb5 } => src/raw.rs:3753:17: 3753:35 (#0);
    coverage Code { bcb: bcb5 } => src/raw.rs:3753:17: 3755:33 (#0);
    coverage Code { bcb: bcb5 } => src/raw.rs:3756:17: 3756:26 (#0);
    coverage Code { bcb: bcb5 } => src/raw.rs:3756:17: 3756:59 (#0);
    coverage Code { bcb: bcb5 } => src/raw.rs:3757:17: 3757:31 (#0);
    coverage Code { bcb: bcb5 } => src/raw.rs:3757:17: 3757:66 (#0);
    coverage Code { bcb: bcb4 } => src/raw.rs:3760:5: 3760:6 (#0);
    coverage Branch { true_bcb: bcb6, false_bcb: bcb3 } => src/raw.rs:3711:23: 3711:34 (#0);
    coverage Branch { true_bcb: bcb4, false_bcb: bcb5 } => src/raw.rs:3720:16: 3720:22 (#0);

    bb0: {
        _31 = const false;               // scope 0 at src/raw.rs:3710:9: 3759:10
        _31 = const true;                // scope 0 at src/raw.rs:3710:9: 3759:10
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/raw.rs:3710:9: 3759:10
        goto -> bb1;                     // scope 0 at src/raw.rs:3710:9: 3759:10
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/raw.rs:3710:9: 3759:10
        goto -> bb2;                     // scope 0 at src/raw.rs:3711:13: 3718:14
    }

    bb2: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at src/raw.rs:3711:13: 3718:14
        _6 = &mut (_1.0: control::bitmask::BitMaskIter); // scope 1 at src/raw.rs:3711:37: 3711:55
        _5 = <BitMaskIter as Iterator>::next(move _6) -> [return: bb3, unwind: bb20]; // scope 1 at src/raw.rs:3711:37: 3711:62
    }

    bb3: {
        _7 = discriminant(_5);           // scope 1 at src/raw.rs:3711:37: 3711:62
        switchInt(move _7) -> [1: bb5, 0: bb4, otherwise: bb24]; // scope 1 at src/raw.rs:3711:23: 3711:34
    }

    bb4: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at src/raw.rs:3720:16: 3720:22
        _20 = copy _2;                   // scope 0 at src/raw.rs:3720:16: 3720:17
        _19 = Eq(move _20, const 0_usize); // scope 0 at src/raw.rs:3720:16: 3720:22
        switchInt(move _19) -> [0: bb12, otherwise: bb11]; // scope 0 at src/raw.rs:3720:16: 3720:22
    }

    bb5: {
        Coverage::VirtualCounter(bcb6);  // scope 1 at src/raw.rs:3711:23: 3711:34
        _8 = copy ((_5 as Some).0: usize); // scope 1 at src/raw.rs:3711:28: 3711:33
        Coverage::VirtualCounter(bcb7);  // scope 1 at src/raw.rs:3714:31: 3714:37
        _10 = copy _2;                   // scope 1 at src/raw.rs:3714:31: 3714:32
        _9 = Ne(move _10, const 0_usize); // scope 1 at src/raw.rs:3714:31: 3714:37
        switchInt(move _9) -> [0: bb7, otherwise: bb6]; // scope 1 at src/raw.rs:3714:31: 3714:37
    }

    bb6: {
        Coverage::VirtualCounter(bcb8);  // scope 1 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:292:9: 294:10
        Coverage::VirtualCounter(bcb9);  // scope 1 at src/raw.rs:3715:39: 3715:62
        _13 = &(_1.1: raw::Bucket<T>);   // scope 1 at src/raw.rs:3715:39: 3715:48
        _12 = Bucket::<T>::next_n(move _13, copy _8) -> [return: bb8, unwind: bb20]; // scope 1 at src/raw.rs:3715:39: 3715:62
    }

    bb7: {
        Coverage::VirtualCounter(bcb10); // scope 1 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:293:13: 293:38
        _11 = panic(const "assertion failed: n != 0") -> bb20; // scope 1 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/macros/mod.rs:293:13: 293:38
    }

    bb8: {
        _15 = &mut _4;                   // scope 2 at src/raw.rs:3716:23: 3716:24
        _31 = const false;               // scope 2 at src/raw.rs:3716:25: 3716:28
        _17 = move _3;                   // scope 2 at src/raw.rs:3716:25: 3716:28
        _16 = (move _17, copy _12);      // scope 2 at src/raw.rs:3716:23: 3716:37
        _14 = <F as FnMut<(B, Bucket<T>)>>::call_mut(move _15, move _16) -> [return: bb9, unwind: bb20]; // scope 2 at src/raw.rs:3716:23: 3716:37
    }

    bb9: {
        _31 = const true;                // scope 2 at src/raw.rs:3716:17: 3716:20
        _3 = move _14;                   // scope 2 at src/raw.rs:3716:17: 3716:20
        _18 = SubWithOverflow(copy _2, const 1_usize); // scope 2 at src/raw.rs:3717:17: 3717:23
        assert(!move (_18.1: bool), "attempt to compute `{} - {}`, which would overflow", copy _2, const 1_usize) -> [success: bb10, unwind: bb20]; // scope 2 at src/raw.rs:3717:17: 3717:23
    }

    bb10: {
        _2 = move (_18.0: usize);        // scope 2 at src/raw.rs:3717:17: 3717:23
        goto -> bb2;                     // scope 0 at src/raw.rs:3711:13: 3718:14
    }

    bb11: {
        Coverage::VirtualCounter(bcb4);  // scope 0 at src/raw.rs:3760:5: 3760:6
        _31 = const false;               // scope 0 at src/raw.rs:3721:24: 3721:27
        _0 = move _3;                    // scope 0 at src/raw.rs:3721:24: 3721:27
        drop(_4) -> [return: bb19, unwind: bb23]; // scope 0 at src/raw.rs:3760:5: 3760:6
    }

    bb12: {
        Coverage::VirtualCounter(bcb5);  // scope 0 at src/raw.rs:3753:58: 3753:79
        _26 = copy (_1.2: *const u8);    // scope 0 at src/raw.rs:3753:58: 3753:72
        _25 = core::ptr::const_ptr::<impl *const u8>::cast::<Tag>(move _26) -> [return: bb13, unwind: bb20]; // scope 0 at src/raw.rs:3753:58: 3753:79
    }

    bb13: {
        _24 = Group::load_aligned(move _25) -> [return: bb14, unwind: bb20]; // scope 0 at src/raw.rs:3753:38: 3753:80
    }

    bb14: {
        _23 = &_24;                      // scope 0 at src/raw.rs:3753:38: 3753:80
        _22 = Group::match_full(move _23) -> [return: bb15, unwind: bb20]; // scope 0 at src/raw.rs:3753:38: 3754:34
    }

    bb15: {
        _21 = <BitMask as IntoIterator>::into_iter(move _22) -> [return: bb16, unwind: bb20]; // scope 0 at src/raw.rs:3753:38: 3755:33
    }

    bb16: {
        (_1.0: control::bitmask::BitMaskIter) = move _21; // scope 0 at src/raw.rs:3753:17: 3755:33
        _28 = &(_1.1: raw::Bucket<T>);   // scope 0 at src/raw.rs:3756:29: 3756:38
        _27 = Bucket::<T>::next_n(move _28, const control::group::sse2::Group::WIDTH) -> [return: bb17, unwind: bb20]; // scope 0 at src/raw.rs:3756:29: 3756:59
    }

    bb17: {
        (_1.1: raw::Bucket<T>) = move _27; // scope 0 at src/raw.rs:3756:17: 3756:59
        _30 = copy (_1.2: *const u8);    // scope 0 at src/raw.rs:3757:34: 3757:48
        _29 = core::ptr::const_ptr::<impl *const u8>::add(move _30, const control::group::sse2::Group::WIDTH) -> [return: bb18, unwind: bb20]; // scope 0 at src/raw.rs:3757:34: 3757:66
    }

    bb18: {
        (_1.2: *const u8) = move _29;    // scope 0 at src/raw.rs:3757:17: 3757:66
        goto -> bb1;                     // scope 0 at src/raw.rs:3710:9: 3759:10
    }

    bb19: {
        return;                          // scope 0 at src/raw.rs:3760:6: 3760:6
    }

    bb20 (cleanup): {
        drop(_4) -> [return: bb23, unwind terminate(cleanup)]; // scope 0 at src/raw.rs:3760:5: 3760:6
    }

    bb21 (cleanup): {
        resume;                          // scope 0 at src/raw.rs:3706:5: 3760:6
    }

    bb22 (cleanup): {
        drop(_3) -> [return: bb21, unwind terminate(cleanup)]; // scope 0 at src/raw.rs:3760:5: 3760:6
    }

    bb23 (cleanup): {
        switchInt(copy _31) -> [0: bb21, otherwise: bb22]; // scope 0 at src/raw.rs:3760:5: 3760:6
    }

    bb24: {
        unreachable;                     // scope 0 at src/raw.rs:3706:5: 3760:6
    }
}
"#;

    #[test]
    fn drop_flag_write_before_a_loop_is_not_the_decision() {
        // Before the fix both decomposed to `Const false`: the drop
        // flag write in the entry block was read as the function's
        // value, giving a candidate with no conditions.
        for text in [COPY_WIDE_MIR, FOLD_IMPL_MIR] {
            let mir = crate::mir::parse_text(text).expect("MIR parses");
            assert_eq!(mir.functions.len(), 1);
            let f = &mir.functions[0];
            assert_eq!(
                try_decompose_function(f),
                Err(DeclineReason::UnsupportedShape),
                "{}",
                f.name
            );
        }
    }

    /// `fn decide(a: bool, b: bool) -> Option<bool> { let r = if a { b } else { false }; Some(r) }`
    /// lowered with the per-arm value in `_3`, which the join block
    /// moves into `_0`.
    const ARM_LOCAL_FLOWS_TO_RETURN_MIR: &str = r#"
fn decide(_1: bool, _2: bool) -> Option<bool> {
    debug a => _1;
    debug b => _2;
    let mut _0: std::option::Option<bool>;
    let mut _3: bool;
    let mut _4: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _3 = copy _2;
        goto -> bb3;
    }

    bb2: {
        _3 = const false;
        goto -> bb3;
    }

    bb3: {
        _4 = move _3;
        _0 = Option::<bool>::Some(move _4);
        return;
    }
}
"#;

    #[test]
    fn arm_local_that_reaches_the_return_place_is_the_value() {
        let mir = crate::mir::parse_text(ARM_LOCAL_FLOWS_TO_RETURN_MIR).expect("MIR parses");
        assert_eq!(
            decompose_function(&mir.functions[0]),
            Some(ite("a", cond("b"), const_(false)))
        );
        // The same arm values written to a local the join block
        // overwrites never reach `_0`.
        let text = ARM_LOCAL_FLOWS_TO_RETURN_MIR.replace("_4 = move _3;", "_4 = const true;");
        let mir = crate::mir::parse_text(&text).expect("MIR parses");
        assert_eq!(decompose_function(&mir.functions[0]), None);
    }

    /// A body whose entry block sets the return place to a constant
    /// and only then branches (on something that does not change the
    /// result): the entry-block decomposer yields `Const false`.
    const CONST_ENTRY_MIR: &str = r#"
fn f(_1: bool) -> bool {
    debug flag => _1;
    let mut _0: bool;

    bb0: {
        _0 = const false;
        goto -> bb1;
    }

    bb1: {
        switchInt(copy _1) -> [0: bb3, otherwise: bb2];
    }

    bb2: {
        goto -> bb3;
    }

    bb3: {
        return;
    }
}
"#;

    #[test]
    fn zero_condition_tree_is_not_a_candidate() {
        let mir = crate::mir::parse_text(CONST_ENTRY_MIR).expect("MIR parses");
        let f = &mir.functions[0];
        assert_eq!(try_decompose_function(f), Ok(const_(false)));
        assert_eq!(try_decompose_candidate(f), Err(DeclineReason::NoConditions));
        assert_eq!(
            serde_json::to_value(DeclineReason::NoConditions).unwrap(),
            "no_conditions"
        );
        // A tree with a condition is still a candidate.
        let mir = crate::mir::parse_text(ARM_LOCAL_FLOWS_TO_RETURN_MIR).expect("MIR parses");
        assert!(try_decompose_candidate(&mir.functions[0]).is_ok());
    }

    /// rust-url `idna/src/uts46.rs`, `cargo floyd test` MIR
    /// (comment-only lines other than the CTFE marker stripped):
    /// the runtime body and the `// MIR FOR CTFE` body of
    ///
    /// ```text
    /// pub const fn new(deny_glyphless: bool, deny_list: &str) -> Self {
    ///     let mut bits = UPPER_CASE_MASK;
    ///     if deny_glyphless {
    ///         bits |= GLYPHLESS_MASK;
    ///     }
    ///     .. // a loop of assert!s over deny_list
    /// }
    /// ```
    ///
    /// The CTFE body keeps `_5 = copy _1; switchInt(move _5)` in bb0.
    const ASCII_DENY_LIST_NEW_MIR: &str = r#"
fn uts46::<impl at idna/src/uts46.rs:276:1: 276:19>::new(_1: bool, _2: &str) -> uts46::AsciiDenyList {
    debug deny_glyphless => _1;          // in scope 0 at idna/src/uts46.rs:294:22: 294:36
    debug deny_list => _2;               // in scope 0 at idna/src/uts46.rs:294:44: 294:53
    let mut _0: uts46::AsciiDenyList;    // return place in scope 0 at idna/src/uts46.rs:294:64: 294:68
    let mut _3: u128;                    // in scope 0 at idna/src/uts46.rs:295:13: 295:21
    let mut _6: bool;                    // in scope 0 at idna/src/uts46.rs:301:15: 301:30
    let mut _7: usize;                   // in scope 0 at idna/src/uts46.rs:301:15: 301:16
    let mut _8: usize;                   // in scope 0 at idna/src/uts46.rs:301:19: 301:30
    let _10: usize;                      // in scope 0 at idna/src/uts46.rs:302:27: 302:28
    let mut _11: usize;                  // in scope 0 at idna/src/uts46.rs:302:21: 302:29
    let mut _12: bool;                   // in scope 0 at idna/src/uts46.rs:302:21: 302:29
    let mut _13: bool;                   // in scope 0 at idna/src/uts46.rs:303:21: 303:29
    let mut _14: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let mut _15: bool;                   // in scope 0 at idna/src/uts46.rs:305:21: 305:30
    let mut _16: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let mut _17: bool;                   // in scope 0 at idna/src/uts46.rs:306:21: 306:30
    let mut _18: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let mut _19: bool;                   // in scope 0 at idna/src/uts46.rs:308:19: 308:30
    let mut _20: bool;                   // in scope 0 at idna/src/uts46.rs:308:34: 308:45
    let mut _21: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let mut _22: bool;                   // in scope 0 at idna/src/uts46.rs:312:19: 312:30
    let mut _23: bool;                   // in scope 0 at idna/src/uts46.rs:312:34: 312:45
    let mut _24: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let mut _25: bool;                   // in scope 0 at idna/src/uts46.rs:316:19: 316:30
    let mut _26: bool;                   // in scope 0 at idna/src/uts46.rs:316:34: 316:45
    let mut _27: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let mut _28: u128;                   // in scope 0 at idna/src/uts46.rs:319:21: 319:31
    let mut _29: bool;                   // in scope 0 at idna/src/uts46.rs:319:21: 319:31
    let mut _30: (usize, bool);          // in scope 0 at idna/src/uts46.rs:320:13: 320:19
    let mut _31: u128;                   // in scope 0 at idna/src/uts46.rs:322:16: 322:20
    scope 1 {
        debug bits => _3;                // in scope 1 at idna/src/uts46.rs:295:13: 295:21
        let mut _4: usize;               // in scope 1 at idna/src/uts46.rs:299:13: 299:18
        scope 2 {
            debug i => _4;               // in scope 2 at idna/src/uts46.rs:299:13: 299:18
            let _5: &[u8];               // in scope 2 at idna/src/uts46.rs:300:13: 300:18
            scope 3 {
                debug bytes => _5;       // in scope 3 at idna/src/uts46.rs:300:13: 300:18
                let _9: u8;              // in scope 3 at idna/src/uts46.rs:302:17: 302:18
                scope 4 {
                    debug b => _9;       // in scope 4 at idna/src/uts46.rs:302:17: 302:18
                }
            }
        }
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => idna/src/uts46.rs:296:12: 296:26 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => idna/src/uts46.rs:301:15: 301:30 (#0)
    coverage branch { true: BlockMarkerId(4), false: BlockMarkerId(5) } => idna/src/uts46.rs:303:13: 303:64 (#44)
    coverage branch { true: BlockMarkerId(6), false: BlockMarkerId(7) } => idna/src/uts46.rs:305:13: 305:76 (#48)
    coverage branch { true: BlockMarkerId(8), false: BlockMarkerId(9) } => idna/src/uts46.rs:306:13: 306:79 (#52)
    coverage branch { true: BlockMarkerId(10), false: BlockMarkerId(11) } => idna/src/uts46.rs:308:19: 308:30 (#0)
    coverage branch { true: BlockMarkerId(12), false: BlockMarkerId(13) } => idna/src/uts46.rs:308:34: 308:45 (#0)
    coverage branch { true: BlockMarkerId(14), false: BlockMarkerId(15) } => idna/src/uts46.rs:312:19: 312:30 (#0)
    coverage branch { true: BlockMarkerId(16), false: BlockMarkerId(17) } => idna/src/uts46.rs:312:34: 312:45 (#0)
    coverage branch { true: BlockMarkerId(18), false: BlockMarkerId(19) } => idna/src/uts46.rs:316:19: 316:30 (#0)
    coverage branch { true: BlockMarkerId(20), false: BlockMarkerId(21) } => idna/src/uts46.rs:316:34: 316:45 (#0)

    coverage Code { bcb: bcb0 } => idna/src/uts46.rs:294:5: 294:68 (#0);
    coverage Code { bcb: bcb0 } => idna/src/uts46.rs:295:24: 295:39 (#0);
    coverage Code { bcb: bcb0 } => idna/src/uts46.rs:296:12: 296:26 (#0);
    coverage Code { bcb: bcb1 } => idna/src/uts46.rs:297:13: 297:35 (#0);
    coverage Code { bcb: bcb27 } => idna/src/uts46.rs:298:9: 298:10 (#0);
    coverage Code { bcb: bcb2 } => idna/src/uts46.rs:299:21: 299:22 (#0);
    coverage Code { bcb: bcb2 } => idna/src/uts46.rs:300:21: 300:41 (#0);
    coverage Code { bcb: bcb3 } => idna/src/uts46.rs:301:15: 301:30 (#0);
    coverage Code { bcb: bcb4 } => idna/src/uts46.rs:302:21: 302:29 (#0);
    coverage Code { bcb: bcb4 } => idna/src/uts46.rs:303:13: 303:20 (#0);
    coverage Code { bcb: bcb4 } => idna/src/uts46.rs:303:21: 303:29 (#0);
    coverage Code { bcb: bcb5 } => idna/src/uts46.rs:305:13: 305:20 (#0);
    coverage Code { bcb: bcb5 } => idna/src/uts46.rs:305:21: 305:30 (#0);
    coverage Code { bcb: bcb6 } => idna/src/uts46.rs:306:13: 306:20 (#0);
    coverage Code { bcb: bcb6 } => idna/src/uts46.rs:306:21: 306:30 (#0);
    coverage Code { bcb: bcb7 } => idna/src/uts46.rs:307:13: 307:20 (#0);
    coverage Code { bcb: bcb7 } => idna/src/uts46.rs:308:19: 308:30 (#0);
    coverage Code { bcb: bcb8 } => idna/src/uts46.rs:308:34: 308:45 (#0);
    coverage Code { bcb: bcb11 } => idna/src/uts46.rs:311:13: 311:20 (#0);
    coverage Code { bcb: bcb11 } => idna/src/uts46.rs:312:19: 312:30 (#0);
    coverage Code { bcb: bcb12 } => idna/src/uts46.rs:312:34: 312:45 (#0);
    coverage Code { bcb: bcb15 } => idna/src/uts46.rs:315:13: 315:20 (#0);
    coverage Code { bcb: bcb15 } => idna/src/uts46.rs:316:19: 316:30 (#0);
    coverage Code { bcb: bcb16 } => idna/src/uts46.rs:316:34: 316:45 (#0);
    coverage Code { bcb: bcb19 } => idna/src/uts46.rs:319:13: 319:17 (#0);
    coverage Code { bcb: bcb19 } => idna/src/uts46.rs:319:13: 319:31 (#0);
    coverage Code { bcb: bcb19 } => idna/src/uts46.rs:320:13: 320:19 (#0);
    coverage Code { bcb: bcb26 } => idna/src/uts46.rs:322:9: 322:22 (#0);
    coverage Code { bcb: bcb26 } => idna/src/uts46.rs:323:5: 323:6 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb27 } => idna/src/uts46.rs:296:12: 296:26 (#0);
    coverage Branch { true_bcb: bcb4, false_bcb: bcb26 } => idna/src/uts46.rs:301:15: 301:30 (#0);
    coverage Branch { true_bcb: bcb8, false_bcb: bcb22 } => idna/src/uts46.rs:308:19: 308:30 (#0);
    coverage Branch { true_bcb: bcb9, false_bcb: bcb10 } => idna/src/uts46.rs:308:34: 308:45 (#0);
    coverage Branch { true_bcb: bcb12, false_bcb: bcb21 } => idna/src/uts46.rs:312:19: 312:30 (#0);
    coverage Branch { true_bcb: bcb13, false_bcb: bcb14 } => idna/src/uts46.rs:312:34: 312:45 (#0);
    coverage Branch { true_bcb: bcb16, false_bcb: bcb20 } => idna/src/uts46.rs:316:19: 316:30 (#0);
    coverage Branch { true_bcb: bcb17, false_bcb: bcb18 } => idna/src/uts46.rs:316:34: 316:45 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 1 at idna/src/uts46.rs:296:12: 296:26
        _3 = const uts46::UPPER_CASE_MASK; // scope 0 at idna/src/uts46.rs:295:24: 295:39
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 1 at idna/src/uts46.rs:296:12: 296:26
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 1 at idna/src/uts46.rs:296:9: 298:10
        _3 = BitOr(copy _3, const uts46::GLYPHLESS_MASK); // scope 1 at idna/src/uts46.rs:297:13: 297:35
        goto -> bb3;                     // scope 1 at idna/src/uts46.rs:296:9: 298:10
    }

    bb2: {
        Coverage::VirtualCounter(bcb27); // scope 1 at idna/src/uts46.rs:296:9: 298:10
        goto -> bb3;                     // scope 1 at idna/src/uts46.rs:296:9: 298:10
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 2 at idna/src/uts46.rs:300:21: 300:41
        _4 = const 0_usize;              // scope 1 at idna/src/uts46.rs:299:21: 299:22
        _5 = core::str::<impl str>::as_bytes(copy _2) -> [return: bb4, unwind continue]; // scope 2 at idna/src/uts46.rs:300:21: 300:41
    }

    bb4: {
        Coverage::VirtualCounter(bcb3);  // scope 3 at idna/src/uts46.rs:301:9: 321:10
        _7 = copy _4;                    // scope 3 at idna/src/uts46.rs:301:15: 301:16
        _8 = PtrMetadata(copy _5);       // scope 3 at idna/src/uts46.rs:301:19: 301:30
        _6 = Lt(move _7, move _8);       // scope 3 at idna/src/uts46.rs:301:15: 301:30
        switchInt(move _6) -> [0: bb6, otherwise: bb5]; // scope 3 at idna/src/uts46.rs:301:15: 301:30
    }

    bb5: {
        Coverage::VirtualCounter(bcb4);  // scope 3 at idna/src/uts46.rs:302:21: 302:29
        _10 = copy _4;                   // scope 3 at idna/src/uts46.rs:302:27: 302:28
        _11 = PtrMetadata(copy _5);      // scope 3 at idna/src/uts46.rs:302:21: 302:29
        _12 = Lt(copy _10, copy _11);    // scope 3 at idna/src/uts46.rs:302:21: 302:29
        assert(move _12, "index out of bounds: the length is {} but the index is {}", move _11, copy _10) -> [success: bb7, unwind continue]; // scope 3 at idna/src/uts46.rs:302:21: 302:29
    }

    bb6: {
        Coverage::VirtualCounter(bcb26); // scope 0 at idna/src/uts46.rs:323:6: 323:6
        _31 = copy _3;                   // scope 3 at idna/src/uts46.rs:322:16: 322:20
        _0 = uts46::AsciiDenyList { bits: move _31 }; // scope 3 at idna/src/uts46.rs:322:9: 322:22
        return;                          // scope 0 at idna/src/uts46.rs:323:6: 323:6
    }

    bb7: {
        _9 = copy (*_5)[_10];            // scope 3 at idna/src/uts46.rs:302:21: 302:29
        _13 = Lt(copy _9, const 128_u8); // scope 4 at idna/src/uts46.rs:303:21: 303:29
        switchInt(move _13) -> [0: bb9, otherwise: bb8]; // scope 4 at idna/src/uts46.rs:303:21: 303:29
    }

    bb8: {
        Coverage::VirtualCounter(bcb5);  // scope 4 at idna/src/uts46.rs:305:21: 305:30
        _15 = Ne(copy _9, const 46_u8);  // scope 4 at idna/src/uts46.rs:305:21: 305:30
        switchInt(move _15) -> [0: bb11, otherwise: bb10]; // scope 4 at idna/src/uts46.rs:305:21: 305:30
    }

    bb9: {
        Coverage::VirtualCounter(bcb25); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _14 = panic(const "ASCII deny list must be ASCII.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb10: {
        Coverage::VirtualCounter(bcb6);  // scope 4 at idna/src/uts46.rs:306:21: 306:30
        _17 = Ne(copy _9, const 45_u8);  // scope 4 at idna/src/uts46.rs:306:21: 306:30
        switchInt(move _17) -> [0: bb13, otherwise: bb12]; // scope 4 at idna/src/uts46.rs:306:21: 306:30
    }

    bb11: {
        Coverage::VirtualCounter(bcb24); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _16 = panic(const "ASCII deny list must not contain the dot.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb12: {
        Coverage::VirtualCounter(bcb7);  // scope 4 at idna/src/uts46.rs:308:19: 308:30
        _19 = Ge(copy _9, const 48_u8);  // scope 4 at idna/src/uts46.rs:308:19: 308:30
        switchInt(move _19) -> [0: bb15, otherwise: bb14]; // scope 4 at idna/src/uts46.rs:308:19: 308:30
    }

    bb13: {
        Coverage::VirtualCounter(bcb23); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _18 = panic(const "ASCII deny list must not contain the hyphen.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb14: {
        Coverage::VirtualCounter(bcb8);  // scope 4 at idna/src/uts46.rs:308:34: 308:45
        _20 = Le(copy _9, const 57_u8);  // scope 4 at idna/src/uts46.rs:308:34: 308:45
        switchInt(move _20) -> [0: bb17, otherwise: bb16]; // scope 4 at idna/src/uts46.rs:308:34: 308:45
    }

    bb15: {
        Coverage::VirtualCounter(bcb22); // scope 4 at no-location
        goto -> bb18;                    // scope 4 at no-location
    }

    bb16: {
        Coverage::VirtualCounter(bcb9);  // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _21 = panic(const "ASCII deny list must not contain digits.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb17: {
        Coverage::VirtualCounter(bcb10); // scope 4 at no-location
        goto -> bb18;                    // scope 4 at no-location
    }

    bb18: {
        Coverage::VirtualCounter(bcb11); // scope 4 at idna/src/uts46.rs:312:19: 312:30
        _22 = Ge(copy _9, const 97_u8);  // scope 4 at idna/src/uts46.rs:312:19: 312:30
        switchInt(move _22) -> [0: bb20, otherwise: bb19]; // scope 4 at idna/src/uts46.rs:312:19: 312:30
    }

    bb19: {
        Coverage::VirtualCounter(bcb12); // scope 4 at idna/src/uts46.rs:312:34: 312:45
        _23 = Le(copy _9, const 122_u8); // scope 4 at idna/src/uts46.rs:312:34: 312:45
        switchInt(move _23) -> [0: bb22, otherwise: bb21]; // scope 4 at idna/src/uts46.rs:312:34: 312:45
    }

    bb20: {
        Coverage::VirtualCounter(bcb21); // scope 4 at no-location
        goto -> bb23;                    // scope 4 at no-location
    }

    bb21: {
        Coverage::VirtualCounter(bcb13); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _24 = panic(const "ASCII deny list must not contain letters.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb22: {
        Coverage::VirtualCounter(bcb14); // scope 4 at no-location
        goto -> bb23;                    // scope 4 at no-location
    }

    bb23: {
        Coverage::VirtualCounter(bcb15); // scope 4 at idna/src/uts46.rs:316:19: 316:30
        _25 = Ge(copy _9, const 65_u8);  // scope 4 at idna/src/uts46.rs:316:19: 316:30
        switchInt(move _25) -> [0: bb25, otherwise: bb24]; // scope 4 at idna/src/uts46.rs:316:19: 316:30
    }

    bb24: {
        Coverage::VirtualCounter(bcb16); // scope 4 at idna/src/uts46.rs:316:34: 316:45
        _26 = Le(copy _9, const 90_u8);  // scope 4 at idna/src/uts46.rs:316:34: 316:45
        switchInt(move _26) -> [0: bb27, otherwise: bb26]; // scope 4 at idna/src/uts46.rs:316:34: 316:45
    }

    bb25: {
        Coverage::VirtualCounter(bcb20); // scope 4 at no-location
        goto -> bb28;                    // scope 4 at no-location
    }

    bb26: {
        Coverage::VirtualCounter(bcb17); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _27 = panic(const "ASCII deny list must not contain letters.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb27: {
        Coverage::VirtualCounter(bcb18); // scope 4 at no-location
        goto -> bb28;                    // scope 4 at no-location
    }

    bb28: {
        Coverage::VirtualCounter(bcb19); // scope 4 at idna/src/uts46.rs:319:21: 319:31
        _29 = Lt(copy _9, const 128_u8); // scope 4 at idna/src/uts46.rs:319:21: 319:31
        assert(move _29, "attempt to shift left by `{}`, which would overflow", copy _9) -> [success: bb29, unwind continue]; // scope 4 at idna/src/uts46.rs:319:21: 319:31
    }

    bb29: {
        _28 = Shl(const 1_u128, copy _9); // scope 4 at idna/src/uts46.rs:319:21: 319:31
        _3 = BitOr(copy _3, move _28);   // scope 4 at idna/src/uts46.rs:319:13: 319:31
        _30 = AddWithOverflow(copy _4, const 1_usize); // scope 4 at idna/src/uts46.rs:320:13: 320:19
        assert(!move (_30.1: bool), "attempt to compute `{} + {}`, which would overflow", copy _4, const 1_usize) -> [success: bb30, unwind continue]; // scope 4 at idna/src/uts46.rs:320:13: 320:19
    }

    bb30: {
        _4 = move (_30.0: usize);        // scope 4 at idna/src/uts46.rs:320:13: 320:19
        goto -> bb4;                     // scope 3 at idna/src/uts46.rs:301:9: 321:10
    }
}

// MIR FOR CTFE
fn uts46::<impl at idna/src/uts46.rs:276:1: 276:19>::new(_1: bool, _2: &str) -> uts46::AsciiDenyList {
    debug deny_glyphless => _1;          // in scope 0 at idna/src/uts46.rs:294:22: 294:36
    debug deny_list => _2;               // in scope 0 at idna/src/uts46.rs:294:44: 294:53
    let mut _0: uts46::AsciiDenyList;    // return place in scope 0 at idna/src/uts46.rs:294:64: 294:68
    let mut _3: u128;                    // in scope 0 at idna/src/uts46.rs:295:13: 295:21
    let _4: ();                          // in scope 0 at idna/src/uts46.rs:296:9: 298:10
    let mut _5: bool;                    // in scope 0 at idna/src/uts46.rs:296:12: 296:26
    let mut _8: &str;                    // in scope 0 at idna/src/uts46.rs:300:21: 300:30
    let _9: ();                          // in scope 0 at idna/src/uts46.rs:301:9: 321:10
    let mut _10: ();                     // in scope 0 at idna/src/uts46.rs:294:5: 323:6
    let mut _11: bool;                   // in scope 0 at idna/src/uts46.rs:301:15: 301:30
    let mut _12: usize;                  // in scope 0 at idna/src/uts46.rs:301:15: 301:16
    let mut _13: usize;                  // in scope 0 at idna/src/uts46.rs:301:19: 301:30
    let mut _14: &[u8];                  // in scope 0 at idna/src/uts46.rs:301:19: 301:24
    let _16: usize;                      // in scope 0 at idna/src/uts46.rs:302:27: 302:28
    let mut _17: usize;                  // in scope 0 at idna/src/uts46.rs:302:21: 302:29
    let mut _18: bool;                   // in scope 0 at idna/src/uts46.rs:302:21: 302:29
    let _19: ();                         // in scope 0 at idna/src/uts46.rs:303:13: 303:64
    let mut _20: bool;                   // in scope 0 at idna/src/uts46.rs:303:21: 303:29
    let mut _21: u8;                     // in scope 0 at idna/src/uts46.rs:303:21: 303:22
    let mut _22: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let _23: ();                         // in scope 0 at idna/src/uts46.rs:305:13: 305:76
    let mut _24: bool;                   // in scope 0 at idna/src/uts46.rs:305:21: 305:30
    let mut _25: u8;                     // in scope 0 at idna/src/uts46.rs:305:21: 305:22
    let mut _26: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let _27: ();                         // in scope 0 at idna/src/uts46.rs:306:13: 306:79
    let mut _28: bool;                   // in scope 0 at idna/src/uts46.rs:306:21: 306:30
    let mut _29: u8;                     // in scope 0 at idna/src/uts46.rs:306:21: 306:22
    let mut _30: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let _31: ();                         // in scope 0 at idna/src/uts46.rs:307:13: 310:14
    let mut _32: bool;                   // in scope 0 at idna/src/uts46.rs:308:19: 308:30
    let mut _33: u8;                     // in scope 0 at idna/src/uts46.rs:308:20: 308:21
    let mut _34: bool;                   // in scope 0 at idna/src/uts46.rs:308:34: 308:45
    let mut _35: u8;                     // in scope 0 at idna/src/uts46.rs:308:35: 308:36
    let mut _36: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let _37: ();                         // in scope 0 at idna/src/uts46.rs:311:13: 314:14
    let mut _38: bool;                   // in scope 0 at idna/src/uts46.rs:312:19: 312:30
    let mut _39: u8;                     // in scope 0 at idna/src/uts46.rs:312:20: 312:21
    let mut _40: bool;                   // in scope 0 at idna/src/uts46.rs:312:34: 312:45
    let mut _41: u8;                     // in scope 0 at idna/src/uts46.rs:312:35: 312:36
    let mut _42: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let _43: ();                         // in scope 0 at idna/src/uts46.rs:315:13: 318:14
    let mut _44: bool;                   // in scope 0 at idna/src/uts46.rs:316:19: 316:30
    let mut _45: u8;                     // in scope 0 at idna/src/uts46.rs:316:20: 316:21
    let mut _46: bool;                   // in scope 0 at idna/src/uts46.rs:316:34: 316:45
    let mut _47: u8;                     // in scope 0 at idna/src/uts46.rs:316:35: 316:36
    let mut _48: !;                      // in scope 0 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    let mut _49: u128;                   // in scope 0 at idna/src/uts46.rs:319:21: 319:31
    let mut _50: u8;                     // in scope 0 at idna/src/uts46.rs:319:30: 319:31
    let mut _51: bool;                   // in scope 0 at idna/src/uts46.rs:319:21: 319:31
    let mut _52: (usize, bool);          // in scope 0 at idna/src/uts46.rs:320:13: 320:19
    let mut _53: !;                      // in scope 0 at idna/src/uts46.rs:301:9: 321:10
    let _54: ();                         // in scope 0 at idna/src/uts46.rs:301:9: 321:10
    let mut _55: !;                      // in scope 0 at idna/src/uts46.rs:301:9: 321:10
    let mut _56: u128;                   // in scope 0 at idna/src/uts46.rs:322:16: 322:20
    scope 1 {
        debug bits => _3;                // in scope 1 at idna/src/uts46.rs:295:13: 295:21
        let mut _6: usize;               // in scope 1 at idna/src/uts46.rs:299:13: 299:18
        scope 2 {
            debug i => _6;               // in scope 2 at idna/src/uts46.rs:299:13: 299:18
            let _7: &[u8];               // in scope 2 at idna/src/uts46.rs:300:13: 300:18
            scope 3 {
                debug bytes => _7;       // in scope 3 at idna/src/uts46.rs:300:13: 300:18
                let _15: u8;             // in scope 3 at idna/src/uts46.rs:302:17: 302:18
                scope 4 {
                    debug b => _15;      // in scope 4 at idna/src/uts46.rs:302:17: 302:18
                }
            }
        }
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => idna/src/uts46.rs:296:12: 296:26 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => idna/src/uts46.rs:301:15: 301:30 (#0)
    coverage branch { true: BlockMarkerId(4), false: BlockMarkerId(5) } => idna/src/uts46.rs:303:13: 303:64 (#44)
    coverage branch { true: BlockMarkerId(6), false: BlockMarkerId(7) } => idna/src/uts46.rs:305:13: 305:76 (#48)
    coverage branch { true: BlockMarkerId(8), false: BlockMarkerId(9) } => idna/src/uts46.rs:306:13: 306:79 (#52)
    coverage branch { true: BlockMarkerId(10), false: BlockMarkerId(11) } => idna/src/uts46.rs:308:19: 308:30 (#0)
    coverage branch { true: BlockMarkerId(12), false: BlockMarkerId(13) } => idna/src/uts46.rs:308:34: 308:45 (#0)
    coverage branch { true: BlockMarkerId(14), false: BlockMarkerId(15) } => idna/src/uts46.rs:312:19: 312:30 (#0)
    coverage branch { true: BlockMarkerId(16), false: BlockMarkerId(17) } => idna/src/uts46.rs:312:34: 312:45 (#0)
    coverage branch { true: BlockMarkerId(18), false: BlockMarkerId(19) } => idna/src/uts46.rs:316:19: 316:30 (#0)
    coverage branch { true: BlockMarkerId(20), false: BlockMarkerId(21) } => idna/src/uts46.rs:316:34: 316:45 (#0)

    coverage Code { bcb: bcb0 } => idna/src/uts46.rs:294:5: 294:68 (#0);
    coverage Code { bcb: bcb0 } => idna/src/uts46.rs:295:24: 295:39 (#0);
    coverage Code { bcb: bcb0 } => idna/src/uts46.rs:296:12: 296:26 (#0);
    coverage Code { bcb: bcb1 } => idna/src/uts46.rs:297:13: 297:35 (#0);
    coverage Code { bcb: bcb27 } => idna/src/uts46.rs:298:9: 298:10 (#0);
    coverage Code { bcb: bcb2 } => idna/src/uts46.rs:299:21: 299:22 (#0);
    coverage Code { bcb: bcb2 } => idna/src/uts46.rs:300:21: 300:41 (#0);
    coverage Code { bcb: bcb3 } => idna/src/uts46.rs:301:15: 301:30 (#0);
    coverage Code { bcb: bcb4 } => idna/src/uts46.rs:302:21: 302:29 (#0);
    coverage Code { bcb: bcb4 } => idna/src/uts46.rs:303:13: 303:20 (#0);
    coverage Code { bcb: bcb4 } => idna/src/uts46.rs:303:21: 303:29 (#0);
    coverage Code { bcb: bcb5 } => idna/src/uts46.rs:305:13: 305:20 (#0);
    coverage Code { bcb: bcb5 } => idna/src/uts46.rs:305:21: 305:30 (#0);
    coverage Code { bcb: bcb6 } => idna/src/uts46.rs:306:13: 306:20 (#0);
    coverage Code { bcb: bcb6 } => idna/src/uts46.rs:306:21: 306:30 (#0);
    coverage Code { bcb: bcb7 } => idna/src/uts46.rs:307:13: 307:20 (#0);
    coverage Code { bcb: bcb7 } => idna/src/uts46.rs:308:19: 308:30 (#0);
    coverage Code { bcb: bcb8 } => idna/src/uts46.rs:308:34: 308:45 (#0);
    coverage Code { bcb: bcb11 } => idna/src/uts46.rs:311:13: 311:20 (#0);
    coverage Code { bcb: bcb11 } => idna/src/uts46.rs:312:19: 312:30 (#0);
    coverage Code { bcb: bcb12 } => idna/src/uts46.rs:312:34: 312:45 (#0);
    coverage Code { bcb: bcb15 } => idna/src/uts46.rs:315:13: 315:20 (#0);
    coverage Code { bcb: bcb15 } => idna/src/uts46.rs:316:19: 316:30 (#0);
    coverage Code { bcb: bcb16 } => idna/src/uts46.rs:316:34: 316:45 (#0);
    coverage Code { bcb: bcb19 } => idna/src/uts46.rs:319:13: 319:17 (#0);
    coverage Code { bcb: bcb19 } => idna/src/uts46.rs:319:13: 319:31 (#0);
    coverage Code { bcb: bcb19 } => idna/src/uts46.rs:320:13: 320:19 (#0);
    coverage Code { bcb: bcb26 } => idna/src/uts46.rs:322:9: 322:22 (#0);
    coverage Code { bcb: bcb26 } => idna/src/uts46.rs:323:5: 323:6 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb27 } => idna/src/uts46.rs:296:12: 296:26 (#0);
    coverage Branch { true_bcb: bcb4, false_bcb: bcb26 } => idna/src/uts46.rs:301:15: 301:30 (#0);
    coverage Branch { true_bcb: bcb8, false_bcb: bcb22 } => idna/src/uts46.rs:308:19: 308:30 (#0);
    coverage Branch { true_bcb: bcb9, false_bcb: bcb10 } => idna/src/uts46.rs:308:34: 308:45 (#0);
    coverage Branch { true_bcb: bcb12, false_bcb: bcb21 } => idna/src/uts46.rs:312:19: 312:30 (#0);
    coverage Branch { true_bcb: bcb13, false_bcb: bcb14 } => idna/src/uts46.rs:312:34: 312:45 (#0);
    coverage Branch { true_bcb: bcb16, false_bcb: bcb20 } => idna/src/uts46.rs:316:19: 316:30 (#0);
    coverage Branch { true_bcb: bcb17, false_bcb: bcb18 } => idna/src/uts46.rs:316:34: 316:45 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 1 at idna/src/uts46.rs:296:12: 296:26
        StorageLive(_3);                 // scope 0 at idna/src/uts46.rs:295:13: 295:21
        _3 = const uts46::UPPER_CASE_MASK; // scope 0 at idna/src/uts46.rs:295:24: 295:39
        StorageLive(_4);                 // scope 1 at idna/src/uts46.rs:296:9: 298:10
        StorageLive(_5);                 // scope 1 at idna/src/uts46.rs:296:12: 296:26
        _5 = copy _1;                    // scope 1 at idna/src/uts46.rs:296:12: 296:26
        switchInt(move _5) -> [0: bb2, otherwise: bb1]; // scope 1 at idna/src/uts46.rs:296:12: 296:26
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 1 at idna/src/uts46.rs:296:9: 298:10
        _3 = BitOr(copy _3, const uts46::GLYPHLESS_MASK); // scope 1 at idna/src/uts46.rs:297:13: 297:35
        _4 = const ();                   // scope 1 at idna/src/uts46.rs:296:27: 298:10
        goto -> bb3;                     // scope 1 at idna/src/uts46.rs:296:9: 298:10
    }

    bb2: {
        Coverage::VirtualCounter(bcb27); // scope 1 at idna/src/uts46.rs:296:9: 298:10
        _4 = const ();                   // scope 1 at idna/src/uts46.rs:298:10: 298:10
        goto -> bb3;                     // scope 1 at idna/src/uts46.rs:296:9: 298:10
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 2 at idna/src/uts46.rs:300:21: 300:41
        StorageDead(_5);                 // scope 1 at idna/src/uts46.rs:298:9: 298:10
        StorageDead(_4);                 // scope 1 at idna/src/uts46.rs:298:9: 298:10
        StorageLive(_6);                 // scope 1 at idna/src/uts46.rs:299:13: 299:18
        _6 = const 0_usize;              // scope 1 at idna/src/uts46.rs:299:21: 299:22
        StorageLive(_7);                 // scope 2 at idna/src/uts46.rs:300:13: 300:18
        StorageLive(_8);                 // scope 2 at idna/src/uts46.rs:300:21: 300:30
        _8 = &(*_2);                     // scope 2 at idna/src/uts46.rs:300:21: 300:30
        ConstEvalCounter;                // scope 2 at idna/src/uts46.rs:300:21: 300:41
        _7 = core::str::<impl str>::as_bytes(move _8) -> [return: bb4, unwind continue]; // scope 2 at idna/src/uts46.rs:300:21: 300:41
    }

    bb4: {
        StorageDead(_8);                 // scope 2 at idna/src/uts46.rs:300:40: 300:41
        StorageLive(_9);                 // scope 3 at idna/src/uts46.rs:301:9: 321:10
        goto -> bb5;                     // scope 3 at idna/src/uts46.rs:301:9: 321:10
    }

    bb5: {
        Coverage::VirtualCounter(bcb3);  // scope 3 at idna/src/uts46.rs:301:9: 321:10
        StorageLive(_11);                // scope 3 at idna/src/uts46.rs:301:15: 301:30
        StorageLive(_12);                // scope 3 at idna/src/uts46.rs:301:15: 301:16
        _12 = copy _6;                   // scope 3 at idna/src/uts46.rs:301:15: 301:16
        StorageLive(_13);                // scope 3 at idna/src/uts46.rs:301:19: 301:30
        StorageLive(_14);                // scope 3 at idna/src/uts46.rs:301:19: 301:24
        _14 = &(*_7);                    // scope 3 at idna/src/uts46.rs:301:19: 301:24
        ConstEvalCounter;                // scope 3 at idna/src/uts46.rs:301:19: 301:30
        _13 = core::slice::<impl [u8]>::len(move _14) -> [return: bb6, unwind continue]; // scope 3 at idna/src/uts46.rs:301:19: 301:30
    }

    bb6: {
        StorageDead(_14);                // scope 3 at idna/src/uts46.rs:301:29: 301:30
        _11 = Lt(move _12, move _13);    // scope 3 at idna/src/uts46.rs:301:15: 301:30
        switchInt(move _11) -> [0: bb8, otherwise: bb7]; // scope 3 at idna/src/uts46.rs:301:15: 301:30
    }

    bb7: {
        Coverage::VirtualCounter(bcb4);  // scope 3 at idna/src/uts46.rs:302:21: 302:29
        StorageDead(_13);                // scope 3 at idna/src/uts46.rs:301:29: 301:30
        StorageDead(_12);                // scope 3 at idna/src/uts46.rs:301:29: 301:30
        StorageLive(_15);                // scope 3 at idna/src/uts46.rs:302:17: 302:18
        StorageLive(_16);                // scope 3 at idna/src/uts46.rs:302:27: 302:28
        _16 = copy _6;                   // scope 3 at idna/src/uts46.rs:302:27: 302:28
        _17 = PtrMetadata(copy _7);      // scope 3 at idna/src/uts46.rs:302:21: 302:29
        _18 = Lt(copy _16, copy _17);    // scope 3 at idna/src/uts46.rs:302:21: 302:29
        assert(move _18, "index out of bounds: the length is {} but the index is {}", move _17, copy _16) -> [success: bb9, unwind continue]; // scope 3 at idna/src/uts46.rs:302:21: 302:29
    }

    bb8: {
        Coverage::VirtualCounter(bcb26); // scope 0 at idna/src/uts46.rs:323:6: 323:6
        StorageDead(_13);                // scope 3 at idna/src/uts46.rs:301:29: 301:30
        StorageDead(_12);                // scope 3 at idna/src/uts46.rs:301:29: 301:30
        StorageLive(_54);                // scope 3 at idna/src/uts46.rs:301:9: 321:10
        _9 = const ();                   // scope 3 at idna/src/uts46.rs:301:9: 321:10
        StorageDead(_54);                // scope 3 at idna/src/uts46.rs:321:9: 321:10
        StorageDead(_11);                // scope 3 at idna/src/uts46.rs:321:9: 321:10
        StorageDead(_9);                 // scope 3 at idna/src/uts46.rs:321:9: 321:10
        StorageLive(_56);                // scope 3 at idna/src/uts46.rs:322:16: 322:20
        _56 = copy _3;                   // scope 3 at idna/src/uts46.rs:322:16: 322:20
        _0 = uts46::AsciiDenyList { bits: move _56 }; // scope 3 at idna/src/uts46.rs:322:9: 322:22
        StorageDead(_56);                // scope 3 at idna/src/uts46.rs:322:21: 322:22
        StorageDead(_7);                 // scope 2 at idna/src/uts46.rs:323:5: 323:6
        StorageDead(_6);                 // scope 1 at idna/src/uts46.rs:323:5: 323:6
        StorageDead(_3);                 // scope 0 at idna/src/uts46.rs:323:5: 323:6
        return;                          // scope 0 at idna/src/uts46.rs:323:6: 323:6
    }

    bb9: {
        _15 = copy (*_7)[_16];           // scope 3 at idna/src/uts46.rs:302:21: 302:29
        StorageDead(_16);                // scope 3 at idna/src/uts46.rs:302:29: 302:30
        StorageLive(_19);                // scope 4 at idna/src/uts46.rs:303:13: 303:64
        StorageLive(_20);                // scope 4 at idna/src/uts46.rs:303:21: 303:29
        StorageLive(_21);                // scope 4 at idna/src/uts46.rs:303:21: 303:22
        _21 = copy _15;                  // scope 4 at idna/src/uts46.rs:303:21: 303:22
        _20 = Lt(move _21, const 128_u8); // scope 4 at idna/src/uts46.rs:303:21: 303:29
        switchInt(move _20) -> [0: bb11, otherwise: bb10]; // scope 4 at idna/src/uts46.rs:303:21: 303:29
    }

    bb10: {
        Coverage::VirtualCounter(bcb5);  // scope 4 at idna/src/uts46.rs:305:21: 305:30
        StorageDead(_21);                // scope 4 at idna/src/uts46.rs:303:28: 303:29
        _19 = const ();                  // scope 4 at idna/src/uts46.rs:303:64: 303:64
        StorageDead(_20);                // scope 4 at idna/src/uts46.rs:303:63: 303:64
        StorageDead(_19);                // scope 4 at idna/src/uts46.rs:303:63: 303:64
        StorageLive(_23);                // scope 4 at idna/src/uts46.rs:305:13: 305:76
        StorageLive(_24);                // scope 4 at idna/src/uts46.rs:305:21: 305:30
        StorageLive(_25);                // scope 4 at idna/src/uts46.rs:305:21: 305:22
        _25 = copy _15;                  // scope 4 at idna/src/uts46.rs:305:21: 305:22
        _24 = Ne(move _25, const 46_u8); // scope 4 at idna/src/uts46.rs:305:21: 305:30
        switchInt(move _24) -> [0: bb13, otherwise: bb12]; // scope 4 at idna/src/uts46.rs:305:21: 305:30
    }

    bb11: {
        Coverage::VirtualCounter(bcb25); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        StorageDead(_21);                // scope 4 at idna/src/uts46.rs:303:28: 303:29
        StorageLive(_22);                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        ConstEvalCounter;                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _22 = panic(const "ASCII deny list must be ASCII.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb12: {
        Coverage::VirtualCounter(bcb6);  // scope 4 at idna/src/uts46.rs:306:21: 306:30
        StorageDead(_25);                // scope 4 at idna/src/uts46.rs:305:29: 305:30
        _23 = const ();                  // scope 4 at idna/src/uts46.rs:305:76: 305:76
        StorageDead(_24);                // scope 4 at idna/src/uts46.rs:305:75: 305:76
        StorageDead(_23);                // scope 4 at idna/src/uts46.rs:305:75: 305:76
        StorageLive(_27);                // scope 4 at idna/src/uts46.rs:306:13: 306:79
        StorageLive(_28);                // scope 4 at idna/src/uts46.rs:306:21: 306:30
        StorageLive(_29);                // scope 4 at idna/src/uts46.rs:306:21: 306:22
        _29 = copy _15;                  // scope 4 at idna/src/uts46.rs:306:21: 306:22
        _28 = Ne(move _29, const 45_u8); // scope 4 at idna/src/uts46.rs:306:21: 306:30
        switchInt(move _28) -> [0: bb15, otherwise: bb14]; // scope 4 at idna/src/uts46.rs:306:21: 306:30
    }

    bb13: {
        Coverage::VirtualCounter(bcb24); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        StorageDead(_25);                // scope 4 at idna/src/uts46.rs:305:29: 305:30
        StorageLive(_26);                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        ConstEvalCounter;                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _26 = panic(const "ASCII deny list must not contain the dot.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb14: {
        Coverage::VirtualCounter(bcb7);  // scope 4 at idna/src/uts46.rs:308:19: 308:30
        StorageDead(_29);                // scope 4 at idna/src/uts46.rs:306:29: 306:30
        _27 = const ();                  // scope 4 at idna/src/uts46.rs:306:79: 306:79
        StorageDead(_28);                // scope 4 at idna/src/uts46.rs:306:78: 306:79
        StorageDead(_27);                // scope 4 at idna/src/uts46.rs:306:78: 306:79
        StorageLive(_31);                // scope 4 at idna/src/uts46.rs:307:13: 310:14
        StorageLive(_32);                // scope 4 at idna/src/uts46.rs:308:19: 308:30
        StorageLive(_33);                // scope 4 at idna/src/uts46.rs:308:20: 308:21
        _33 = copy _15;                  // scope 4 at idna/src/uts46.rs:308:20: 308:21
        _32 = Ge(move _33, const 48_u8); // scope 4 at idna/src/uts46.rs:308:19: 308:30
        switchInt(move _32) -> [0: bb17, otherwise: bb16]; // scope 4 at idna/src/uts46.rs:308:19: 308:30
    }

    bb15: {
        Coverage::VirtualCounter(bcb23); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        StorageDead(_29);                // scope 4 at idna/src/uts46.rs:306:29: 306:30
        StorageLive(_30);                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        ConstEvalCounter;                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _30 = panic(const "ASCII deny list must not contain the hyphen.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb16: {
        Coverage::VirtualCounter(bcb8);  // scope 4 at idna/src/uts46.rs:308:34: 308:45
        StorageDead(_33);                // scope 4 at idna/src/uts46.rs:308:29: 308:30
        StorageLive(_34);                // scope 4 at idna/src/uts46.rs:308:34: 308:45
        StorageLive(_35);                // scope 4 at idna/src/uts46.rs:308:35: 308:36
        _35 = copy _15;                  // scope 4 at idna/src/uts46.rs:308:35: 308:36
        _34 = Le(move _35, const 57_u8); // scope 4 at idna/src/uts46.rs:308:34: 308:45
        switchInt(move _34) -> [0: bb19, otherwise: bb18]; // scope 4 at idna/src/uts46.rs:308:34: 308:45
    }

    bb17: {
        Coverage::VirtualCounter(bcb22); // scope 4 at no-location
        StorageDead(_33);                // scope 4 at idna/src/uts46.rs:308:29: 308:30
        goto -> bb20;                    // scope 4 at no-location
    }

    bb18: {
        Coverage::VirtualCounter(bcb9);  // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        StorageDead(_35);                // scope 4 at idna/src/uts46.rs:308:44: 308:45
        StorageLive(_36);                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        ConstEvalCounter;                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _36 = panic(const "ASCII deny list must not contain digits.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb19: {
        Coverage::VirtualCounter(bcb10); // scope 4 at no-location
        StorageDead(_35);                // scope 4 at idna/src/uts46.rs:308:44: 308:45
        goto -> bb20;                    // scope 4 at no-location
    }

    bb20: {
        Coverage::VirtualCounter(bcb11); // scope 4 at idna/src/uts46.rs:312:19: 312:30
        _31 = const ();                  // scope 4 at idna/src/uts46.rs:310:14: 310:14
        StorageDead(_34);                // scope 4 at idna/src/uts46.rs:310:13: 310:14
        StorageDead(_32);                // scope 4 at idna/src/uts46.rs:310:13: 310:14
        StorageDead(_31);                // scope 4 at idna/src/uts46.rs:310:13: 310:14
        StorageLive(_37);                // scope 4 at idna/src/uts46.rs:311:13: 314:14
        StorageLive(_38);                // scope 4 at idna/src/uts46.rs:312:19: 312:30
        StorageLive(_39);                // scope 4 at idna/src/uts46.rs:312:20: 312:21
        _39 = copy _15;                  // scope 4 at idna/src/uts46.rs:312:20: 312:21
        _38 = Ge(move _39, const 97_u8); // scope 4 at idna/src/uts46.rs:312:19: 312:30
        switchInt(move _38) -> [0: bb22, otherwise: bb21]; // scope 4 at idna/src/uts46.rs:312:19: 312:30
    }

    bb21: {
        Coverage::VirtualCounter(bcb12); // scope 4 at idna/src/uts46.rs:312:34: 312:45
        StorageDead(_39);                // scope 4 at idna/src/uts46.rs:312:29: 312:30
        StorageLive(_40);                // scope 4 at idna/src/uts46.rs:312:34: 312:45
        StorageLive(_41);                // scope 4 at idna/src/uts46.rs:312:35: 312:36
        _41 = copy _15;                  // scope 4 at idna/src/uts46.rs:312:35: 312:36
        _40 = Le(move _41, const 122_u8); // scope 4 at idna/src/uts46.rs:312:34: 312:45
        switchInt(move _40) -> [0: bb24, otherwise: bb23]; // scope 4 at idna/src/uts46.rs:312:34: 312:45
    }

    bb22: {
        Coverage::VirtualCounter(bcb21); // scope 4 at no-location
        StorageDead(_39);                // scope 4 at idna/src/uts46.rs:312:29: 312:30
        goto -> bb25;                    // scope 4 at no-location
    }

    bb23: {
        Coverage::VirtualCounter(bcb13); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        StorageDead(_41);                // scope 4 at idna/src/uts46.rs:312:44: 312:45
        StorageLive(_42);                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        ConstEvalCounter;                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _42 = panic(const "ASCII deny list must not contain letters.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb24: {
        Coverage::VirtualCounter(bcb14); // scope 4 at no-location
        StorageDead(_41);                // scope 4 at idna/src/uts46.rs:312:44: 312:45
        goto -> bb25;                    // scope 4 at no-location
    }

    bb25: {
        Coverage::VirtualCounter(bcb15); // scope 4 at idna/src/uts46.rs:316:19: 316:30
        _37 = const ();                  // scope 4 at idna/src/uts46.rs:314:14: 314:14
        StorageDead(_40);                // scope 4 at idna/src/uts46.rs:314:13: 314:14
        StorageDead(_38);                // scope 4 at idna/src/uts46.rs:314:13: 314:14
        StorageDead(_37);                // scope 4 at idna/src/uts46.rs:314:13: 314:14
        StorageLive(_43);                // scope 4 at idna/src/uts46.rs:315:13: 318:14
        StorageLive(_44);                // scope 4 at idna/src/uts46.rs:316:19: 316:30
        StorageLive(_45);                // scope 4 at idna/src/uts46.rs:316:20: 316:21
        _45 = copy _15;                  // scope 4 at idna/src/uts46.rs:316:20: 316:21
        _44 = Ge(move _45, const 65_u8); // scope 4 at idna/src/uts46.rs:316:19: 316:30
        switchInt(move _44) -> [0: bb27, otherwise: bb26]; // scope 4 at idna/src/uts46.rs:316:19: 316:30
    }

    bb26: {
        Coverage::VirtualCounter(bcb16); // scope 4 at idna/src/uts46.rs:316:34: 316:45
        StorageDead(_45);                // scope 4 at idna/src/uts46.rs:316:29: 316:30
        StorageLive(_46);                // scope 4 at idna/src/uts46.rs:316:34: 316:45
        StorageLive(_47);                // scope 4 at idna/src/uts46.rs:316:35: 316:36
        _47 = copy _15;                  // scope 4 at idna/src/uts46.rs:316:35: 316:36
        _46 = Le(move _47, const 90_u8); // scope 4 at idna/src/uts46.rs:316:34: 316:45
        switchInt(move _46) -> [0: bb29, otherwise: bb28]; // scope 4 at idna/src/uts46.rs:316:34: 316:45
    }

    bb27: {
        Coverage::VirtualCounter(bcb20); // scope 4 at no-location
        StorageDead(_45);                // scope 4 at idna/src/uts46.rs:316:29: 316:30
        goto -> bb30;                    // scope 4 at no-location
    }

    bb28: {
        Coverage::VirtualCounter(bcb17); // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        StorageDead(_47);                // scope 4 at idna/src/uts46.rs:316:44: 316:45
        StorageLive(_48);                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        ConstEvalCounter;                // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
        _48 = panic(const "ASCII deny list must not contain letters.") -> unwind continue; // scope 4 at /rustc/92ffebeceaa0a071066f233873c2438e73d70fe7/library/core/src/panic.rs:28:9: 28:39
    }

    bb29: {
        Coverage::VirtualCounter(bcb18); // scope 4 at no-location
        StorageDead(_47);                // scope 4 at idna/src/uts46.rs:316:44: 316:45
        goto -> bb30;                    // scope 4 at no-location
    }

    bb30: {
        Coverage::VirtualCounter(bcb19); // scope 4 at idna/src/uts46.rs:319:21: 319:31
        _43 = const ();                  // scope 4 at idna/src/uts46.rs:318:14: 318:14
        StorageDead(_46);                // scope 4 at idna/src/uts46.rs:318:13: 318:14
        StorageDead(_44);                // scope 4 at idna/src/uts46.rs:318:13: 318:14
        StorageDead(_43);                // scope 4 at idna/src/uts46.rs:318:13: 318:14
        StorageLive(_49);                // scope 4 at idna/src/uts46.rs:319:21: 319:31
        StorageLive(_50);                // scope 4 at idna/src/uts46.rs:319:30: 319:31
        _50 = copy _15;                  // scope 4 at idna/src/uts46.rs:319:30: 319:31
        _51 = Lt(copy _50, const 128_u8); // scope 4 at idna/src/uts46.rs:319:21: 319:31
        assert(move _51, "attempt to shift left by `{}`, which would overflow", copy _50) -> [success: bb31, unwind continue]; // scope 4 at idna/src/uts46.rs:319:21: 319:31
    }

    bb31: {
        _49 = Shl(const 1_u128, move _50); // scope 4 at idna/src/uts46.rs:319:21: 319:31
        StorageDead(_50);                // scope 4 at idna/src/uts46.rs:319:30: 319:31
        _3 = BitOr(copy _3, move _49);   // scope 4 at idna/src/uts46.rs:319:13: 319:31
        StorageDead(_49);                // scope 4 at idna/src/uts46.rs:319:30: 319:31
        _52 = AddWithOverflow(copy _6, const 1_usize); // scope 4 at idna/src/uts46.rs:320:13: 320:19
        assert(!move (_52.1: bool), "attempt to compute `{} + {}`, which would overflow", copy _6, const 1_usize) -> [success: bb32, unwind continue]; // scope 4 at idna/src/uts46.rs:320:13: 320:19
    }

    bb32: {
        _6 = move (_52.0: usize);        // scope 4 at idna/src/uts46.rs:320:13: 320:19
        _10 = const ();                  // scope 3 at idna/src/uts46.rs:301:31: 321:10
        StorageDead(_15);                // scope 3 at idna/src/uts46.rs:321:9: 321:10
        StorageDead(_11);                // scope 3 at idna/src/uts46.rs:321:9: 321:10
        ConstEvalCounter;                // scope 3 at idna/src/uts46.rs:301:9: 321:10
        goto -> bb5;                     // scope 3 at idna/src/uts46.rs:301:9: 321:10
    }
}
"#;

    #[test]
    fn ctfe_body_is_never_a_candidate() {
        let mir = crate::mir::parse_text(ASCII_DENY_LIST_NEW_MIR).expect("MIR parses");
        assert_eq!(mir.functions.len(), 2, "runtime and CTFE bodies");
        let (runtime, ctfe) = (&mir.functions[0], &mir.functions[1]);
        assert_eq!(runtime.name, ctfe.name);
        assert!(!runtime.is_ctfe);
        assert!(ctfe.is_ctfe);
        // The CTFE body has a decision shape of its own; it is
        // declined before decomposition.
        assert_eq!(try_decompose_function(ctfe), Ok(cond("deny_glyphless")));
        assert_eq!(try_decompose_candidate(ctfe), Err(DeclineReason::CtfeBody));
        assert_eq!(
            serde_json::to_value(DeclineReason::CtfeBody).unwrap(),
            "ctfe_body"
        );
        assert!(try_decompose_candidate(runtime).is_err());
        assert_ne!(
            try_decompose_candidate(runtime),
            Err(DeclineReason::CtfeBody)
        );
    }

    /// `cargo floyd test` MIR (rustc nightly 1.101, coverage flags,
    /// comment lines stripped) for uuid 1.x `format_simple` in
    /// `src/fmt.rs`, runtime body then `MIR FOR CTFE` body:
    ///
    /// ```text
    /// const fn format_simple(src: &[u8; 16], upper: bool) -> [u8; 32] {
    ///     let alpha_offset = if upper { 0x07 } else { 0x27 };
    ///     let mut dst = [0; 32];
    ///     let mut i = 0;
    ///     while i < 16 { .. i += 1; }
    ///     dst
    /// }
    /// ```
    const FORMAT_SIMPLE_MIR: &str = r#"
fn format_simple(_1: &[u8; 16], _2: bool) -> [u8; 32] {
    debug src => _1;                     // in scope 0 at src/fmt.rs:190:24: 190:27
    debug upper => _2;                   // in scope 0 at src/fmt.rs:190:40: 190:45
    let mut _0: [u8; 32];                // return place in scope 0 at src/fmt.rs:190:56: 190:64
    let _3: u8;                          // in scope 0 at src/fmt.rs:191:9: 191:21
    let mut _6: bool;                    // in scope 0 at src/fmt.rs:194:11: 194:17
    let mut _7: usize;                   // in scope 0 at src/fmt.rs:194:11: 194:12
    let _9: usize;                       // in scope 0 at src/fmt.rs:195:21: 195:22
    let mut _10: bool;                   // in scope 0 at src/fmt.rs:195:17: 195:23
    let mut _11: u8;                     // in scope 0 at src/fmt.rs:196:22: 196:57
    let mut _12: u8;                     // in scope 0 at src/fmt.rs:196:36: 196:42
    let mut _13: u32;                    // in scope 0 at src/fmt.rs:196:36: 196:42
    let mut _14: bool;                   // in scope 0 at src/fmt.rs:196:36: 196:42
    let mut _15: u8;                     // in scope 0 at src/fmt.rs:196:44: 196:56
    let _16: usize;                      // in scope 0 at src/fmt.rs:196:13: 196:18
    let mut _17: usize;                  // in scope 0 at src/fmt.rs:196:13: 196:14
    let mut _18: (usize, bool);          // in scope 0 at src/fmt.rs:196:13: 196:18
    let mut _19: bool;                   // in scope 0 at src/fmt.rs:196:9: 196:19
    let mut _20: u8;                     // in scope 0 at src/fmt.rs:197:26: 197:63
    let mut _21: u8;                     // in scope 0 at src/fmt.rs:197:40: 197:48
    let mut _22: u8;                     // in scope 0 at src/fmt.rs:197:50: 197:62
    let _23: usize;                      // in scope 0 at src/fmt.rs:197:13: 197:22
    let mut _24: usize;                  // in scope 0 at src/fmt.rs:197:13: 197:18
    let mut _25: usize;                  // in scope 0 at src/fmt.rs:197:13: 197:14
    let mut _26: (usize, bool);          // in scope 0 at src/fmt.rs:197:13: 197:18
    let mut _27: (usize, bool);          // in scope 0 at src/fmt.rs:197:13: 197:22
    let mut _28: bool;                   // in scope 0 at src/fmt.rs:197:9: 197:23
    let mut _29: (usize, bool);          // in scope 0 at src/fmt.rs:198:9: 198:15
    scope 1 {
        debug alpha_offset => _3;        // in scope 1 at src/fmt.rs:191:9: 191:21
        let mut _4: [u8; 32];            // in scope 1 at src/fmt.rs:192:9: 192:16
        scope 2 {
            debug dst => _4;             // in scope 2 at src/fmt.rs:192:9: 192:16
            let mut _5: usize;           // in scope 2 at src/fmt.rs:193:9: 193:14
            scope 3 {
                debug i => _5;           // in scope 3 at src/fmt.rs:193:9: 193:14
                let _8: u8;              // in scope 3 at src/fmt.rs:195:13: 195:14
                scope 4 {
                    debug x => _8;       // in scope 4 at src/fmt.rs:195:13: 195:14
                }
            }
        }
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/fmt.rs:191:27: 191:32 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => src/fmt.rs:194:11: 194:17 (#0)

    coverage Code { bcb: bcb0 } => src/fmt.rs:190:1: 190:64 (#0);
    coverage Code { bcb: bcb0 } => src/fmt.rs:191:27: 191:32 (#0);
    coverage Code { bcb: bcb1 } => src/fmt.rs:191:35: 191:39 (#0);
    coverage Code { bcb: bcb6 } => src/fmt.rs:191:49: 191:53 (#0);
    coverage Code { bcb: bcb2 } => src/fmt.rs:192:19: 192:26 (#0);
    coverage Code { bcb: bcb2 } => src/fmt.rs:193:17: 193:18 (#0);
    coverage Code { bcb: bcb3 } => src/fmt.rs:194:11: 194:17 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:195:17: 195:23 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:196:9: 196:12 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:196:9: 196:19 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:196:22: 196:35 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:196:9: 196:57 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:197:9: 197:12 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:197:9: 197:23 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:197:26: 197:39 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:197:9: 197:63 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:198:9: 198:15 (#0);
    coverage Code { bcb: bcb5 } => src/fmt.rs:200:5: 200:8 (#0);
    coverage Code { bcb: bcb5 } => src/fmt.rs:201:1: 201:2 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb6 } => src/fmt.rs:191:27: 191:32 (#0);
    coverage Branch { true_bcb: bcb4, false_bcb: bcb5 } => src/fmt.rs:194:11: 194:17 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/fmt.rs:191:27: 191:32
        switchInt(copy _2) -> [0: bb2, otherwise: bb1]; // scope 0 at src/fmt.rs:191:27: 191:32
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/fmt.rs:191:24: 191:55
        _3 = const 7_u8;                 // scope 0 at src/fmt.rs:191:35: 191:39
        goto -> bb3;                     // scope 0 at src/fmt.rs:191:24: 191:55
    }

    bb2: {
        Coverage::VirtualCounter(bcb6);  // scope 0 at src/fmt.rs:191:24: 191:55
        _3 = const 39_u8;                // scope 0 at src/fmt.rs:191:49: 191:53
        goto -> bb3;                     // scope 0 at src/fmt.rs:191:24: 191:55
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 3 at src/fmt.rs:194:5: 199:6
        _4 = [const 0_u8; 32];           // scope 1 at src/fmt.rs:192:19: 192:26
        _5 = const 0_usize;              // scope 2 at src/fmt.rs:193:17: 193:18
        goto -> bb4;                     // scope 3 at src/fmt.rs:194:5: 199:6
    }

    bb4: {
        Coverage::VirtualCounter(bcb3);  // scope 3 at src/fmt.rs:194:5: 199:6
        _7 = copy _5;                    // scope 3 at src/fmt.rs:194:11: 194:12
        _6 = Lt(move _7, const 16_usize); // scope 3 at src/fmt.rs:194:11: 194:17
        switchInt(move _6) -> [0: bb6, otherwise: bb5]; // scope 3 at src/fmt.rs:194:11: 194:17
    }

    bb5: {
        Coverage::VirtualCounter(bcb4);  // scope 3 at src/fmt.rs:195:17: 195:23
        _9 = copy _5;                    // scope 3 at src/fmt.rs:195:21: 195:22
        _10 = Lt(copy _9, const 16_usize); // scope 3 at src/fmt.rs:195:17: 195:23
        assert(move _10, "index out of bounds: the length is {} but the index is {}", const 16_usize, copy _9) -> [success: bb7, unwind continue]; // scope 3 at src/fmt.rs:195:17: 195:23
    }

    bb6: {
        Coverage::VirtualCounter(bcb5);  // scope 0 at src/fmt.rs:201:2: 201:2
        _0 = copy _4;                    // scope 3 at src/fmt.rs:200:5: 200:8
        return;                          // scope 0 at src/fmt.rs:201:2: 201:2
    }

    bb7: {
        _8 = copy (*_1)[_9];             // scope 3 at src/fmt.rs:195:17: 195:23
        _13 = const 4_i32 as u32 (IntToInt); // scope 4 at src/fmt.rs:196:36: 196:42
        _14 = Lt(move _13, const 8_u32); // scope 4 at src/fmt.rs:196:36: 196:42
        assert(move _14, "attempt to shift right by `{}`, which would overflow", const 4_i32) -> [success: bb8, unwind continue]; // scope 4 at src/fmt.rs:196:36: 196:42
    }

    bb8: {
        _12 = Shr(copy _8, const 4_i32); // scope 4 at src/fmt.rs:196:36: 196:42
        _15 = copy _3;                   // scope 4 at src/fmt.rs:196:44: 196:56
        _11 = nibble_to_hex(move _12, move _15) -> [return: bb9, unwind continue]; // scope 4 at src/fmt.rs:196:22: 196:57
    }

    bb9: {
        _17 = copy _5;                   // scope 4 at src/fmt.rs:196:13: 196:14
        _18 = MulWithOverflow(copy _17, const 2_usize); // scope 4 at src/fmt.rs:196:13: 196:18
        assert(!move (_18.1: bool), "attempt to compute `{} * {}`, which would overflow", move _17, const 2_usize) -> [success: bb10, unwind continue]; // scope 4 at src/fmt.rs:196:13: 196:18
    }

    bb10: {
        _16 = move (_18.0: usize);       // scope 4 at src/fmt.rs:196:13: 196:18
        _19 = Lt(copy _16, const 32_usize); // scope 4 at src/fmt.rs:196:9: 196:19
        assert(move _19, "index out of bounds: the length is {} but the index is {}", const 32_usize, copy _16) -> [success: bb11, unwind continue]; // scope 4 at src/fmt.rs:196:9: 196:19
    }

    bb11: {
        _4[_16] = move _11;              // scope 4 at src/fmt.rs:196:9: 196:57
        _21 = BitAnd(copy _8, const 15_u8); // scope 4 at src/fmt.rs:197:40: 197:48
        _22 = copy _3;                   // scope 4 at src/fmt.rs:197:50: 197:62
        _20 = nibble_to_hex(move _21, move _22) -> [return: bb12, unwind continue]; // scope 4 at src/fmt.rs:197:26: 197:63
    }

    bb12: {
        _25 = copy _5;                   // scope 4 at src/fmt.rs:197:13: 197:14
        _26 = MulWithOverflow(copy _25, const 2_usize); // scope 4 at src/fmt.rs:197:13: 197:18
        assert(!move (_26.1: bool), "attempt to compute `{} * {}`, which would overflow", move _25, const 2_usize) -> [success: bb13, unwind continue]; // scope 4 at src/fmt.rs:197:13: 197:18
    }

    bb13: {
        _24 = move (_26.0: usize);       // scope 4 at src/fmt.rs:197:13: 197:18
        _27 = AddWithOverflow(copy _24, const 1_usize); // scope 4 at src/fmt.rs:197:13: 197:22
        assert(!move (_27.1: bool), "attempt to compute `{} + {}`, which would overflow", move _24, const 1_usize) -> [success: bb14, unwind continue]; // scope 4 at src/fmt.rs:197:13: 197:22
    }

    bb14: {
        _23 = move (_27.0: usize);       // scope 4 at src/fmt.rs:197:13: 197:22
        _28 = Lt(copy _23, const 32_usize); // scope 4 at src/fmt.rs:197:9: 197:23
        assert(move _28, "index out of bounds: the length is {} but the index is {}", const 32_usize, copy _23) -> [success: bb15, unwind continue]; // scope 4 at src/fmt.rs:197:9: 197:23
    }

    bb15: {
        _4[_23] = move _20;              // scope 4 at src/fmt.rs:197:9: 197:63
        _29 = AddWithOverflow(copy _5, const 1_usize); // scope 4 at src/fmt.rs:198:9: 198:15
        assert(!move (_29.1: bool), "attempt to compute `{} + {}`, which would overflow", copy _5, const 1_usize) -> [success: bb16, unwind continue]; // scope 4 at src/fmt.rs:198:9: 198:15
    }

    bb16: {
        _5 = move (_29.0: usize);        // scope 4 at src/fmt.rs:198:9: 198:15
        goto -> bb4;                     // scope 3 at src/fmt.rs:194:5: 199:6
    }
}

// MIR FOR CTFE
fn format_simple(_1: &[u8; 16], _2: bool) -> [u8; 32] {
    debug src => _1;                     // in scope 0 at src/fmt.rs:190:24: 190:27
    debug upper => _2;                   // in scope 0 at src/fmt.rs:190:40: 190:45
    let mut _0: [u8; 32];                // return place in scope 0 at src/fmt.rs:190:56: 190:64
    let _3: u8;                          // in scope 0 at src/fmt.rs:191:9: 191:21
    let mut _4: bool;                    // in scope 0 at src/fmt.rs:191:27: 191:32
    let _7: ();                          // in scope 0 at src/fmt.rs:194:5: 199:6
    let mut _8: ();                      // in scope 0 at src/fmt.rs:190:1: 201:2
    let mut _9: bool;                    // in scope 0 at src/fmt.rs:194:11: 194:17
    let mut _10: usize;                  // in scope 0 at src/fmt.rs:194:11: 194:12
    let _12: usize;                      // in scope 0 at src/fmt.rs:195:21: 195:22
    let mut _13: bool;                   // in scope 0 at src/fmt.rs:195:17: 195:23
    let mut _14: u8;                     // in scope 0 at src/fmt.rs:196:22: 196:57
    let mut _15: u8;                     // in scope 0 at src/fmt.rs:196:36: 196:42
    let mut _16: u8;                     // in scope 0 at src/fmt.rs:196:36: 196:37
    let mut _17: u32;                    // in scope 0 at src/fmt.rs:196:36: 196:42
    let mut _18: bool;                   // in scope 0 at src/fmt.rs:196:36: 196:42
    let mut _19: u8;                     // in scope 0 at src/fmt.rs:196:44: 196:56
    let _20: usize;                      // in scope 0 at src/fmt.rs:196:13: 196:18
    let mut _21: usize;                  // in scope 0 at src/fmt.rs:196:13: 196:14
    let mut _22: (usize, bool);          // in scope 0 at src/fmt.rs:196:13: 196:18
    let mut _23: bool;                   // in scope 0 at src/fmt.rs:196:9: 196:19
    let mut _24: u8;                     // in scope 0 at src/fmt.rs:197:26: 197:63
    let mut _25: u8;                     // in scope 0 at src/fmt.rs:197:40: 197:48
    let mut _26: u8;                     // in scope 0 at src/fmt.rs:197:40: 197:41
    let mut _27: u8;                     // in scope 0 at src/fmt.rs:197:50: 197:62
    let _28: usize;                      // in scope 0 at src/fmt.rs:197:13: 197:22
    let mut _29: usize;                  // in scope 0 at src/fmt.rs:197:13: 197:18
    let mut _30: usize;                  // in scope 0 at src/fmt.rs:197:13: 197:14
    let mut _31: (usize, bool);          // in scope 0 at src/fmt.rs:197:13: 197:18
    let mut _32: (usize, bool);          // in scope 0 at src/fmt.rs:197:13: 197:22
    let mut _33: bool;                   // in scope 0 at src/fmt.rs:197:9: 197:23
    let mut _34: (usize, bool);          // in scope 0 at src/fmt.rs:198:9: 198:15
    let mut _35: !;                      // in scope 0 at src/fmt.rs:194:5: 199:6
    let _36: ();                         // in scope 0 at src/fmt.rs:194:5: 199:6
    let mut _37: !;                      // in scope 0 at src/fmt.rs:194:5: 199:6
    scope 1 {
        debug alpha_offset => _3;        // in scope 1 at src/fmt.rs:191:9: 191:21
        let mut _5: [u8; 32];            // in scope 1 at src/fmt.rs:192:9: 192:16
        scope 2 {
            debug dst => _5;             // in scope 2 at src/fmt.rs:192:9: 192:16
            let mut _6: usize;           // in scope 2 at src/fmt.rs:193:9: 193:14
            scope 3 {
                debug i => _6;           // in scope 3 at src/fmt.rs:193:9: 193:14
                let _11: u8;             // in scope 3 at src/fmt.rs:195:13: 195:14
                scope 4 {
                    debug x => _11;      // in scope 4 at src/fmt.rs:195:13: 195:14
                }
            }
        }
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/fmt.rs:191:27: 191:32 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => src/fmt.rs:194:11: 194:17 (#0)

    coverage Code { bcb: bcb0 } => src/fmt.rs:190:1: 190:64 (#0);
    coverage Code { bcb: bcb0 } => src/fmt.rs:191:27: 191:32 (#0);
    coverage Code { bcb: bcb1 } => src/fmt.rs:191:35: 191:39 (#0);
    coverage Code { bcb: bcb6 } => src/fmt.rs:191:49: 191:53 (#0);
    coverage Code { bcb: bcb2 } => src/fmt.rs:192:19: 192:26 (#0);
    coverage Code { bcb: bcb2 } => src/fmt.rs:193:17: 193:18 (#0);
    coverage Code { bcb: bcb3 } => src/fmt.rs:194:11: 194:17 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:195:17: 195:23 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:196:9: 196:12 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:196:9: 196:19 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:196:22: 196:35 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:196:9: 196:57 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:197:9: 197:12 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:197:9: 197:23 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:197:26: 197:39 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:197:9: 197:63 (#0);
    coverage Code { bcb: bcb4 } => src/fmt.rs:198:9: 198:15 (#0);
    coverage Code { bcb: bcb5 } => src/fmt.rs:200:5: 200:8 (#0);
    coverage Code { bcb: bcb5 } => src/fmt.rs:201:1: 201:2 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb6 } => src/fmt.rs:191:27: 191:32 (#0);
    coverage Branch { true_bcb: bcb4, false_bcb: bcb5 } => src/fmt.rs:194:11: 194:17 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at src/fmt.rs:191:27: 191:32
        StorageLive(_3);                 // scope 0 at src/fmt.rs:191:9: 191:21
        StorageLive(_4);                 // scope 0 at src/fmt.rs:191:27: 191:32
        _4 = copy _2;                    // scope 0 at src/fmt.rs:191:27: 191:32
        switchInt(move _4) -> [0: bb2, otherwise: bb1]; // scope 0 at src/fmt.rs:191:27: 191:32
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at src/fmt.rs:191:24: 191:55
        _3 = const 7_u8;                 // scope 0 at src/fmt.rs:191:35: 191:39
        goto -> bb3;                     // scope 0 at src/fmt.rs:191:24: 191:55
    }

    bb2: {
        Coverage::VirtualCounter(bcb6);  // scope 0 at src/fmt.rs:191:24: 191:55
        _3 = const 39_u8;                // scope 0 at src/fmt.rs:191:49: 191:53
        goto -> bb3;                     // scope 0 at src/fmt.rs:191:24: 191:55
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 3 at src/fmt.rs:194:5: 199:6
        StorageDead(_4);                 // scope 0 at src/fmt.rs:191:54: 191:55
        StorageLive(_5);                 // scope 1 at src/fmt.rs:192:9: 192:16
        _5 = [const 0_u8; 32];           // scope 1 at src/fmt.rs:192:19: 192:26
        StorageLive(_6);                 // scope 2 at src/fmt.rs:193:9: 193:14
        _6 = const 0_usize;              // scope 2 at src/fmt.rs:193:17: 193:18
        StorageLive(_7);                 // scope 3 at src/fmt.rs:194:5: 199:6
        goto -> bb4;                     // scope 3 at src/fmt.rs:194:5: 199:6
    }

    bb4: {
        Coverage::VirtualCounter(bcb3);  // scope 3 at src/fmt.rs:194:5: 199:6
        StorageLive(_9);                 // scope 3 at src/fmt.rs:194:11: 194:17
        StorageLive(_10);                // scope 3 at src/fmt.rs:194:11: 194:12
        _10 = copy _6;                   // scope 3 at src/fmt.rs:194:11: 194:12
        _9 = Lt(move _10, const 16_usize); // scope 3 at src/fmt.rs:194:11: 194:17
        switchInt(move _9) -> [0: bb6, otherwise: bb5]; // scope 3 at src/fmt.rs:194:11: 194:17
    }

    bb5: {
        Coverage::VirtualCounter(bcb4);  // scope 3 at src/fmt.rs:195:17: 195:23
        StorageDead(_10);                // scope 3 at src/fmt.rs:194:16: 194:17
        StorageLive(_11);                // scope 3 at src/fmt.rs:195:13: 195:14
        StorageLive(_12);                // scope 3 at src/fmt.rs:195:21: 195:22
        _12 = copy _6;                   // scope 3 at src/fmt.rs:195:21: 195:22
        _13 = Lt(copy _12, const 16_usize); // scope 3 at src/fmt.rs:195:17: 195:23
        assert(move _13, "index out of bounds: the length is {} but the index is {}", const 16_usize, copy _12) -> [success: bb7, unwind continue]; // scope 3 at src/fmt.rs:195:17: 195:23
    }

    bb6: {
        Coverage::VirtualCounter(bcb5);  // scope 0 at src/fmt.rs:201:2: 201:2
        StorageDead(_10);                // scope 3 at src/fmt.rs:194:16: 194:17
        StorageLive(_36);                // scope 3 at src/fmt.rs:194:5: 199:6
        _7 = const ();                   // scope 3 at src/fmt.rs:194:5: 199:6
        StorageDead(_36);                // scope 3 at src/fmt.rs:199:5: 199:6
        StorageDead(_9);                 // scope 3 at src/fmt.rs:199:5: 199:6
        StorageDead(_7);                 // scope 3 at src/fmt.rs:199:5: 199:6
        _0 = copy _5;                    // scope 3 at src/fmt.rs:200:5: 200:8
        StorageDead(_6);                 // scope 2 at src/fmt.rs:201:1: 201:2
        StorageDead(_5);                 // scope 1 at src/fmt.rs:201:1: 201:2
        StorageDead(_3);                 // scope 0 at src/fmt.rs:201:1: 201:2
        return;                          // scope 0 at src/fmt.rs:201:2: 201:2
    }

    bb7: {
        _11 = copy (*_1)[_12];           // scope 3 at src/fmt.rs:195:17: 195:23
        StorageDead(_12);                // scope 3 at src/fmt.rs:195:23: 195:24
        StorageLive(_14);                // scope 4 at src/fmt.rs:196:22: 196:57
        StorageLive(_15);                // scope 4 at src/fmt.rs:196:36: 196:42
        StorageLive(_16);                // scope 4 at src/fmt.rs:196:36: 196:37
        _16 = copy _11;                  // scope 4 at src/fmt.rs:196:36: 196:37
        _17 = const 4_i32 as u32 (IntToInt); // scope 4 at src/fmt.rs:196:36: 196:42
        _18 = Lt(move _17, const 8_u32); // scope 4 at src/fmt.rs:196:36: 196:42
        assert(move _18, "attempt to shift right by `{}`, which would overflow", const 4_i32) -> [success: bb8, unwind continue]; // scope 4 at src/fmt.rs:196:36: 196:42
    }

    bb8: {
        _15 = Shr(move _16, const 4_i32); // scope 4 at src/fmt.rs:196:36: 196:42
        StorageDead(_16);                // scope 4 at src/fmt.rs:196:41: 196:42
        StorageLive(_19);                // scope 4 at src/fmt.rs:196:44: 196:56
        _19 = copy _3;                   // scope 4 at src/fmt.rs:196:44: 196:56
        ConstEvalCounter;                // scope 4 at src/fmt.rs:196:22: 196:57
        _14 = nibble_to_hex(move _15, move _19) -> [return: bb9, unwind continue]; // scope 4 at src/fmt.rs:196:22: 196:57
    }

    bb9: {
        StorageDead(_19);                // scope 4 at src/fmt.rs:196:56: 196:57
        StorageDead(_15);                // scope 4 at src/fmt.rs:196:56: 196:57
        StorageLive(_20);                // scope 4 at src/fmt.rs:196:13: 196:18
        StorageLive(_21);                // scope 4 at src/fmt.rs:196:13: 196:14
        _21 = copy _6;                   // scope 4 at src/fmt.rs:196:13: 196:14
        _22 = MulWithOverflow(copy _21, const 2_usize); // scope 4 at src/fmt.rs:196:13: 196:18
        assert(!move (_22.1: bool), "attempt to compute `{} * {}`, which would overflow", move _21, const 2_usize) -> [success: bb10, unwind continue]; // scope 4 at src/fmt.rs:196:13: 196:18
    }

    bb10: {
        _20 = move (_22.0: usize);       // scope 4 at src/fmt.rs:196:13: 196:18
        StorageDead(_21);                // scope 4 at src/fmt.rs:196:17: 196:18
        _23 = Lt(copy _20, const 32_usize); // scope 4 at src/fmt.rs:196:9: 196:19
        assert(move _23, "index out of bounds: the length is {} but the index is {}", const 32_usize, copy _20) -> [success: bb11, unwind continue]; // scope 4 at src/fmt.rs:196:9: 196:19
    }

    bb11: {
        _5[_20] = move _14;              // scope 4 at src/fmt.rs:196:9: 196:57
        StorageDead(_14);                // scope 4 at src/fmt.rs:196:56: 196:57
        StorageDead(_20);                // scope 4 at src/fmt.rs:196:57: 196:58
        StorageLive(_24);                // scope 4 at src/fmt.rs:197:26: 197:63
        StorageLive(_25);                // scope 4 at src/fmt.rs:197:40: 197:48
        StorageLive(_26);                // scope 4 at src/fmt.rs:197:40: 197:41
        _26 = copy _11;                  // scope 4 at src/fmt.rs:197:40: 197:41
        _25 = BitAnd(move _26, const 15_u8); // scope 4 at src/fmt.rs:197:40: 197:48
        StorageDead(_26);                // scope 4 at src/fmt.rs:197:47: 197:48
        StorageLive(_27);                // scope 4 at src/fmt.rs:197:50: 197:62
        _27 = copy _3;                   // scope 4 at src/fmt.rs:197:50: 197:62
        ConstEvalCounter;                // scope 4 at src/fmt.rs:197:26: 197:63
        _24 = nibble_to_hex(move _25, move _27) -> [return: bb12, unwind continue]; // scope 4 at src/fmt.rs:197:26: 197:63
    }

    bb12: {
        StorageDead(_27);                // scope 4 at src/fmt.rs:197:62: 197:63
        StorageDead(_25);                // scope 4 at src/fmt.rs:197:62: 197:63
        StorageLive(_28);                // scope 4 at src/fmt.rs:197:13: 197:22
        StorageLive(_29);                // scope 4 at src/fmt.rs:197:13: 197:18
        StorageLive(_30);                // scope 4 at src/fmt.rs:197:13: 197:14
        _30 = copy _6;                   // scope 4 at src/fmt.rs:197:13: 197:14
        _31 = MulWithOverflow(copy _30, const 2_usize); // scope 4 at src/fmt.rs:197:13: 197:18
        assert(!move (_31.1: bool), "attempt to compute `{} * {}`, which would overflow", move _30, const 2_usize) -> [success: bb13, unwind continue]; // scope 4 at src/fmt.rs:197:13: 197:18
    }

    bb13: {
        _29 = move (_31.0: usize);       // scope 4 at src/fmt.rs:197:13: 197:18
        StorageDead(_30);                // scope 4 at src/fmt.rs:197:17: 197:18
        _32 = AddWithOverflow(copy _29, const 1_usize); // scope 4 at src/fmt.rs:197:13: 197:22
        assert(!move (_32.1: bool), "attempt to compute `{} + {}`, which would overflow", move _29, const 1_usize) -> [success: bb14, unwind continue]; // scope 4 at src/fmt.rs:197:13: 197:22
    }

    bb14: {
        _28 = move (_32.0: usize);       // scope 4 at src/fmt.rs:197:13: 197:22
        StorageDead(_29);                // scope 4 at src/fmt.rs:197:21: 197:22
        _33 = Lt(copy _28, const 32_usize); // scope 4 at src/fmt.rs:197:9: 197:23
        assert(move _33, "index out of bounds: the length is {} but the index is {}", const 32_usize, copy _28) -> [success: bb15, unwind continue]; // scope 4 at src/fmt.rs:197:9: 197:23
    }

    bb15: {
        _5[_28] = move _24;              // scope 4 at src/fmt.rs:197:9: 197:63
        StorageDead(_24);                // scope 4 at src/fmt.rs:197:62: 197:63
        StorageDead(_28);                // scope 4 at src/fmt.rs:197:63: 197:64
        _34 = AddWithOverflow(copy _6, const 1_usize); // scope 4 at src/fmt.rs:198:9: 198:15
        assert(!move (_34.1: bool), "attempt to compute `{} + {}`, which would overflow", copy _6, const 1_usize) -> [success: bb16, unwind continue]; // scope 4 at src/fmt.rs:198:9: 198:15
    }

    bb16: {
        _6 = move (_34.0: usize);        // scope 4 at src/fmt.rs:198:9: 198:15
        _8 = const ();                   // scope 3 at src/fmt.rs:194:18: 199:6
        StorageDead(_11);                // scope 3 at src/fmt.rs:199:5: 199:6
        StorageDead(_9);                 // scope 3 at src/fmt.rs:199:5: 199:6
        ConstEvalCounter;                // scope 3 at src/fmt.rs:194:5: 199:6
        goto -> bb4;                     // scope 3 at src/fmt.rs:194:5: 199:6
    }
}

format_simple::{constant#0}: usize = {
    let mut _0: usize;                   // return place in scope 0 at src/fmt.rs:190:35: 190:37

    bb0: {
        _0 = const 16_usize;             // scope 0 at src/fmt.rs:190:35: 190:37
        return;                          // scope 0 at src/fmt.rs:190:35: 190:37
    }
}

format_simple::{constant#1}: usize = {
    let mut _0: usize;                   // return place in scope 0 at src/fmt.rs:190:61: 190:63

    bb0: {
        _0 = const 32_usize;             // scope 0 at src/fmt.rs:190:61: 190:63
        return;                          // scope 0 at src/fmt.rs:190:61: 190:63
    }
}

format_simple::{constant#2}: usize = {
    let mut _0: usize;                   // return place in scope 0 at src/fmt.rs:192:23: 192:25

    bb0: {
        _0 = const 32_usize;             // scope 0 at src/fmt.rs:192:23: 192:25
        return;                          // scope 0 at src/fmt.rs:192:23: 192:25
    }
}
"#;

    /// `rustc +nightly --emit=mir -Cinstrument-coverage
    /// -Zcoverage-options=branch,condition -Zmir-include-spans`
    /// (comment lines stripped) for:
    ///
    /// ```text
    /// pub fn nested(a: bool, b: bool) -> u8 {
    ///     if a { if b { 1 } else { 2 } } else { 3 }
    /// }
    /// pub fn spin(c: bool, n: u32) -> u32 {
    ///     let x = if c { 3 } else { let mut k = n; while k > 9 { k -= 1; } k };
    ///     x
    /// }
    /// pub fn split(c: bool) -> u32 {
    ///     let mut a = 0u32;
    ///     let mut b = 0u32;
    ///     if c { a = 5; } else { b = 7; }
    ///     a + b
    /// }
    /// ```
    const VALUE_ARM_DECLINES_MIR: &str = r#"
fn nested(_1: bool, _2: bool) -> u8 {
    debug a => _1;                       // in scope 0 at lift.rs:1:15: 1:16
    debug b => _2;                       // in scope 0 at lift.rs:1:24: 1:25
    let mut _0: u8;                      // return place in scope 0 at lift.rs:1:36: 1:38

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => lift.rs:2:8: 2:9 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => lift.rs:3:12: 3:13 (#0)

    coverage Code { bcb: bcb0 } => lift.rs:1:1: 1:38 (#0);
    coverage Code { bcb: bcb0 } => lift.rs:2:8: 2:9 (#0);
    coverage Code { bcb: bcb1 } => lift.rs:3:12: 3:13 (#0);
    coverage Code { bcb: bcb2 } => lift.rs:3:16: 3:17 (#0);
    coverage Code { bcb: bcb5 } => lift.rs:3:27: 3:28 (#0);
    coverage Code { bcb: bcb6 } => lift.rs:5:9: 5:10 (#0);
    coverage Code { bcb: bcb4 } => lift.rs:7:1: 7:2 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb6 } => lift.rs:2:8: 2:9 (#0);
    coverage Branch { true_bcb: bcb2, false_bcb: bcb5 } => lift.rs:3:12: 3:13 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at lift.rs:2:8: 2:9
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at lift.rs:2:8: 2:9
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at lift.rs:3:12: 3:13
        switchInt(copy _2) -> [0: bb4, otherwise: bb3]; // scope 0 at lift.rs:3:12: 3:13
    }

    bb2: {
        Coverage::VirtualCounter(bcb6);  // scope 0 at lift.rs:2:5: 6:6
        _0 = const 3_u8;                 // scope 0 at lift.rs:5:9: 5:10
        goto -> bb6;                     // scope 0 at lift.rs:2:5: 6:6
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at lift.rs:3:9: 3:30
        _0 = const 1_u8;                 // scope 0 at lift.rs:3:16: 3:17
        goto -> bb5;                     // scope 0 at lift.rs:3:9: 3:30
    }

    bb4: {
        Coverage::VirtualCounter(bcb5);  // scope 0 at lift.rs:3:9: 3:30
        _0 = const 2_u8;                 // scope 0 at lift.rs:3:27: 3:28
        goto -> bb5;                     // scope 0 at lift.rs:3:9: 3:30
    }

    bb5: {
        Coverage::VirtualCounter(bcb3);  // scope 0 at lift.rs:2:5: 6:6
        goto -> bb6;                     // scope 0 at lift.rs:2:5: 6:6
    }

    bb6: {
        Coverage::VirtualCounter(bcb4);  // scope 0 at lift.rs:7:2: 7:2
        return;                          // scope 0 at lift.rs:7:2: 7:2
    }
}

fn spin(_1: bool, _2: u32) -> u32 {
    debug c => _1;                       // in scope 0 at lift.rs:9:13: 9:14
    debug n => _2;                       // in scope 0 at lift.rs:9:22: 9:23
    let mut _0: u32;                     // return place in scope 0 at lift.rs:9:33: 9:36
    let _3: u32;                         // in scope 0 at lift.rs:10:9: 10:10
    let mut _4: u32;                     // in scope 0 at lift.rs:13:13: 13:18
    let mut _5: bool;                    // in scope 0 at lift.rs:14:15: 14:20
    let mut _6: u32;                     // in scope 0 at lift.rs:14:15: 14:16
    let mut _7: (u32, bool);             // in scope 0 at lift.rs:15:13: 15:19
    scope 1 {
        debug x => _3;                   // in scope 1 at lift.rs:10:9: 10:10
    }
    scope 2 {
        debug k => _4;                   // in scope 2 at lift.rs:13:13: 13:18
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => lift.rs:10:16: 10:17 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => lift.rs:14:15: 14:20 (#0)

    coverage Code { bcb: bcb0 } => lift.rs:9:1: 9:36 (#0);
    coverage Code { bcb: bcb0 } => lift.rs:10:16: 10:17 (#0);
    coverage Code { bcb: bcb1 } => lift.rs:11:9: 11:10 (#0);
    coverage Code { bcb: bcb3 } => lift.rs:13:21: 13:22 (#0);
    coverage Code { bcb: bcb4 } => lift.rs:14:15: 14:20 (#0);
    coverage Code { bcb: bcb5 } => lift.rs:15:13: 15:19 (#0);
    coverage Code { bcb: bcb6 } => lift.rs:17:9: 17:10 (#0);
    coverage Code { bcb: bcb2 } => lift.rs:19:5: 19:6 (#0);
    coverage Code { bcb: bcb2 } => lift.rs:20:1: 20:2 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => lift.rs:10:16: 10:17 (#0);
    coverage Branch { true_bcb: bcb5, false_bcb: bcb6 } => lift.rs:14:15: 14:20 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 0 at lift.rs:10:16: 10:17
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at lift.rs:10:16: 10:17
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 0 at lift.rs:10:13: 18:6
        _3 = const 3_u32;                // scope 0 at lift.rs:11:9: 11:10
        goto -> bb7;                     // scope 0 at lift.rs:10:13: 18:6
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 2 at lift.rs:14:9: 16:10
        _4 = copy _2;                    // scope 0 at lift.rs:13:21: 13:22
        goto -> bb3;                     // scope 2 at lift.rs:14:9: 16:10
    }

    bb3: {
        Coverage::VirtualCounter(bcb4);  // scope 2 at lift.rs:14:9: 16:10
        _6 = copy _4;                    // scope 2 at lift.rs:14:15: 14:16
        _5 = Gt(move _6, const 9_u32);   // scope 2 at lift.rs:14:15: 14:20
        switchInt(move _5) -> [0: bb5, otherwise: bb4]; // scope 2 at lift.rs:14:15: 14:20
    }

    bb4: {
        Coverage::VirtualCounter(bcb5);  // scope 2 at lift.rs:15:13: 15:19
        _7 = SubWithOverflow(copy _4, const 1_u32); // scope 2 at lift.rs:15:13: 15:19
        assert(!move (_7.1: bool), "attempt to compute `{} - {}`, which would overflow", copy _4, const 1_u32) -> [success: bb6, unwind continue]; // scope 2 at lift.rs:15:13: 15:19
    }

    bb5: {
        Coverage::VirtualCounter(bcb6);  // scope 0 at lift.rs:10:13: 18:6
        _3 = copy _4;                    // scope 2 at lift.rs:17:9: 17:10
        goto -> bb7;                     // scope 0 at lift.rs:10:13: 18:6
    }

    bb6: {
        _4 = move (_7.0: u32);           // scope 2 at lift.rs:15:13: 15:19
        goto -> bb3;                     // scope 2 at lift.rs:14:9: 16:10
    }

    bb7: {
        Coverage::VirtualCounter(bcb2);  // scope 0 at lift.rs:20:2: 20:2
        _0 = copy _3;                    // scope 1 at lift.rs:19:5: 19:6
        return;                          // scope 0 at lift.rs:20:2: 20:2
    }
}

fn split(_1: bool) -> u32 {
    debug c => _1;                       // in scope 0 at lift.rs:22:14: 22:15
    let mut _0: u32;                     // return place in scope 0 at lift.rs:22:26: 22:29
    let mut _2: u32;                     // in scope 0 at lift.rs:23:9: 23:14
    let mut _4: u32;                     // in scope 0 at lift.rs:30:5: 30:6
    let mut _5: u32;                     // in scope 0 at lift.rs:30:9: 30:10
    let mut _6: (u32, bool);             // in scope 0 at lift.rs:30:5: 30:10
    scope 1 {
        debug a => _2;                   // in scope 1 at lift.rs:23:9: 23:14
        let mut _3: u32;                 // in scope 1 at lift.rs:24:9: 24:14
        scope 2 {
            debug b => _3;               // in scope 2 at lift.rs:24:9: 24:14
        }
    }

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => lift.rs:25:8: 25:9 (#0)

    coverage Code { bcb: bcb0 } => lift.rs:22:1: 22:29 (#0);
    coverage Code { bcb: bcb0 } => lift.rs:23:17: 23:21 (#0);
    coverage Code { bcb: bcb0 } => lift.rs:24:17: 24:21 (#0);
    coverage Code { bcb: bcb0 } => lift.rs:25:8: 25:9 (#0);
    coverage Code { bcb: bcb1 } => lift.rs:26:9: 26:14 (#0);
    coverage Code { bcb: bcb3 } => lift.rs:28:9: 28:14 (#0);
    coverage Code { bcb: bcb2 } => lift.rs:30:5: 30:10 (#0);
    coverage Code { bcb: bcb2 } => lift.rs:31:1: 31:2 (#0);
    coverage Branch { true_bcb: bcb1, false_bcb: bcb3 } => lift.rs:25:8: 25:9 (#0);

    bb0: {
        Coverage::VirtualCounter(bcb0);  // scope 2 at lift.rs:25:8: 25:9
        _2 = const 0_u32;                // scope 0 at lift.rs:23:17: 23:21
        _3 = const 0_u32;                // scope 1 at lift.rs:24:17: 24:21
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 2 at lift.rs:25:8: 25:9
    }

    bb1: {
        Coverage::VirtualCounter(bcb1);  // scope 2 at lift.rs:25:5: 29:6
        _2 = const 5_u32;                // scope 2 at lift.rs:26:9: 26:14
        goto -> bb3;                     // scope 2 at lift.rs:25:5: 29:6
    }

    bb2: {
        Coverage::VirtualCounter(bcb3);  // scope 2 at lift.rs:25:5: 29:6
        _3 = const 7_u32;                // scope 2 at lift.rs:28:9: 28:14
        goto -> bb3;                     // scope 2 at lift.rs:25:5: 29:6
    }

    bb3: {
        Coverage::VirtualCounter(bcb2);  // scope 2 at lift.rs:30:5: 30:10
        _4 = copy _2;                    // scope 2 at lift.rs:30:5: 30:6
        _5 = copy _3;                    // scope 2 at lift.rs:30:9: 30:10
        _6 = AddWithOverflow(copy _4, copy _5); // scope 2 at lift.rs:30:5: 30:10
        assert(!move (_6.1: bool), "attempt to compute `{} + {}`, which would overflow", move _4, move _5) -> [success: bb4, unwind continue]; // scope 2 at lift.rs:30:5: 30:10
    }

    bb4: {
        _0 = move (_6.0: u32);           // scope 2 at lift.rs:30:5: 30:10
        return;                          // scope 0 at lift.rs:31:2: 31:2
    }
}
"#;

    #[test]
    fn value_arms_lift_format_simple_to_one_condition() {
        let mir = mir::parse_text(FORMAT_SIMPLE_MIR).expect("MIR parses");
        assert_eq!(mir.functions.len(), 2, "runtime and CTFE bodies");
        let runtime = &mir.functions[0];
        assert!(!runtime.is_ctfe);
        // Arms `_3 = const 7_u8` / `_3 = const 39_u8` join at bb3.
        assert_eq!(local_type(runtime, 3), Some("u8"));
        assert_eq!(build_decision_from_block(0, runtime), None);
        assert_eq!(
            try_decompose_candidate_lifted(runtime),
            Ok((
                ite("upper", const_(true), const_(false)),
                Some(Lift::ValueArms)
            ))
        );
        assert_eq!(serde_json::to_value(Lift::ValueArms).unwrap(), "value_arms");
    }

    #[test]
    fn value_arms_lift_leaves_the_ctfe_twin_declined() {
        let mir = mir::parse_text(FORMAT_SIMPLE_MIR).expect("MIR parses");
        let ctfe = &mir.functions[1];
        assert!(ctfe.is_ctfe);
        assert_eq!(ctfe.name, mir.functions[0].name);
        assert_eq!(try_decompose_candidate(ctfe), Err(DeclineReason::CtfeBody));
    }

    #[test]
    fn value_arms_lift_declines_a_nested_if() {
        // Three arms (`1`, `2`, `3`): the inner `if b` is a decision
        // of its own, so neither it nor `a` is lifted.
        let mir = mir::parse_text(VALUE_ARM_DECLINES_MIR).expect("MIR parses");
        let f = leaf_fn(&mir, "nested");
        assert_eq!(lift_value_arms(0, f), None);
        assert_eq!(
            try_decompose_candidate(f),
            Err(DeclineReason::UnsupportedShape)
        );
    }

    #[test]
    fn value_arms_lift_declines_an_arm_that_loops() {
        // The `else` arm runs into the `while k > 9` header, not a
        // join with the `then` arm.
        let mir = mir::parse_text(VALUE_ARM_DECLINES_MIR).expect("MIR parses");
        let f = leaf_fn(&mir, "spin");
        assert_eq!(lift_value_arms(0, f), None);
        assert_eq!(
            try_decompose_candidate(f),
            Err(DeclineReason::UnsupportedShape)
        );
    }

    #[test]
    fn value_arms_lift_declines_arms_writing_different_locals() {
        // `a = 5` / `b = 7`: no common value reaches the join.
        let mir = mir::parse_text(VALUE_ARM_DECLINES_MIR).expect("MIR parses");
        let f = leaf_fn(&mir, "split");
        assert_eq!(lift_value_arms(0, f), None);
        assert_eq!(
            try_decompose_candidate(f),
            Err(DeclineReason::UnsupportedShape)
        );
    }

    #[test]
    fn call_arms_lift_is_recorded() {
        let mir = mir::parse_text(PICK_ASSERT_ARMS_MIR).expect("MIR parses");
        let (_, lift) = try_decompose_candidate_lifted(&mir.functions[0]).expect("a candidate");
        assert_eq!(lift, Some(Lift::CallArms));
        assert_eq!(serde_json::to_value(Lift::CallArms).unwrap(), "call_arms");
        // A Boolean-leaf decision carries no lift.
        let mir = mir::parse_text(DECIDE_AND_MIR).expect("MIR parses");
        let (_, lift) = try_decompose_candidate_lifted(&mir.functions[0]).expect("a candidate");
        assert_eq!(lift, None);
    }
}
