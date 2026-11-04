//! MIR input to the decomposer stage.
//!
//! `Mir` is the typed contract on the `mir-extractor -> decomposer` edge
//! of `architecture/workflow.toml`. Phase 0 parses MIR from rustc's
//! `--emit=mir` text dump (plus optional `-Zmir-include-spans` for
//! source-span recovery — see [ADR-0002]). Production builds will swap
//! this front-end for typed access via `rustc_driver` or the emerging
//! `stable_mir` crate; the [`Mir`] type below is the stable interface
//! that survives that swap.
//!
//! The text format we accept is what rustc emits today (warning in
//! the dump header acknowledged: "subject to change without notice").
//! Only the subset needed for the corpus patterns is parsed — function
//! header, `debug` annotations, `let` locals, basic blocks with the
//! statement / terminator shapes used by short-circuit boolean
//! decisions. Unrecognised shapes are preserved as
//! [`MirStatement::Other`] / [`MirTerminator::Other`] so adding new
//! pattern support is additive.
//!
//! With `-Zmir-include-spans`, every statement and terminator carries
//! a trailing source-span comment of the form
//! `// scope <N> at <file>:<line>:<col>: <line>:<col>`. The parser
//! extracts these into [`SourceSpan`] values attached to each item.
//! Without the flag, span fields are `None` but parsing still succeeds.
//!
//! [ADR-0002]: ../../../architecture/decisions/0002-runtime-pipeline.md

use std::collections::BTreeMap;

/// MIR local identifier (`_0`, `_1`, ...).
pub type LocalId = u32;

/// MIR basic block identifier (`bb0`, `bb1`, ...).
pub type BlockId = u32;

/// A source location range.
///
/// Matches the `(start_line, start_col, end_line, end_col)` shape used
/// by both rustc's `-Zmir-include-spans` annotations and by the
/// `llvm-cov export` branches array. The cross-format compatibility
/// is what lets `floyd::correlate` join MIR decisions to runtime
/// coverage data without resolving counter IDs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceSpan {
    /// Source file path as rustc reports it.
    pub file: String,
    /// Inclusive 1-based start line.
    pub start_line: u32,
    /// Inclusive 1-based start column.
    pub start_col: u32,
    /// End line (1-based; semantics follow rustc).
    pub end_line: u32,
    /// End column (1-based; semantics follow rustc).
    pub end_col: u32,
}

/// Parsed MIR for one or more functions.
#[derive(Debug, Default, Clone)]
pub struct Mir {
    /// Top-level functions in source order.
    pub functions: Vec<MirFunction>,
    /// Count of function definitions in the input text whose fn
    /// header the parser could not recognise. The function body is
    /// skipped (brace-balanced); the count surfaces that something
    /// was elided so the honest-scope report can tell a reader
    /// "Floyd parsed M of M+N functions in this MIR, N were in
    /// shapes the engine's grammar does not yet model (HRTB
    /// signatures, complex trait objects, ...)".
    pub unparseable_functions: usize,
    /// Count of function definitions whose body contained no
    /// `switchInt` terminator at all and were therefore
    /// brace-skipped before the parser ever walked them. These are
    /// straight-line / pure-call-delegation / constant-returning
    /// functions — the engine cannot analyse a decision in them
    /// because there is no decision. Dropping them before the main
    /// parse walk is a scalability win on real crates (heapless'
    /// const-generic monomorphisation produces hundreds of such
    /// bodies, each thousands of lines of MIR).
    pub decisionless_functions: usize,
    /// A top-level item whose closing brace the parser never found:
    /// everything from that item to the end of the input was not
    /// parsed, so functions after it are missing from
    /// [`Mir::functions`]. `None` for a well-formed dump.
    pub unterminated_item: Option<UnterminatedItem>,
}

/// A top-level MIR item that runs to end of input without its
/// closing `}`. See [`Mir::unterminated_item`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnterminatedItem {
    /// The item's header line, e.g. `fn f(_1: bool) -> bool {`.
    pub header: String,
    /// Function headers in the unparsed tail (the item itself, if it
    /// is a function, included).
    pub functions_lost: usize,
}

impl Mir {
    /// One sentence per problem that made the parse incomplete:
    /// unrecognised function headers and an unterminated item. Empty
    /// when every function in the input was parsed.
    pub fn parse_problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.unparseable_functions > 0 {
            out.push(format!(
                "{} function header(s) not recognised by Floyd's MIR grammar; \
                 those functions were not analysed",
                self.unparseable_functions
            ));
        }
        if let Some(u) = &self.unterminated_item {
            out.push(format!(
                "no closing brace for the top-level item `{}`; the rest of the \
                 file ({} function(s)) was not parsed or analysed",
                u.header, u.functions_lost
            ));
        }
        out
    }
}

/// One function's MIR.
#[derive(Debug, Default, Clone)]
pub struct MirFunction {
    /// Function name as it appears in the MIR dump.
    pub name: String,
    /// Typed arguments, in declaration order.
    pub args: Vec<MirArg>,
    /// Raw return type text (Phase 0 keeps this as a string).
    pub return_ty: String,
    /// `let`-declared locals (excluding arguments).
    pub locals: Vec<MirLocal>,
    /// Map from `LocalId` to its source name when the `debug`
    /// annotation declares one. Critical for mapping MC/DC analysis
    /// back to user variable names.
    pub debug_names: BTreeMap<LocalId, String>,
    /// Captured variables when this function is a closure body.
    /// Populated from `debug name => <capture_expr>;` annotations
    /// of the form `((*_N).<F>: <Ty>)` (by-value),
    /// `(*((*_N).<F>: &<Ty>))` (by-ref), or
    /// `(*((*_N).<F>: &mut <Ty>))` (FnMut). A post-pass after
    /// function parsing propagates each capture's name into
    /// `debug_names` for any body local that reads the capture,
    /// so the decomposer can recover conditions like
    /// `x && b` even when `b` is a closure capture.
    pub captures: Vec<MirCapture>,
    /// Basic blocks in source order.
    pub blocks: Vec<MirBlock>,
    /// Number of coverage mapping statements in the body preamble
    /// (`coverage Code { .. } => <span>;`, `coverage Branch { .. }
    /// => <span>;`, ...). rustc prints these when the MIR is emitted
    /// by the same `-Cinstrument-coverage` compilation that produces
    /// the test binary; they are the regions `llvm-cov` will report
    /// for this function. Zero in an instrumented MIR means rustc
    /// did not instrument the body (it skips
    /// `#[automatically_derived]` impls, `#[coverage(off)]` items and
    /// bodies whose code has no usable source span), so the function
    /// has no coverage record. Lower-case `coverage branch { .. }`
    /// lines are branch markers, not mappings, and are not counted.
    pub coverage_mappings: usize,
    /// The source spans of those mappings, as printed. They are the
    /// spans of the code and branch regions `llvm-cov` reports for
    /// this body, so they locate its coverage record even when a
    /// decomposed condition's own span carries no region.
    pub coverage_spans: Vec<SourceSpan>,
    /// `true` for the body rustc prints under a `// MIR FOR CTFE`
    /// line: the compile-time-evaluation rendering of a `const fn`,
    /// which follows its runtime body under the same name. CTFE MIR
    /// keeps shapes that the runtime body optimises away (operand
    /// copies, `StorageLive`), and it never runs at runtime; the
    /// runtime body carries the decision and its coverage.
    pub is_ctfe: bool,
}

/// A captured variable from a closure's environment.
#[derive(Debug, Clone)]
pub struct MirCapture {
    /// Source-level name of the captured variable.
    pub name: String,
    /// Local id of the closure environment (typically `_1`).
    pub env_local: LocalId,
    /// Field index inside the closure environment.
    pub field: u32,
    /// `true` when the capture is held by reference (`&T` or
    /// `&mut T`); `false` when held by value (move closure).
    /// The body access patterns differ — by-value reads field
    /// directly, by-ref reads a reference then dereferences it.
    pub by_ref: bool,
}

/// A function argument: `_<n>: <ty>`.
#[derive(Debug, Clone)]
pub struct MirArg {
    /// The local identifier (e.g. `1` for `_1`).
    pub local: LocalId,
    /// Raw type text.
    pub ty: String,
}

/// A `let`-declared local: `let [mut] _<n>: <ty>;`.
#[derive(Debug, Clone)]
pub struct MirLocal {
    /// The local identifier.
    pub local: LocalId,
    /// Raw type text.
    pub ty: String,
    /// Whether the local was declared `mut`.
    pub mutable: bool,
    /// Source span of this local's declaration, if present in the MIR.
    pub span: Option<SourceSpan>,
}

/// One basic block.
#[derive(Debug, Clone)]
pub struct MirBlock {
    /// Block identifier (e.g. `0` for `bb0`).
    pub id: BlockId,
    /// Body statements in source order.
    pub statements: Vec<MirStatement>,
    /// The block-terminating instruction.
    pub terminator: MirTerminator,
}

/// A non-terminating statement inside a block.
#[derive(Debug, Clone)]
pub enum MirStatement {
    /// `_<dst> = copy _<src>;` or `_<dst> = copy (*_<src>);`
    AssignCopy {
        /// Destination local.
        dst: LocalId,
        /// Source local.
        src: LocalId,
        /// `true` for the dereference form `copy (*_<src>)`: `dst`
        /// receives the pointee of `src`, not `src`'s own value.
        /// `false` for a plain `copy _<src>` value alias.
        deref: bool,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `_<dst> = const <bool>;`
    AssignConstBool {
        /// Destination local.
        dst: LocalId,
        /// Constant value.
        value: bool,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `_<dst> = discriminant(_<src>);`
    ///
    /// Read the enum tag of `src` into `dst`. Emitted by rustc as the
    /// first statement of an `if let` / `match` decision block. The
    /// `switchInt` that follows in the same block is a switch on
    /// `dst`'s value (the variant index).
    AssignDiscriminant {
        /// Destination local (holds the discriminant integer).
        dst: LocalId,
        /// Source local (the enum being matched).
        src: LocalId,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `_<dst> = copy ((_<src> as <Variant>).<field>: <Ty>);`
    ///
    /// Read field `field` of the downcast view of `src` as `Variant`,
    /// into `dst`. Emitted by rustc inside the matched arm of an
    /// `if let` / `match` whose pattern has a binding. The presence
    /// of this statement at the head of an arm block is what lets the
    /// decomposer tell the matched arm from the unmatched arm.
    AssignDowncast {
        /// Destination local.
        dst: LocalId,
        /// Scrutinee local being downcast.
        src: LocalId,
        /// Enum variant name (e.g. `Some`, `Ok`, `Err`, or a user variant).
        variant: String,
        /// Field index inside the variant.
        field: u32,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `_<dst> = [no_retag ]copy ((*_<env>).<field>: <Ty>);`
    ///
    /// A closure body reading a captured variable. By-value
    /// captures (move closures) read the field directly as a value;
    /// by-ref / FnMut captures read the field as a reference, with
    /// `holds_ref` set, and the value is read via a subsequent
    /// `_<other> = copy (*_<dst>)` deref. The capture-propagation
    /// pass uses these to set `debug_names` for any body locals
    /// that read captures, so the decomposer can recover decisions
    /// involving captured variables.
    AssignCaptureRead {
        /// Destination local — receives either the captured value
        /// (`holds_ref` false) or a reference to it (true).
        dst: LocalId,
        /// Closure environment local (typically `_1`).
        env: LocalId,
        /// Field index inside the closure environment.
        field: u32,
        /// `true` when the field's type starts with `&` — the dst
        /// holds a reference, not the value.
        holds_ref: bool,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `_<dst> = <CompareOp>(<lhs>, <rhs>);`
    ///
    /// A binary comparison assigned to a bool local. Emitted by
    /// rustc for inline comparisons like `speed > 50`, `code == 1`,
    /// `state != 0`. The decomposer uses this both as a terminal
    /// leaf (when `dst` is the function's return value) and to
    /// synthesize a human-readable condition name (e.g.
    /// `speed > 50`) when a later `switchInt` branches on the
    /// resulting bool temporary.
    AssignBinaryCompare {
        /// Destination local (always bool-typed).
        dst: LocalId,
        /// Comparison operator.
        op: CompareOp,
        /// Left-hand operand.
        lhs: Operand,
        /// Right-hand operand.
        rhs: Operand,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `_<dst> = Not(copy _<src>);` (or `move _<src>`).
    ///
    /// Boolean unary negation. Emitted by rustc when
    /// `-Cinstrument-coverage` runs with
    /// `-Zcoverage-options=branch,condition` — the Not operation's
    /// two outcomes need their own branch counter, so rustc
    /// materialises it as a distinct statement instead of folding
    /// the `!` into a switchInt arm swap (the fallback shape).
    AssignUnaryNot {
        /// Destination local (bool-typed).
        dst: LocalId,
        /// Source local being negated.
        src: LocalId,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// A statement shape Phase 0 doesn't yet parse. Preserved verbatim.
    Other {
        /// Original text of the statement.
        text: String,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
}

/// Comparison operator recognised by the MIR parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

impl CompareOp {
    /// The source-level spelling, used when synthesizing condition
    /// names for inline comparisons.
    pub fn as_source_str(self) -> &'static str {
        match self {
            CompareOp::Eq => "==",
            CompareOp::Ne => "!=",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
        }
    }
}

/// An operand of a MIR rvalue. Comparison operands take this shape:
/// either a copy/move of a local, or a constant literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// `copy _<N>` — read a local without moving it.
    Copy(LocalId),
    /// `move _<N>` — read a local and consume it.
    Move(LocalId),
    /// `const <literal>` — a constant value, preserved as its MIR
    /// textual form (e.g. `50_i32`, `0_u32`).
    Const(String),
}

impl Operand {
    /// For a `Const(...)` operand, return the literal value without
    /// its trailing `_<type>` suffix (so `50_i32` renders as `50`).
    /// Returns the operand text unchanged when no recognisable type
    /// suffix is present. For non-const operands the caller is
    /// expected to use [`Self::local_id`] to recover the underlying
    /// local; this method returns an empty string in that case as a
    /// safe fallback rather than panicking.
    pub fn const_display_value(&self) -> &str {
        match self {
            Operand::Const(s) => match s.rfind('_') {
                Some(i)
                    if s[i + 1..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic()) =>
                {
                    &s[..i]
                }
                _ => s.as_str(),
            },
            _ => "",
        }
    }

    /// For a `Copy` or `Move` operand, return the [`LocalId`] of the
    /// referenced local. Returns `None` for `Const`.
    pub fn local_id(&self) -> Option<LocalId> {
        match self {
            Operand::Copy(id) | Operand::Move(id) => Some(*id),
            Operand::Const(_) => None,
        }
    }
}

/// A block terminator.
#[derive(Debug, Clone)]
pub enum MirTerminator {
    /// `return;`
    Return {
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `goto -> bb<target>;`
    Goto {
        /// Successor block.
        target: BlockId,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `switchInt(copy _<discr>) -> [<value>: bb<target>, ..., otherwise: bb<id>];`
    ///
    /// The `span` here is the source location of the *condition test* —
    /// e.g. for `a && b` it is the span of `a`. This is the load-bearing
    /// field for [ADR-0002] correlate: it matches against the
    /// per-condition branch entries that `llvm-cov export` emits with
    /// the same `(file, line:col)` shape.
    ///
    /// [ADR-0002]: ../../../architecture/decisions/0002-runtime-pipeline.md
    SwitchInt {
        /// Local whose value is being switched on.
        discr: LocalId,
        /// Specific-value arms (value, target).
        targets: Vec<(u128, BlockId)>,
        /// The `otherwise` (fallthrough) target.
        otherwise: BlockId,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `unreachable;`
    ///
    /// Compiler-inserted catch-all for impossible control flow. Emitted
    /// at the `otherwise` arm of a `switchInt` whose discriminant comes
    /// from `discriminant(_X)` when every variant of the scrutinee's
    /// enum is already covered by an explicit arm. The decomposer
    /// treats such arms as non-paths: an [`MirTerminator::Unreachable`]
    /// arm is filtered out of the reachable-arm set before deciding the
    /// shape of an `if let` / `match`.
    Unreachable {
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `_<dst> = <func-expr>(<args>) -> [return: bb<target>, unwind ...];`
    ///
    /// A function-call terminator. The decomposer recognises one
    /// specific shape — `<T as Try>::branch(scrutinee)` whose return
    /// flows into a `discriminant + switchInt` block — to skip past
    /// the `?` operator and decompose the Continue arm directly.
    /// The unwind path is not represented; cleanup blocks carry no
    /// user-level decisions.
    Call {
        /// Local that receives the function's return value.
        dst: LocalId,
        /// Verbatim text of the function expression (the
        /// `<...>::name` part), kept for diagnostics and for the
        /// `?` detector to match against `Try::branch`.
        func_text: String,
        /// Successor block reached on normal return.
        target: BlockId,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `_<dst> = <func-expr>(<args>) -> unwind <action>;` or
    /// `_<dst> = <func-expr>(<args>) -> bb<cleanup>;`
    ///
    /// A call with no `return` edge: the callee returns `!`
    /// (`core::panicking::panic`, `std::rt::panic_fmt`,
    /// `std::rt::begin_panic`, `core::panicking::assert_failed`,
    /// `std::process::exit`, ..). Control never continues past it, so
    /// a path that reaches one diverges. This is how rustc lowers the
    /// failure arm of `assert!`, `assert_eq!`, `panic!`, `unwrap()`
    /// and friends.
    NoReturnCall {
        /// Local named as the (never-written) destination.
        dst: LocalId,
        /// Verbatim text of the function expression, as for
        /// [`MirTerminator::Call::func_text`].
        func_text: String,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// `assert(<cond>, "<msg>", <args>..) -> [success: bb<target>, unwind ..];`
    ///
    /// rustc's checked-operation terminator: overflow checks on
    /// arithmetic (`a + b` under `-C overflow-checks`), division by
    /// zero, array bounds checks, alignment checks. When `<cond>`
    /// holds, control continues at `success`; otherwise the runtime
    /// panics with `msg`. The `success` edge is the function's
    /// normal control flow, so successor-following code treats an
    /// `Assert` like a non-panicking [`MirTerminator::Call`] or a
    /// [`MirTerminator::Goto`]. The assert itself is never a user
    /// decision: it has no source-level `if`, and its failure edge
    /// is a panic.
    Assert {
        /// Successor block reached when the asserted condition holds.
        success: BlockId,
        /// Cleanup block named by `unwind: bb<N>`, if any. `None` for
        /// `unwind continue`, `unwind unreachable` and
        /// `unwind terminate(..)`.
        unwind: Option<BlockId>,
        /// The panic message template (the string literal's contents,
        /// escapes left as rustc printed them), e.g.
        /// ``attempt to compute `{} + {}`, which would overflow``.
        msg: String,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
    /// A terminator shape Phase 0 doesn't yet parse. Preserved verbatim.
    Other {
        /// Original text of the terminator.
        text: String,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
}

/// A parser error.
#[derive(Debug, Clone)]
pub struct ParseError {
    /// 1-indexed line number where the error was detected.
    pub line: usize,
    /// Human-readable description.
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MIR parse error at line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// First-pass filter that brace-strips function bodies with no
/// `switchInt` terminator.
///
/// Walks the input line by line, tracking brace depth. At depth 0,
/// when a line that looks like a function header (`fn ... {`) is
/// seen, scan ahead to the matching close brace and check whether
/// the body contains the string `switchInt`. If not, emit the fn
/// header followed by a minimal `bb0: { return; }` stub body and
/// skip the real body. If yes, emit the function verbatim. Any
/// non-function top-level item (static, const, impl block, scope,
/// etc.) is emitted verbatim.
///
/// Returns the filtered input and the count of stripped functions.
fn strip_switchintless_fn_bodies(input: &str) -> (String, usize) {
    let lines: Vec<&str> = input.lines().collect();
    let mut out = String::with_capacity(input.len());
    let mut skipped: usize = 0;
    let mut top_depth: u32 = 0;
    let mut i: usize = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // At depth 0: detect function headers. These start with
        // `fn ` after trimming leading whitespace and end with
        // `{`. Non-fn top-level items (static X = {, const FOO =
        // {, scope ... {) are left untouched; the main parser's
        // own skip_depth / scope_nest machinery handles them.
        // An allocation hex dump is data: its ASCII column can end
        // in `{` (e.g. the bytes `\u{`), which must not count as a
        // brace. Copy it through to its closing `}` uncounted.
        if top_depth == 0 && is_alloc_dump_header(trimmed) {
            loop {
                out.push_str(lines[i]);
                out.push('\n');
                i += 1;
                if i >= lines.len() || lines[i - 1].trim() == "}" {
                    break;
                }
            }
            continue;
        }

        if top_depth == 0 && is_fn_header_line(trimmed) {
            // Scan ahead for the matching close brace, tracking
            // local depth. We only consider `switchInt` occurrences
            // within THIS function's body (so a nested fn in a
            // scope would still have its own switchInt check if
            // we ever hit one, though in practice MIR dumps do
            // not nest fns inside bodies).
            let mut local_depth: u32 = 1;
            let mut j = i + 1;
            let mut has_decision_marker = false;
            while j < lines.len() && local_depth > 0 {
                let tl = lines[j].trim();
                if !has_decision_marker && line_has_decision_marker(tl) {
                    has_decision_marker = true;
                }
                if tl.ends_with('{') {
                    local_depth += 1;
                } else if tl == "}" {
                    local_depth -= 1;
                }
                j += 1;
            }
            // j now points one past the function's closing brace.
            //
            // Size threshold: the strip pass is a scalability
            // optimisation for crates whose per-function MIR is
            // thousands of lines each (heapless' const-generic
            // monomorphisation). For small bodies (hand-written
            // test fixtures, tiny real functions) the parser's
            // own walk is already cheap and stripping would elide
            // legitimate `debug` / `let` metadata the parser-level
            // tests depend on. Keep anything under the threshold
            // verbatim; apply the strip only when it actually
            // moves the needle.
            let body_lines = j.saturating_sub(i);
            let worth_stripping = body_lines >= 50;
            if has_decision_marker || !worth_stripping {
                for body_line in &lines[i..j] {
                    out.push_str(body_line);
                    out.push('\n');
                }
            } else {
                // Keep the fn header (first line) + stub body.
                out.push_str(lines[i]);
                out.push('\n');
                out.push_str("    bb0: {\n");
                out.push_str("        return;\n");
                out.push_str("    }\n");
                out.push_str("}\n");
                skipped += 1;
            }
            i = j;
            continue;
        }

        out.push_str(line);
        out.push('\n');
        if trimmed.ends_with('{') {
            top_depth = top_depth.saturating_add(1);
        } else if trimmed == "}" {
            top_depth = top_depth.saturating_sub(1);
        }
        i += 1;
    }
    (out, skipped)
}

/// Does this body line contain a MIR syntactic marker that could
/// let Floyd's decomposer produce a candidate decision?
///
/// Keeps the strip pass's positive-list conservative: any `switchInt`
/// terminator (branching); any inline-compare rvalue (`= Eq(`,
/// `= Ne(`, `= Lt(`, `= Le(`, `= Gt(`, `= Ge(`); any unary negation
/// (`= Not(`); or a constant assignment to the return place
/// (`_0 = const `) which `extract_terminal_value` recovers as
/// `Node::Const`. Bodies with none of these markers cannot
/// produce a candidate, so they are safe to drop.
fn line_has_decision_marker(line: &str) -> bool {
    line.contains("switchInt")
        || line.contains(" = Eq(")
        || line.contains(" = Ne(")
        || line.contains(" = Lt(")
        || line.contains(" = Le(")
        || line.contains(" = Gt(")
        || line.contains(" = Ge(")
        || line.contains(" = Not(")
        || line.contains("_0 = const ")
}

/// Does this line look like a top-level MIR function header? E.g.
/// `fn make_input(_1: &[u8]) -> bool {`. Matched conservatively —
/// we only strip bodies when we are confident the line is a fn
/// header and not something else (nested fns inside bodies,
/// closures, scope declarations).
fn is_fn_header_line(line: &str) -> bool {
    line.starts_with("fn ") && line.ends_with('{')
}

/// Is this line the header of an allocation dump, e.g.
/// `alloc196 (size: 16, align: 8) {`? rustc prints one after the
/// functions that use a constant allocation. Its body is a hex dump
/// with an ASCII rendering of the bytes, so braces in it are data;
/// it ends at the first line that is exactly `}` (a dump line always
/// starts with an offset or hex bytes).
fn is_alloc_dump_header(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("alloc") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(" (") && line.ends_with('{')
}

/// Parse an `--emit=mir` text dump into a [`Mir`].
///
/// Phase 0 scope: function headers, `debug` annotations, `let`
/// locals, blocks with `copy`/`const bool` assignments, and
/// `return` / `goto` / `switchInt` / `unreachable` / call / `assert`
/// terminators. Other shapes pass
/// through as [`MirStatement::Other`] / [`MirTerminator::Other`].
///
/// When the MIR was emitted with `-Zmir-include-spans`, the parser
/// also captures inline source-span comments of the form
/// `// scope <N> at <file>:<line>:<col>: <line>:<col>` and attaches
/// them to the corresponding statement / terminator. Span-less MIR
/// is also accepted; the `span` fields are then `None`.
///
/// Runs `strip_switchintless_fn_bodies` first, so functions whose
/// bodies carry no `switchInt` terminator are brace-stripped before
/// the main parse walk. The stripped count is surfaced via
/// [`Mir::decisionless_functions`].
pub fn parse_text(input: &str) -> Result<Mir, ParseError> {
    // Lazy skip pass: brace-strip any function body that contains
    // no `switchInt` terminator. These functions carry no decision
    // Floyd can analyse (straight-line code, pure-call delegation,
    // constant returns) so there is no analytical cost to dropping
    // them before the main parse walk. The win is scalability:
    // heapless' const-generic monomorphisation produces hundreds
    // of such bodies, each thousands of lines of MIR text; parsing
    // them in full was the single biggest contributor to Pass 1
    // wall-clock on real embedded crates. The brace-stripped body
    // is replaced with a minimal `bb0: { return; }` stub so the
    // existing parser still sees a well-formed function header
    // and records it for the per-crate function count (both the
    // function and its emptiness are user-visible in the report
    // via `decisionless_functions`).
    let (input, decisionless) = strip_switchintless_fn_bodies(input);
    let mut mir = Mir {
        decisionless_functions: decisionless,
        ..Mir::default()
    };
    let input = input.as_str();
    let mut depth: u8 = 0;
    // Counter for "skip this block" regions outside any function —
    // top-level non-`fn` items like
    // `const <path>::promoted[N]: <ty> = { ... }` for closures'
    // promoted constants, or the duplicate `MIR FOR CTFE` rendering
    // of `const fn`s. When > 0 the parser tracks brace depth and
    // swallows everything until the matching close.
    let mut skip_depth: u32 = 0;
    // Nested `scope N { ... }` regions inside a function body. The
    // body of a scope is parsed (it carries `debug` and `let`
    // declarations the decomposer relies on) but the enclosing
    // braces must be tracked so the matching close doesn't get
    // misread as the function's close.
    let mut scope_nest: u32 = 0;
    // Brace depth inside a function-level `coroutine layout { ... }`
    // block (async fns and blocks): the coroutine's saved-local
    // layout, which carries no control flow. Skipped, braces counted.
    let mut layout_depth: u32 = 0;
    let mut current_fn: Option<MirFunction> = None;
    let mut current_block: Option<MirBlock> = None;
    // Set by a `// MIR FOR CTFE` line; consumed by the next
    // top-level item, which is the CTFE body of a `const fn`.
    let mut next_is_ctfe = false;
    // Inside an allocation dump (see `is_alloc_dump_header`): skip
    // to its closing `}` without counting braces.
    let mut in_alloc_dump = false;
    // Header of the top-level item being parsed or skipped, for
    // `Mir::unterminated_item`.
    let mut open_item = String::new();

    for (i, raw) in input.lines().enumerate() {
        let line_no = i + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if in_alloc_dump {
            if trimmed == "}" {
                in_alloc_dump = false;
            }
            continue;
        }
        if depth == 0 && skip_depth == 0 && is_alloc_dump_header(trimmed) {
            next_is_ctfe = false;
            in_alloc_dump = true;
            open_item = trimmed.to_string();
            continue;
        }
        // Lines that are *entirely* comments (preamble warnings, etc.)
        // are skipped. Lines with a trailing comment are split below.
        if trimmed.starts_with("//") {
            if depth == 0 && trimmed == "// MIR FOR CTFE" {
                next_is_ctfe = true;
            }
            continue;
        }

        let (code, comment) = split_off_trailing_comment(trimmed);
        let span = comment.and_then(extract_span_from_comment);

        // Inside a skipped block, only track brace depth.
        if skip_depth > 0 {
            if code.ends_with('{') {
                skip_depth += 1;
            } else if code == "}" {
                skip_depth -= 1;
            }
            continue;
        }

        // Decide whether to *enter* a skipped block at this `{`. The
        // skip applies only to top-level non-function items at depth
        // 0 — promoted constants, the duplicate `MIR FOR CTFE`
        // function rendering, etc. Inside a function (depth 1+) the
        // parser walks every nested brace so that `scope N { ... }`
        // bodies, which carry `debug` and `let` declarations, are
        // captured.
        if code.ends_with('{') && depth == 0 && !code.starts_with("fn ") {
            next_is_ctfe = false;
            skip_depth = 1;
            open_item = code.to_string();
            continue;
        }

        if layout_depth > 0 {
            if code.ends_with('{') {
                layout_depth += 1;
            } else if code == "}" {
                layout_depth -= 1;
            }
            continue;
        }
        if depth == 1 && code == "coroutine layout {" {
            layout_depth = 1;
            continue;
        }

        // `scope N { ... }` inside a function: don't reinterpret as a
        // basic-block header. Just track the nesting so the matching
        // `}` doesn't get read as the function's close. The body
        // (`debug`, `let`) is processed by the depth=1 branch below.
        if depth == 1 && code.ends_with('{') && code.starts_with("scope ") {
            scope_nest += 1;
            continue;
        }

        // Open a brace: either a function or a basic block header.
        if code.ends_with('{') {
            if depth == 0 {
                open_item = code.to_string();
                let is_ctfe = std::mem::take(&mut next_is_ctfe);
                match parse_fn_header(code) {
                    Some(f) => {
                        current_fn = Some(MirFunction { is_ctfe, ..f });
                        depth = 1;
                        continue;
                    }
                    None => {
                        // Error-tolerant: an unparseable fn header
                        // (complex HRTB generics, trait object
                        // return types the current grammar does
                        // not model) skips the whole function body
                        // via the `skip_depth` mechanism. One
                        // unparseable function must not kill the
                        // whole MIR parse — a 2.9MB MIR with 360
                        // functions should surface the 359 that
                        // DO parse. The count is tracked so the
                        // honest-scope reporter can say "N
                        // functions could not be parsed by this
                        // engine's MIR grammar".
                        mir.unparseable_functions += 1;
                        skip_depth = 1;
                        continue;
                    }
                }
            } else if depth == 1 {
                let id = parse_bb_header(code).ok_or_else(|| ParseError {
                    line: line_no,
                    message: format!("expected bb header, got: {code}"),
                })?;
                current_block = Some(MirBlock {
                    id,
                    statements: Vec::new(),
                    terminator: MirTerminator::Other {
                        text: "(missing)".to_string(),
                        span: None,
                    },
                });
                depth = 2;
                continue;
            }
        }

        // Close a brace.
        if code == "}" {
            if depth == 2 {
                if let (Some(block), Some(f)) = (current_block.take(), current_fn.as_mut()) {
                    f.blocks.push(block);
                }
                depth = 1;
            } else if depth == 1 && scope_nest > 0 {
                scope_nest -= 1;
            } else if depth == 1 {
                if let Some(mut f) = current_fn.take() {
                    // Closure-capture post-pass: fold capture names
                    // into debug_names so the decomposer can recover
                    // conditions involving captured variables.
                    propagate_capture_names(&mut f);
                    mir.functions.push(f);
                }
                depth = 0;
            }
            continue;
        }

        // Body line.
        match depth {
            1 => {
                let f = current_fn.as_mut().expect("depth=1 requires current_fn");
                if code.starts_with("debug ") {
                    match parse_debug(code) {
                        Some(DebugAnnotation::Local { name, local }) => {
                            // First-wins: rustc emits desugaring
                            // bindings for the same local in later
                            // scopes (e.g. the `val` binding the `?`
                            // operator synthesizes for a local the
                            // user named `x`). The user's name is
                            // always first in source order.
                            //
                            // Issue #12: skip reserved identifiers
                            // that can never be a Boolean condition.
                            // Methods declare `debug self => _1;` but
                            // the self receiver is a struct value, not
                            // a decision condition. Letting `self` into
                            // debug_names causes the decomposer to pick
                            // method bodies and report nonsensical
                            // conditions like "self: UNEXERCISED" on
                            // method-heavy real-world crates.
                            if !is_reserved_debug_name(&name) {
                                f.debug_names.entry(local).or_insert(name);
                            }
                        }
                        Some(DebugAnnotation::Capture(c)) => {
                            f.captures.push(c);
                        }
                        None => {}
                    }
                } else if code.starts_with("let ") {
                    if let Some(mut loc) = parse_let(code) {
                        loc.span = span.clone();
                        f.locals.push(loc);
                    }
                } else if is_coverage_mapping(code) {
                    f.coverage_mappings += 1;
                    if let Some(span) = coverage_mapping_span(code) {
                        f.coverage_spans.push(span);
                    }
                }
                // Other fn-level lines (e.g. scope annotations) ignored.
            }
            2 => {
                let b = current_block
                    .as_mut()
                    .expect("depth=2 requires current_block");
                if let Some(t) = parse_terminator(code, span.clone()) {
                    b.terminator = t;
                } else if let Some(s) = parse_statement(code, span.clone()) {
                    b.statements.push(s);
                } else {
                    b.statements.push(MirStatement::Other {
                        text: code.to_string(),
                        span,
                    });
                }
            }
            _ => {
                // Outside any function — Phase 0 silently ignores.
            }
        }
    }

    if in_alloc_dump || skip_depth > 0 || depth > 0 || layout_depth > 0 {
        mir.unterminated_item = Some(UnterminatedItem {
            functions_lost: count_fn_headers_from(input, &open_item),
            header: open_item,
        });
    }

    Ok(mir)
}

/// Number of top-level function headers from the last line equal
/// to `header` to the end of `input`.
fn count_fn_headers_from(input: &str, header: &str) -> usize {
    let lines: Vec<&str> = input.lines().collect();
    let start = lines
        .iter()
        .rposition(|l| l.trim() == header)
        .unwrap_or(lines.len());
    lines[start..]
        .iter()
        .filter(|l| is_fn_header_line(l.trim()))
        .count()
}

// ---------------------------------------------------------------------------
// Line parsers
// ---------------------------------------------------------------------------

/// Split a line into (code-before-comment, comment-after-`//`).
///
/// The comment includes everything after the `//`; the code has any
/// trailing whitespace stripped. If there is no `//`, the comment is
/// `None`.
fn split_off_trailing_comment(line: &str) -> (&str, Option<&str>) {
    match line.find("//") {
        Some(idx) => (line[..idx].trim_end(), Some(line[idx + 2..].trim())),
        None => (line, None),
    }
}

/// Extract a [`SourceSpan`] from an MIR comment of the form
/// `scope <N> at <file>:<L>:<C>: <L>:<C>` (or variants beginning with
/// `in scope`, `return place in scope`, etc.). Returns `None` if the
/// comment doesn't match the expected shape.
fn extract_span_from_comment(comment: &str) -> Option<SourceSpan> {
    // Anchor: the literal " at " that separates the scope prefix from
    // the source location. We use rfind to be tolerant of variants like
    // "return place in scope 0 at ..." where " at " could in principle
    // appear earlier (it doesn't in practice, but rfind is safer).
    let at_idx = comment.rfind(" at ")?;
    let span_str = comment[at_idx + 4..].trim();
    parse_span_literal(span_str)
}

/// Parse a `<file>:<L1>:<C1>: <L2>:<C2>` literal into a [`SourceSpan`].
///
/// The file path may contain colons (notably on Windows). We split
/// from the right at the `": "` separator and then peel two trailing
/// `:N` segments off the left half.
fn parse_span_literal(s: &str) -> Option<SourceSpan> {
    // Split on ": " to separate the LHS (file:L1:C1) from RHS (L2:C2).
    let space_idx = s.rfind(": ")?;
    let lhs = &s[..space_idx];
    let rhs = s[space_idx + 2..].trim();

    let rhs_colon = rhs.find(':')?;
    let end_line = rhs[..rhs_colon].parse::<u32>().ok()?;
    let end_col = rhs[rhs_colon + 1..].parse::<u32>().ok()?;

    let c1_colon = lhs.rfind(':')?;
    let file_and_line = &lhs[..c1_colon];
    let start_col = lhs[c1_colon + 1..].parse::<u32>().ok()?;

    let l1_colon = file_and_line.rfind(':')?;
    let file = &file_and_line[..l1_colon];
    let start_line = file_and_line[l1_colon + 1..].parse::<u32>().ok()?;

    Some(SourceSpan {
        file: file.to_string(),
        start_line,
        start_col,
        end_line,
        end_col,
    })
}

fn parse_fn_header(line: &str) -> Option<MirFunction> {
    // fn decide(_1: bool, _2: bool) -> bool {
    // fn decide(_1: Result<bool, ()>) -> bool {
    let s = line.strip_prefix("fn ")?.strip_suffix(" {")?.trim();
    let paren = s.find('(')?;
    let name = s[..paren].trim().to_string();
    let after_open = &s[paren + 1..];

    // Find the matching `)` for the args list — nested `()`, `<>`,
    // and `[]` inside argument types (e.g. `Result<bool, ()>`) push
    // and pop balanced depth, so we can't just take the first `)`.
    let close_rel = find_matching_close(after_open, '(', ')')?;
    let args_str = &after_open[..close_rel];
    let return_ty = after_open[close_rel + 1..]
        .trim()
        .strip_prefix("->")
        .map(|r| r.trim().to_string())
        .unwrap_or_default();

    let mut args = Vec::new();
    for raw_arg in split_top_level(args_str, ',') {
        let arg = raw_arg.trim();
        if arg.is_empty() {
            continue;
        }
        let colon = arg.find(':')?;
        let local = arg[..colon].trim().strip_prefix('_')?.parse::<u32>().ok()?;
        let ty = arg[colon + 1..].trim().to_string();
        args.push(MirArg { local, ty });
    }

    Some(MirFunction {
        name,
        args,
        return_ty,
        ..MirFunction::default()
    })
}

/// Is the `>` at byte `i` of `s` the head of a `->` arrow (the return
/// type of a fn pointer, `impl Fn..`, `dyn Fn..` or `for<'a> fn..`
/// type) rather than a closing angle bracket? `>>`, `>=`, `<<` and
/// `<=` need no such care: in a type, `>>` is two closing brackets
/// (`Vec<Vec<u8>>`) and `<<` two opening ones (`<<T as A>::B as C>`),
/// and comparisons never appear in a type.
fn is_arrow_head(s: &str, i: usize) -> bool {
    s[..i].ends_with('-')
}

/// Find the matching closing bracket for an open bracket that is
/// considered to sit just before `s`. Returns the index inside `s` of
/// the matching close. Honours nested `()`, `<>`, `[]` and `{}`; the
/// `>` of a `->` arrow is not a bracket.
fn find_matching_close(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth: i32 = 1;
    for (i, c) in s.char_indices() {
        match c {
            '>' if is_arrow_head(s, i) => {}
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            '<' | '[' | '{' => depth += 1,
            '>' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Split `s` on `sep` at top level — i.e. ignoring separators that
/// appear inside balanced `()`, `<>`, `[]` or `{}`. The `>` of a `->`
/// arrow is not a bracket.
fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '>' if is_arrow_head(s, i) => {}
            '(' | '<' | '[' | '{' => depth += 1,
            ')' | '>' | ']' | '}' => depth -= 1,
            x if x == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// `coverage Code { bcb: bcb0 } => src/lib.rs:1:1: 1:9 (#0);` and the
/// other capitalised mapping kinds (`Branch`, `MCDCBranch`, ...).
/// The lower-case `coverage branch { true: .., false: .. } => ..`
/// form is a branch marker recorded before mappings are built.
fn is_coverage_mapping(code: &str) -> bool {
    code.strip_prefix("coverage ")
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_uppercase())
}

/// The span of a coverage mapping statement:
/// `coverage Code { bcb: bcb0 } => src/lib.rs:1:1: 1:9 (#0);` gives
/// `src/lib.rs:1:1: 1:9`.
fn coverage_mapping_span(code: &str) -> Option<SourceSpan> {
    let rest = code.rsplit_once(" => ")?.1.trim_end_matches(';');
    let span = rest.rsplit_once(" (#").map_or(rest, |(span, _)| span);
    parse_span_literal(span.trim())
}

fn parse_bb_header(line: &str) -> Option<BlockId> {
    // `bb0: {`               (canonical)
    // `bb15 (cleanup): {`    (cleanup/unwind blocks carry annotations)
    let s = line.strip_suffix('{')?.trim().strip_suffix(':')?.trim();
    // Take the leading `bb<N>` token; ignore any trailing annotation.
    let token = s.split_whitespace().next()?;
    token.strip_prefix("bb")?.parse::<u32>().ok()
}

/// Result of parsing a `debug` annotation.
enum DebugAnnotation {
    /// `debug name => _N;` — straightforward local binding.
    Local { name: String, local: LocalId },
    /// `debug name => <capture-expr>;` — a closure capture in
    /// one of the by-value, by-ref, or FnMut forms.
    Capture(MirCapture),
}

/// Reserved debug names that must not become condition names.
///
/// Currently just `self` — rustc emits `debug self => _1;` for method
/// receivers, but the receiver is never a Boolean condition. Letting
/// it into `debug_names` would cause the decomposer to accept the
/// method body and surface `self` as the condition name, confusing
/// any user who reads the resulting report.
///
/// This is pattern-matched by name rather than by MIR type because the
/// current parser does not retain type information for debug bindings.
/// A future type-aware pass would subsume this filter.
fn is_reserved_debug_name(name: &str) -> bool {
    matches!(name, "self")
}

fn parse_debug(line: &str) -> Option<DebugAnnotation> {
    // debug a => _1;
    // debug b => (*((*_1).0: &bool));            // by-ref (Fn)
    // debug b => ((*_1).0: bool);                // by-value (move closure)
    // debug b => (*((*_1).0: &mut bool));        // by-mut-ref (FnMut)
    let s = line.strip_prefix("debug ")?.strip_suffix(';')?;
    let arrow = s.find("=>")?;
    let name = s[..arrow].trim().to_string();
    let rhs = s[arrow + 2..].trim();

    if let Some(rest) = rhs.strip_prefix('_') {
        if let Ok(local) = rest.parse::<u32>() {
            return Some(DebugAnnotation::Local { name, local });
        }
    }
    if let Some(capture) = parse_capture_expr(rhs, name.clone()) {
        return Some(DebugAnnotation::Capture(capture));
    }
    None
}

/// Parse a closure-capture expression on the RHS of a `debug`
/// annotation. Returns `None` for any shape that isn't one of the
/// three recognised forms.
fn parse_capture_expr(rhs: &str, name: String) -> Option<MirCapture> {
    // By-ref:    (*((*_N).F: &T))
    // FnMut:     (*((*_N).F: &mut T))
    // By-value:  ((*_N).F: T)
    if let Some(inner) = rhs.strip_prefix("(*(").and_then(|s| s.strip_suffix("))")) {
        // inner = `(*_N).F: &T` or `(*_N).F: &mut T`
        let (env_local, field) = parse_env_field(inner)?;
        return Some(MirCapture {
            name,
            env_local,
            field,
            by_ref: true,
        });
    }
    if let Some(inner) = rhs.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        // inner = `(*_N).F: T`
        let (env_local, field) = parse_env_field(inner)?;
        return Some(MirCapture {
            name,
            env_local,
            field,
            by_ref: false,
        });
    }
    None
}

/// Parse the `(*_N).F: <Ty>` core of a capture expression and
/// return `(env_local, field)`.
fn parse_env_field(s: &str) -> Option<(LocalId, u32)> {
    // s = `(*_N).F: <Ty>`
    let rest = s.strip_prefix("(*_")?;
    let close = rest.find(')')?;
    let env_local = rest[..close].parse::<u32>().ok()?;
    let after_close = rest[close + 1..].strip_prefix('.')?;
    let colon = after_close.find(':')?;
    let field = after_close[..colon].parse::<u32>().ok()?;
    Some((env_local, field))
}

/// Post-pass that runs after a function's MIR is fully parsed.
/// Walks all statements in the function and, for any local that
/// reads one of the function's captures, sets that local's
/// `debug_names` entry so the decomposer can name conditions
/// involving the capture.
///
/// Two reads to recognise:
///
/// - `_X = copy ((*_N).<F>: <Ty>)` — direct field read (by-value
///   capture; the value is `_X`).
/// - `_X = no_retag copy ((*_N).<F>: &<Ty>)` — read of a reference
///   field (by-ref / FnMut capture; `_X` now *holds* the reference,
///   so subsequent `_Y = copy (*_X)` derefs are what actually
///   names the captured value).
///
/// The dereference chain is folded by a second pass over the same
/// blocks: when a body statement does `_Y = copy (*_X)` and `_X`
/// is already known to alias a reference-typed capture, `_Y` gets
/// the captured variable's name.
fn propagate_capture_names(f: &mut MirFunction) {
    if f.captures.is_empty() {
        return;
    }
    let by_env_field: BTreeMap<(LocalId, u32), String> = f
        .captures
        .iter()
        .map(|c| ((c.env_local, c.field), c.name.clone()))
        .collect();

    // First pass: AssignCaptureRead statements directly bind a
    // local to a capture. By-value reads put the captured value
    // straight into `dst`; by-ref reads put a reference into
    // `dst`, with the value reached via a subsequent
    // `_Y = copy (*_dst)` deref. Track ref aliases separately so
    // the second pass can name the deref result rather than the
    // reference itself.
    let mut ref_aliases: BTreeMap<LocalId, String> = BTreeMap::new();
    for block in &f.blocks {
        for stmt in &block.statements {
            if let MirStatement::AssignCaptureRead {
                dst,
                env,
                field,
                holds_ref,
                ..
            } = stmt
            {
                if let Some(name) = by_env_field.get(&(*env, *field)) {
                    if *holds_ref {
                        ref_aliases.insert(*dst, name.clone());
                    }
                    // Set debug_names regardless of holds_ref. For
                    // by-value reads this is the captured value
                    // directly; for by-ref reads it's the reference,
                    // but giving the reference the same source-level
                    // name is what lets the subsequent
                    // `_Y = copy (*_dst)` AssignCopy resolve `_Y`'s
                    // name from `_dst` via the existing
                    // extract_terminal_value logic.
                    f.debug_names.entry(*dst).or_insert_with(|| name.clone());
                }
            }
        }
    }

    // Second pass: `_Y = copy (*_X)` is parsed as `AssignCopy{Y, X}`,
    // so the propagation here is simply "if `X` is a reference-alias
    // for a captured name, `Y` takes that name." Iterate until a
    // fixed point (bounded for safety) to handle chains like
    // `_5 = copy (*_4); _0 = copy (*_5);` if they ever occur.
    let mut changed = true;
    let max_iter = f.blocks.iter().map(|b| b.statements.len()).sum::<usize>() + 1;
    let mut iter = 0;
    while changed && iter < max_iter {
        changed = false;
        iter += 1;
        for block in &f.blocks {
            for stmt in &block.statements {
                if let MirStatement::AssignCopy { dst, src, .. } = stmt {
                    if let Some(name) = ref_aliases.get(src).cloned() {
                        if f.debug_names.get(dst) != Some(&name) {
                            f.debug_names.insert(*dst, name.clone());
                            changed = true;
                        }
                    }
                }
            }
        }
    }
}

/// RHS fields parsed out of a capture-read statement.
struct CaptureReadRhs {
    env: LocalId,
    field: u32,
    holds_ref: bool,
}

/// Parse the RHS of a capture-read statement —
/// `copy ((*_N).<F>: <Ty>)` (by-value) or
/// `no_retag copy ((*_N).<F>: &<Ty>)` (by-ref / FnMut).
fn parse_capture_read_rhs(rhs: &str) -> Option<CaptureReadRhs> {
    let after_copy = rhs
        .strip_prefix("no_retag copy ")
        .or_else(|| rhs.strip_prefix("copy "))?;
    // after_copy = `((*_N).F: <Ty>)`
    let inner = after_copy
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))?;
    let (env, field) = parse_env_field(inner)?;
    let ty = inner.rsplit_once(':').map(|(_, t)| t.trim()).unwrap_or("");
    let holds_ref = ty.starts_with('&');
    Some(CaptureReadRhs {
        env,
        field,
        holds_ref,
    })
}

fn parse_let(line: &str) -> Option<MirLocal> {
    // let mut _0: bool;
    // let _0: bool;
    let s = line.strip_prefix("let ")?.strip_suffix(';')?;
    let (mutable, rest) = match s.strip_prefix("mut ") {
        Some(r) => (true, r),
        None => (false, s),
    };
    let colon = rest.find(':')?;
    let local = rest[..colon]
        .trim()
        .strip_prefix('_')?
        .parse::<u32>()
        .ok()?;
    let ty = rest[colon + 1..].trim().to_string();
    Some(MirLocal {
        local,
        ty,
        mutable,
        span: None,
    })
}

fn parse_statement(line: &str, span: Option<SourceSpan>) -> Option<MirStatement> {
    let s = line.strip_suffix(';')?;
    let eq = s.find(" = ")?;
    let dst = s[..eq].trim().strip_prefix('_')?.parse::<u32>().ok()?;
    let rhs = s[eq + 3..].trim();

    if let Some(src_str) = rhs.strip_prefix("copy _") {
        if let Ok(src) = src_str.parse::<u32>() {
            return Some(MirStatement::AssignCopy {
                dst,
                src,
                deref: false,
                span,
            });
        }
    }
    // `_X = copy (*_Y);` — dereference copy, common when reading
    // through a captured-by-ref alias inside a closure body. Treat
    // as a regular AssignCopy so existing decomposer logic picks
    // up `_Y`'s debug name (which the capture-propagation pass
    // will have set for capture-reference aliases).
    if let Some(inner) = rhs.strip_prefix("copy (*_") {
        if let Some(close) = inner.find(')') {
            if let Ok(src) = inner[..close].parse::<u32>() {
                return Some(MirStatement::AssignCopy {
                    dst,
                    src,
                    deref: true,
                    span,
                });
            }
        }
    }
    // Closure capture reads: `_X = copy ((*_N).<F>: <Ty>)` or
    // `_X = no_retag copy ((*_N).<F>: &<Ty>)`. The propagation pass
    // uses these to set debug names for the body locals that hold
    // captured values.
    if let Some(cap) = parse_capture_read_rhs(rhs) {
        return Some(MirStatement::AssignCaptureRead {
            dst,
            env: cap.env,
            field: cap.field,
            holds_ref: cap.holds_ref,
            span,
        });
    }
    if let Some(val) = rhs.strip_prefix("const ") {
        match val {
            "true" => {
                return Some(MirStatement::AssignConstBool {
                    dst,
                    value: true,
                    span,
                })
            }
            "false" => {
                return Some(MirStatement::AssignConstBool {
                    dst,
                    value: false,
                    span,
                })
            }
            _ => {}
        }
    }
    if let Some(inner) = rhs.strip_prefix("discriminant(") {
        if let Some(arg) = inner.strip_suffix(')') {
            if let Some(src) = parse_local_token(arg.trim()) {
                return Some(MirStatement::AssignDiscriminant { dst, src, span });
            }
        }
    }
    if let Some(dc) = parse_downcast_rhs(rhs) {
        return Some(MirStatement::AssignDowncast {
            dst,
            src: dc.src,
            variant: dc.variant,
            field: dc.field,
            span,
        });
    }
    if let Some(cmp) = parse_compare_rhs(rhs) {
        return Some(MirStatement::AssignBinaryCompare {
            dst,
            op: cmp.op,
            lhs: cmp.lhs,
            rhs: cmp.rhs,
            span,
        });
    }
    // `_X = Not(copy _Y);` / `_X = Not(move _Y);` — boolean unary
    // negation. See `MirStatement::AssignUnaryNot` for context.
    if let Some(inner) = rhs.strip_prefix("Not(") {
        if let Some(arg) = inner.strip_suffix(')') {
            if let Some(src) = parse_local_token(arg.trim()) {
                return Some(MirStatement::AssignUnaryNot { dst, src, span });
            }
        }
    }
    None
}

struct CompareRhs {
    op: CompareOp,
    lhs: Operand,
    rhs: Operand,
}

/// Parse a comparison RHS of the form `<Op>(<operand>, <operand>)`
/// where `<Op>` is one of `Eq`, `Ne`, `Lt`, `Le`, `Gt`, `Ge`. Returns
/// `None` if the shape doesn't match.
fn parse_compare_rhs(rhs: &str) -> Option<CompareRhs> {
    // Recognise the leading operator token. The order matters because
    // `Le` is a prefix of `Lt` only when considered character-by-
    // character — we match exact `Op(` to avoid ambiguity.
    let (op, after_op) = if let Some(rest) = rhs.strip_prefix("Eq(") {
        (CompareOp::Eq, rest)
    } else if let Some(rest) = rhs.strip_prefix("Ne(") {
        (CompareOp::Ne, rest)
    } else if let Some(rest) = rhs.strip_prefix("Le(") {
        (CompareOp::Le, rest)
    } else if let Some(rest) = rhs.strip_prefix("Lt(") {
        (CompareOp::Lt, rest)
    } else if let Some(rest) = rhs.strip_prefix("Ge(") {
        (CompareOp::Ge, rest)
    } else {
        let rest = rhs.strip_prefix("Gt(")?;
        (CompareOp::Gt, rest)
    };
    let inner = after_op.strip_suffix(')')?;
    let parts = split_top_level(inner, ',');
    if parts.len() != 2 {
        return None;
    }
    let lhs = parse_operand(parts[0].trim())?;
    let rhs = parse_operand(parts[1].trim())?;
    Some(CompareRhs { op, lhs, rhs })
}

/// Parse a single operand: `copy _<N>`, `move _<N>`, or
/// `const <literal>`.
fn parse_operand(s: &str) -> Option<Operand> {
    if let Some(rest) = s.strip_prefix("copy _") {
        rest.parse::<u32>().ok().map(Operand::Copy)
    } else if let Some(rest) = s.strip_prefix("move _") {
        rest.parse::<u32>().ok().map(Operand::Move)
    } else {
        s.strip_prefix("const ")
            .map(|rest| Operand::Const(rest.to_string()))
    }
}

struct CallTerminator {
    dst: LocalId,
    func_text: String,
    target: BlockId,
}

/// Parse a function-call terminator of the form
/// `_<dst> = <func-expr>(<args>) -> [return: bb<target>, unwind ...]`
/// (the trailing `;` has already been stripped). Returns `None` if the
/// shape doesn't match.
fn parse_call_terminator(s: &str) -> Option<CallTerminator> {
    // The boundary `) -> [` is unambiguous — it separates the call
    // expression (with its closing `)`) from the arm list. Use rfind
    // so nested parens inside the call expression don't trip us up.
    let arrow_idx = s.rfind(") -> [")?;
    let lhs = &s[..=arrow_idx]; // up to and including the `)`
    let arms_str = s[arrow_idx + 6..].strip_suffix(']')?;

    let (dst, func_text) = parse_call_lhs(lhs)?;

    // Find the `return: bb<N>` arm. Other arms (unwind continue,
    // unwind: bb20, etc.) are ignored — Floyd does not follow
    // cleanup paths.
    let mut target: Option<BlockId> = None;
    for arm in split_top_level(arms_str, ',') {
        let arm = arm.trim();
        if let Some(rest) = arm.strip_prefix("return:") {
            let rest = rest.trim().strip_prefix("bb")?;
            target = rest.parse::<u32>().ok();
            break;
        }
    }
    Some(CallTerminator {
        dst,
        func_text,
        target: target?,
    })
}

struct AssertTerminator {
    success: BlockId,
    unwind: Option<BlockId>,
    msg: String,
}

/// Parse an assert terminator of the form
/// `assert(<cond>, "<msg>", <args>..) -> [success: bb<N>, unwind <action>]`
/// (the trailing `;` has already been stripped). `<action>` is one of
/// `continue`, `unreachable`, `terminate(..)` or `: bb<M>`. Returns
/// `None` if the shape doesn't match.
fn parse_assert_terminator(s: &str) -> Option<AssertTerminator> {
    let rest = s.strip_prefix("assert(")?;
    // The arm list is the last `) -> [..]` on the line; the message
    // literal sits before it, so `rfind` cannot land inside it.
    let arrow_idx = rest.rfind(") -> [")?;
    let args = &rest[..arrow_idx];
    let arms_str = rest[arrow_idx + 6..].strip_suffix(']')?;

    let msg = first_string_literal(args)?;

    let mut success: Option<BlockId> = None;
    let mut unwind: Option<BlockId> = None;
    for arm in split_top_level(arms_str, ',') {
        let arm = arm.trim();
        if let Some(t) = arm.strip_prefix("success:") {
            success = Some(t.trim().strip_prefix("bb")?.parse::<u32>().ok()?);
        } else if let Some(t) = arm.strip_prefix("unwind:") {
            unwind = Some(t.trim().strip_prefix("bb")?.parse::<u32>().ok()?);
        }
    }
    Some(AssertTerminator {
        success: success?,
        unwind,
        msg,
    })
}

/// Return the contents of the first double-quoted string literal in
/// `s`, honouring backslash escapes (which are kept verbatim). `None`
/// if `s` has no complete literal.
fn first_string_literal(s: &str) -> Option<String> {
    let open = s.find('"')?;
    let body = &s[open + 1..];
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(body[..i].to_string()),
            _ => {}
        }
    }
    None
}

/// Split `_<dst> = <func-expr>(<args>)` into the destination local
/// and the function expression text (everything up to the opening
/// `(` of the argument list; used by the `?` detector and the panic
/// filter).
fn parse_call_lhs(lhs: &str) -> Option<(LocalId, String)> {
    let eq = lhs.find(" = ")?;
    let dst = lhs[..eq].trim().strip_prefix('_')?.parse::<u32>().ok()?;
    let call_expr = lhs[eq + 3..].trim();
    let func_text = match call_expr.find('(') {
        Some(p) => call_expr[..p].trim().to_string(),
        None => call_expr.to_string(),
    };
    Some((dst, func_text))
}

/// Parse a call terminator with no return edge,
/// `_<dst> = <func-expr>(<args>) -> unwind <action>` or
/// `_<dst> = <func-expr>(<args>) -> bb<cleanup>` (the trailing `;`
/// has already been stripped). Returns `None` for any other shape,
/// including calls that do return (`-> [return: ..]`).
fn parse_noreturn_call_terminator(s: &str) -> Option<(LocalId, String)> {
    let arrow_idx = s.rfind(") -> ")?;
    let edge = &s[arrow_idx + 5..];
    let is_unwind_only = edge.starts_with("unwind ")
        || edge.starts_with("unwind:")
        || edge
            .strip_prefix("bb")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !is_unwind_only {
        return None;
    }
    parse_call_lhs(&s[..=arrow_idx])
}

/// Parse a local token that may carry a `copy`/`move` prefix, returning
/// the bare local id.
fn parse_local_token(tok: &str) -> Option<LocalId> {
    let bare = tok
        .strip_prefix("copy _")
        .or_else(|| tok.strip_prefix("move _"))
        .or_else(|| tok.strip_prefix('_'))?;
    bare.parse::<u32>().ok()
}

/// RHS fields of an `AssignDowncast`.
struct DowncastRhs {
    src: LocalId,
    variant: String,
    field: u32,
}

/// Parse the RHS of a downcast assignment of the form
/// `copy ((_<src> as <Variant>).<field>: <Ty>)`.
///
/// Returns `None` if the shape doesn't match. We only need enough of
/// the structure to know the scrutinee, the variant name and the
/// field index — the field type is preserved verbatim in the original
/// statement text but not surfaced as a typed field by the parser.
fn parse_downcast_rhs(rhs: &str) -> Option<DowncastRhs> {
    let body = rhs.strip_prefix("copy ((")?.strip_suffix(')')?;
    // body is `_<src> as <Variant>).<field>: <Ty>`
    let close_paren = body.find(')')?;
    let head = &body[..close_paren]; // _<src> as <Variant>
    let tail = &body[close_paren + 1..]; // .<field>: <Ty>
    let as_idx = head.find(" as ")?;
    let src = head[..as_idx]
        .trim()
        .strip_prefix('_')?
        .parse::<u32>()
        .ok()?;
    let variant = head[as_idx + 4..].trim().to_string();
    if variant.is_empty() {
        return None;
    }
    let dot_rest = tail.strip_prefix('.')?;
    let colon = dot_rest.find(':')?;
    let field = dot_rest[..colon].trim().parse::<u32>().ok()?;
    Some(DowncastRhs {
        src,
        variant,
        field,
    })
}

fn parse_terminator(line: &str, span: Option<SourceSpan>) -> Option<MirTerminator> {
    let s = line.strip_suffix(';')?;

    if s == "return" {
        return Some(MirTerminator::Return { span });
    }

    if s == "unreachable" {
        return Some(MirTerminator::Unreachable { span });
    }

    if let Some(rest) = s.strip_prefix("goto -> bb") {
        return rest
            .parse::<u32>()
            .ok()
            .map(|target| MirTerminator::Goto { target, span });
    }

    // Before the Call parser: an assert's message text is free-form
    // and must never be mistaken for a `_N = f(..)` call.
    if let Some(assert) = parse_assert_terminator(s) {
        return Some(MirTerminator::Assert {
            success: assert.success,
            unwind: assert.unwind,
            msg: assert.msg,
            span,
        });
    }

    if let Some(call) = parse_call_terminator(s) {
        let _ = span.as_ref(); // keep `span` lifetime explicit for readability
        return Some(MirTerminator::Call {
            dst: call.dst,
            func_text: call.func_text,
            target: call.target,
            span,
        });
    }

    if let Some((dst, func_text)) = parse_noreturn_call_terminator(s) {
        return Some(MirTerminator::NoReturnCall {
            dst,
            func_text,
            span,
        });
    }

    if let Some(rest) = s.strip_prefix("switchInt(") {
        let close = rest.find(") -> [")?;
        let discr_str = rest[..close]
            .strip_prefix("copy _")
            .or_else(|| rest[..close].strip_prefix("move _"))
            .or_else(|| rest[..close].strip_prefix('_'))?;
        let discr = discr_str.parse::<u32>().ok()?;
        let arms_str = rest[close + 6..].strip_suffix(']')?;
        let mut targets = Vec::new();
        let mut otherwise: Option<BlockId> = None;
        for arm in arms_str.split(',') {
            let arm = arm.trim();
            let colon = arm.find(':')?;
            let key = arm[..colon].trim();
            let target = arm[colon + 1..]
                .trim()
                .strip_prefix("bb")?
                .parse::<u32>()
                .ok()?;
            if key == "otherwise" {
                otherwise = Some(target);
            } else if let Ok(v) = key.parse::<u128>() {
                targets.push((v, target));
            }
        }
        return Some(MirTerminator::SwitchInt {
            discr,
            targets,
            otherwise: otherwise?,
            span,
        });
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Span-less `--emit=mir` output for `fn decide(a, b) { a && b }`.
    /// Kept here so the existing test surface that doesn't care about
    /// spans continues to round-trip cleanly.
    const DECIDE_AND_MIR_NO_SPANS: &str = r#"
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

    /// `--emit=mir -Zmir-include-spans` output for the same function.
    /// Captured from rustc nightly 1.97 on 2026-05-23 during ADR-0002
    /// scouting.
    const DECIDE_AND_MIR_WITH_SPANS: &str = r#"
fn decide(_1: bool, _2: bool) -> bool {
    debug a => _1;                       // in scope 0 at /tmp/floyd-span-scout.rs:1:15: 1:16
    debug b => _2;                       // in scope 0 at /tmp/floyd-span-scout.rs:1:24: 1:25
    let mut _0: bool;                    // return place in scope 0 at /tmp/floyd-span-scout.rs:1:36: 1:40

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at /tmp/floyd-span-scout.rs:2:5: 2:6
    }

    bb1: {
        _0 = copy _2;                    // scope 0 at /tmp/floyd-span-scout.rs:2:10: 2:11
        goto -> bb3;                     // scope 0 at /tmp/floyd-span-scout.rs:2:5: 2:11
    }

    bb2: {
        _0 = const false;                // scope 0 at /tmp/floyd-span-scout.rs:2:5: 2:11
        goto -> bb3;                     // scope 0 at /tmp/floyd-span-scout.rs:2:5: 2:11
    }

    bb3: {
        return;                          // scope 0 at /tmp/floyd-span-scout.rs:3:2: 3:2
    }
}
"#;

    // -----------------------------------------------------------------
    // Span-less round-trip (unchanged from previous tests, with new
    // span fields defaulting to None)
    // -----------------------------------------------------------------

    #[test]
    fn parses_decide_fn() {
        let mir = parse_text(DECIDE_AND_MIR_NO_SPANS).expect("parse ok");
        assert_eq!(mir.functions.len(), 1);
        let f = &mir.functions[0];
        assert_eq!(f.name, "decide");
        assert_eq!(f.args.len(), 2);
        assert_eq!(f.args[0].local, 1);
        assert_eq!(f.args[0].ty, "bool");
        assert_eq!(f.return_ty, "bool");
    }

    #[test]
    fn captures_debug_names() {
        let mir = parse_text(DECIDE_AND_MIR_NO_SPANS).expect("parse ok");
        let names = &mir.functions[0].debug_names;
        assert_eq!(names.get(&1), Some(&"a".to_string()));
        assert_eq!(names.get(&2), Some(&"b".to_string()));
    }

    /// Issue #12: `self` is a reserved method-receiver name, never a
    /// Boolean condition. The debug-name intake must drop it so the
    /// decomposer declines method bodies that would otherwise surface
    /// `self` as a condition name.
    #[test]
    fn self_debug_name_is_dropped_from_debug_names() {
        let mir_with_self = r#"
fn is_valid(_1: &Config) -> bool {
    debug self => _1;
    let mut _0: bool;

    bb0: {
        _0 = const true;
        return;
    }
}
"#;
        let mir = parse_text(mir_with_self).expect("parse ok");
        let names = &mir.functions[0].debug_names;
        assert!(
            !names.values().any(|v| v == "self"),
            "self must never appear in debug_names (got {names:?})"
        );
    }

    #[test]
    fn is_reserved_debug_name_covers_self() {
        assert!(is_reserved_debug_name("self"));
        assert!(!is_reserved_debug_name("a"));
        assert!(!is_reserved_debug_name("x"));
        assert!(!is_reserved_debug_name("Self")); // Self is a type alias, not a receiver
    }

    #[test]
    fn parses_all_four_blocks() {
        let mir = parse_text(DECIDE_AND_MIR_NO_SPANS).expect("parse ok");
        let blocks = &mir.functions[0].blocks;
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[0].id, 0);
        assert_eq!(blocks[3].id, 3);
    }

    #[test]
    fn recognises_switchint_in_bb0() {
        let mir = parse_text(DECIDE_AND_MIR_NO_SPANS).expect("parse ok");
        match &mir.functions[0].blocks[0].terminator {
            MirTerminator::SwitchInt {
                discr,
                targets,
                otherwise,
                span,
            } => {
                assert_eq!(*discr, 1);
                assert_eq!(targets, &[(0u128, 2u32)]);
                assert_eq!(*otherwise, 1);
                assert_eq!(*span, None); // no -Zmir-include-spans
            }
            other => panic!("bb0 terminator not SwitchInt: {other:?}"),
        }
    }

    #[test]
    fn recognises_short_circuit_const_false_in_bb2() {
        let mir = parse_text(DECIDE_AND_MIR_NO_SPANS).expect("parse ok");
        let bb2 = mir.functions[0]
            .blocks
            .iter()
            .find(|b| b.id == 2)
            .expect("bb2 present");
        assert!(matches!(
            bb2.statements[0],
            MirStatement::AssignConstBool {
                dst: 0,
                value: false,
                ..
            }
        ));
    }

    #[test]
    fn recognises_copy_b_in_bb1() {
        let mir = parse_text(DECIDE_AND_MIR_NO_SPANS).expect("parse ok");
        let bb1 = mir.functions[0]
            .blocks
            .iter()
            .find(|b| b.id == 1)
            .expect("bb1 present");
        assert!(matches!(
            bb1.statements[0],
            MirStatement::AssignCopy { dst: 0, src: 2, .. }
        ));
    }

    // -----------------------------------------------------------------
    // Span recovery (new in this commit)
    // -----------------------------------------------------------------

    #[test]
    fn span_literal_parses() {
        let s = parse_span_literal("/tmp/floyd-span-scout.rs:2:5: 2:6").expect("span parses");
        assert_eq!(s.file, "/tmp/floyd-span-scout.rs");
        assert_eq!(s.start_line, 2);
        assert_eq!(s.start_col, 5);
        assert_eq!(s.end_line, 2);
        assert_eq!(s.end_col, 6);
    }

    #[test]
    fn span_literal_tolerates_windows_paths_with_colons() {
        // Windows-style path containing a drive letter colon.
        let s = parse_span_literal(r"C:\src\decide.rs:2:5: 2:6").expect("span parses");
        assert_eq!(s.file, r"C:\src\decide.rs");
        assert_eq!(s.start_line, 2);
        assert_eq!(s.end_col, 6);
    }

    #[test]
    fn comment_extraction_finds_span() {
        let s = extract_span_from_comment("scope 0 at /tmp/x.rs:2:5: 2:6").expect("ok");
        assert_eq!(s.start_line, 2);
        let s = extract_span_from_comment("in scope 0 at /tmp/x.rs:2:5: 2:6").expect("ok");
        assert_eq!(s.start_line, 2);
        let s = extract_span_from_comment("return place in scope 0 at /tmp/x.rs:1:36: 1:40")
            .expect("ok");
        assert_eq!(s.start_line, 1);
        assert_eq!(s.end_col, 40);
    }

    #[test]
    fn switchint_span_captured_from_spanned_mir() {
        let mir = parse_text(DECIDE_AND_MIR_WITH_SPANS).expect("parse ok");
        match &mir.functions[0].blocks[0].terminator {
            MirTerminator::SwitchInt { span: Some(s), .. } => {
                assert_eq!(s.start_line, 2);
                assert_eq!(s.start_col, 5);
                assert_eq!(s.end_line, 2);
                assert_eq!(s.end_col, 6);
            }
            other => panic!("expected SwitchInt with span, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // `if let` shapes
    // -----------------------------------------------------------------

    /// `--emit=mir -Zmir-include-spans` output for
    /// `fn decide(opt: Option<bool>) -> bool { if let Some(x) = opt { x } else { false } }`.
    /// Captured from rustc nightly 1.97 on 2026-05-24.
    const IF_LET_SIMPLE_MIR: &str = r#"
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

    // -----------------------------------------------------------------
    // Closure-capture parsing and name propagation
    // -----------------------------------------------------------------

    /// Closure body MIR for `|x: bool| x && b` where `b` is captured
    /// by reference (the default Fn capture). Engine should recover
    /// `x && b` as the decision once the parser propagates the
    /// capture name into `_3` and through the `*_3` deref.
    const CLOSURE_BY_REF_BODY: &str = r#"
fn outer::{closure#0}(_1: &{closure}, _2: bool) -> bool {
    debug x => _2;
    debug b => (*((*_1).0: &bool));
    let mut _0: bool;
    let mut _3: &bool;

    bb0: {
        switchInt(copy _2) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _3 = no_retag copy ((*_1).0: &bool);
        _0 = copy (*_3);
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

    /// Closure body for `move |x: bool| x && b` (by-value capture).
    /// Captures appear as `((*_1).0: bool)` in the debug
    /// annotation and as direct field reads in the body.
    const CLOSURE_BY_VALUE_BODY: &str = r#"
fn outer::{closure#0}(_1: &{closure}, _2: bool) -> bool {
    debug x => _2;
    debug b => ((*_1).0: bool);
    let mut _0: bool;

    bb0: {
        switchInt(copy _2) -> [0: bb2, otherwise: bb1];
    }

    bb1: {
        _0 = copy ((*_1).0: bool);
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

    /// Closure body capturing two values used in a 3-condition AND.
    /// Captured from real rustc nightly 1.97 MIR; rustc assigns
    /// distinct intermediate locals (`_4`, `_5`) to each capture's
    /// reference, which the propagation pass relies on.
    const CLOSURE_TWO_CAPTURES_BODY: &str = r#"
fn outer::{closure#0}(_1: &{closure}, _2: bool) -> bool {
    debug x => _2;
    debug b => (*((*_1).0: &bool));
    debug c => (*((*_1).1: &bool));
    let mut _0: bool;
    let mut _3: bool;
    let mut _4: &bool;
    let mut _5: &bool;

    bb0: {
        switchInt(copy _2) -> [0: bb3, otherwise: bb1];
    }

    bb1: {
        _4 = no_retag copy ((*_1).0: &bool);
        _3 = copy (*_4);
        switchInt(move _3) -> [0: bb3, otherwise: bb2];
    }

    bb2: {
        _5 = no_retag copy ((*_1).1: &bool);
        _0 = copy (*_5);
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

    #[test]
    fn parses_by_ref_capture_annotation() {
        let mir = parse_text(CLOSURE_BY_REF_BODY).expect("parse ok");
        let f = &mir.functions[0];
        assert_eq!(f.captures.len(), 1);
        let c = &f.captures[0];
        assert_eq!(c.name, "b");
        assert_eq!(c.env_local, 1);
        assert_eq!(c.field, 0);
        assert!(c.by_ref);
    }

    #[test]
    fn parses_by_value_capture_annotation() {
        let mir = parse_text(CLOSURE_BY_VALUE_BODY).expect("parse ok");
        let f = &mir.functions[0];
        assert_eq!(f.captures.len(), 1);
        let c = &f.captures[0];
        assert_eq!(c.name, "b");
        assert_eq!(c.env_local, 1);
        assert_eq!(c.field, 0);
        assert!(!c.by_ref);
    }

    #[test]
    fn propagates_by_ref_capture_through_deref() {
        // `_3 = no_retag copy ((*_1).0: &bool)` aliases `_3` to the
        // reference; `_0 = copy (*_3)` then names `_0` after the
        // captured `b`.
        let mir = parse_text(CLOSURE_BY_REF_BODY).expect("parse ok");
        let names = &mir.functions[0].debug_names;
        assert_eq!(names.get(&0), Some(&"b".to_string()));
        assert_eq!(names.get(&2), Some(&"x".to_string()));
    }

    #[test]
    fn propagates_by_value_capture_directly() {
        // `_0 = copy ((*_1).0: bool)` is a direct field read — `_0`
        // takes the captured name immediately, no deref pass needed.
        let mir = parse_text(CLOSURE_BY_VALUE_BODY).expect("parse ok");
        let names = &mir.functions[0].debug_names;
        assert_eq!(names.get(&0), Some(&"b".to_string()));
        assert_eq!(names.get(&2), Some(&"x".to_string()));
    }

    #[test]
    fn propagates_two_captures_independently() {
        let mir = parse_text(CLOSURE_TWO_CAPTURES_BODY).expect("parse ok");
        let names = &mir.functions[0].debug_names;
        // _3 = copy (*_4) where _4 alternately aliases capture
        // field 0 (`b`) or capture field 1 (`c`). The propagation
        // pass iterates to a fixed point; the LAST aliasing of _4
        // wins for naming, so we check both b and c are propagated
        // to some local at minimum.
        let propagated: std::collections::BTreeSet<&str> =
            names.values().map(String::as_str).collect();
        assert!(propagated.contains("b"));
        assert!(propagated.contains("c"));
        assert!(propagated.contains("x"));
    }

    #[test]
    fn parses_assign_discriminant() {
        let mir = parse_text(IF_LET_SIMPLE_MIR).expect("parse ok");
        let bb0 = mir.functions[0]
            .blocks
            .iter()
            .find(|b| b.id == 0)
            .expect("bb0");
        match &bb0.statements[0] {
            MirStatement::AssignDiscriminant { dst, src, .. } => {
                assert_eq!(*dst, 2);
                assert_eq!(*src, 1);
            }
            other => panic!("expected AssignDiscriminant, got {other:?}"),
        }
    }

    #[test]
    fn parses_assign_downcast() {
        let mir = parse_text(IF_LET_SIMPLE_MIR).expect("parse ok");
        let bb1 = mir.functions[0]
            .blocks
            .iter()
            .find(|b| b.id == 1)
            .expect("bb1");
        match &bb1.statements[0] {
            MirStatement::AssignDowncast {
                dst,
                src,
                variant,
                field,
                ..
            } => {
                assert_eq!(*dst, 3);
                assert_eq!(*src, 1);
                assert_eq!(variant, "Some");
                assert_eq!(*field, 0);
            }
            other => panic!("expected AssignDowncast, got {other:?}"),
        }
    }

    #[test]
    fn parses_unreachable_terminator() {
        let mir = parse_text(IF_LET_SIMPLE_MIR).expect("parse ok");
        let bb4 = mir.functions[0]
            .blocks
            .iter()
            .find(|b| b.id == 4)
            .expect("bb4");
        assert!(matches!(bb4.terminator, MirTerminator::Unreachable { .. }));
    }

    // -----------------------------------------------------------------
    // `?` operator shape (captured from rustc nightly 1.97, 2026-05-24)
    // -----------------------------------------------------------------

    const TRY_OPTION_MIR: &str = r#"
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

    /// `rustc +nightly --emit=mir -Zmir-include-spans -C overflow-checks=on
    /// -C debug-assertions=on` output (nightly 1.101, 2026-10-28) for
    ///
    /// ```text
    /// pub fn add_checked(a: u32, b: u32) -> u32 {
    ///     debug_assert!(a < 1000);
    ///     a + b
    /// }
    /// ```
    ///
    /// `bb2` ends in the overflow-check `assert` terminator; `bb1` is
    /// the `debug_assert!` failure path (a diverging panic call).
    const ASSERT_OVERFLOW_MIR: &str = r#"
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

    /// `rustc +nightly --emit=mir -C overflow-checks=on` output
    /// (nightly 1.101, comment lines stripped) for
    ///
    /// ```text
    /// pub fn idx(v: Vec<u32>, i: usize) -> u32 {
    ///     let a = [1u32, 2, 3];
    ///     a[i] + v.len() as u32
    /// }
    /// ```
    ///
    /// A bounds-check `assert` and an overflow-check `assert`, both
    /// unwinding to the cleanup block `bb5` because `v` needs a drop.
    const ASSERT_UNWIND_CLEANUP_MIR: &str = r#"
fn idx(_1: Vec<u32>, _2: usize) -> u32 {
    debug v => _1;
    debug i => _2;
    let mut _0: u32;
    let _3: [u32; 3];
    let mut _4: u32;
    let mut _5: bool;
    let mut _6: u32;
    let mut _7: usize;
    let mut _8: &std::vec::Vec<u32>;
    let mut _9: (u32, bool);
    scope 1 {
        debug a => _3;
    }

    bb0: {
        _3 = [const 1_u32, const 2_u32, const 3_u32];
        _5 = Lt(copy _2, const 3_usize);
        assert(move _5, "index out of bounds: the length is {} but the index is {}", const 3_usize, copy _2) -> [success: bb1, unwind: bb5];
    }

    bb1: {
        _4 = copy _3[_2];
        _8 = &_1;
        _7 = Vec::<u32>::len(move _8) -> [return: bb2, unwind: bb5];
    }

    bb2: {
        _6 = move _7 as u32 (IntToInt);
        _9 = AddWithOverflow(copy _4, copy _6);
        assert(!move (_9.1: bool), "attempt to compute `{} + {}`, which would overflow", move _4, move _6) -> [success: bb3, unwind: bb5];
    }

    bb3: {
        _0 = move (_9.0: u32);
        drop(_1) -> [return: bb4, unwind continue];
    }

    bb4: {
        return;
    }

    bb5 (cleanup): {
        drop(_1) -> [return: bb6, unwind terminate(cleanup)];
    }

    bb6 (cleanup): {
        resume;
    }
}
"#;

    fn block(f: &MirFunction, id: BlockId) -> &MirBlock {
        f.blocks.iter().find(|b| b.id == id).expect("block exists")
    }

    #[test]
    fn parses_overflow_assert_terminator() {
        let mir = parse_text(ASSERT_OVERFLOW_MIR).expect("parse ok");
        let f = &mir.functions[0];
        let bb2 = block(f, 2);
        match &bb2.terminator {
            MirTerminator::Assert {
                success,
                unwind,
                msg,
                span,
            } => {
                assert_eq!(*success, 3);
                assert_eq!(*unwind, None, "`unwind continue` names no block");
                assert_eq!(msg, "attempt to compute `{} + {}`, which would overflow");
                let span = span.as_ref().expect("span recovered");
                assert_eq!((span.start_line, span.start_col), (3, 5));
            }
            other => panic!("expected Assert, got {other:?}"),
        }
        // The terminator line must not also leak into the statements.
        assert!(
            bb2.statements.iter().all(
                |s| !matches!(s, MirStatement::Other { text, .. } if text.contains("assert("))
            ),
            "assert line kept as a statement: {:?}",
            bb2.statements
        );
    }

    #[test]
    fn parses_assert_terminator_with_cleanup_unwind() {
        let mir = parse_text(ASSERT_UNWIND_CLEANUP_MIR).expect("parse ok");
        let f = &mir.functions[0];
        match &block(f, 0).terminator {
            MirTerminator::Assert {
                success,
                unwind,
                msg,
                ..
            } => {
                assert_eq!(*success, 1);
                assert_eq!(*unwind, Some(5));
                assert_eq!(
                    msg,
                    "index out of bounds: the length is {} but the index is {}"
                );
            }
            other => panic!("expected bounds-check Assert, got {other:?}"),
        }
        assert!(matches!(
            block(f, 2).terminator,
            MirTerminator::Assert {
                success: 3,
                unwind: Some(5),
                ..
            }
        ));
    }

    #[test]
    fn assert_parser_rejects_non_assert_shapes() {
        assert!(parse_assert_terminator("goto -> bb1").is_none());
        // No `success` arm.
        assert!(parse_assert_terminator("assert(move _5, \"m\") -> [unwind continue]").is_none());
        // Message literal with an escaped quote and a comma survives.
        let a = parse_assert_terminator(
            "assert(move _5, \"a \\\"b\\\", c\", copy _2) -> [success: bb7, unwind unreachable]",
        )
        .expect("parses");
        assert_eq!(a.success, 7);
        assert_eq!(a.unwind, None);
        assert_eq!(a.msg, "a \\\"b\\\", c");
    }

    #[test]
    fn parses_noreturn_call_terminators() {
        // `-> unwind continue`: the `debug_assert!` failure call.
        let mir = parse_text(ASSERT_OVERFLOW_MIR).expect("parse ok");
        match &block(&mir.functions[0], 1).terminator {
            MirTerminator::NoReturnCall { dst, func_text, .. } => {
                assert_eq!(*dst, 4);
                assert_eq!(func_text, "core::panicking::panic");
            }
            other => panic!("expected NoReturnCall, got {other:?}"),
        }
        // `-> bb<cleanup>`: `panic!("big")` in a fn holding a `String`
        // (rustc nightly 1.101 output).
        let t = parse_terminator(
            "_4 = std::rt::begin_panic::<&str>(const \"big\") -> bb5;",
            None,
        );
        assert!(
            matches!(
                &t,
                Some(MirTerminator::NoReturnCall { dst: 4, func_text, .. })
                    if func_text == "std::rt::begin_panic::<&str>"
            ),
            "got {t:?}"
        );
        // A returning call is still a `Call`, not a `NoReturnCall`.
        assert!(matches!(
            parse_terminator(
                "_7 = Vec::<u32>::len(move _8) -> [return: bb2, unwind: bb5];",
                None
            ),
            Some(MirTerminator::Call { target: 2, .. })
        ));
    }

    #[test]
    fn parses_call_terminator() {
        let mir = parse_text(TRY_OPTION_MIR).expect("parse ok");
        let bb0 = mir.functions[0]
            .blocks
            .iter()
            .find(|b| b.id == 0)
            .expect("bb0");
        match &bb0.terminator {
            MirTerminator::Call {
                dst,
                func_text,
                target,
                ..
            } => {
                assert_eq!(*dst, 2);
                assert_eq!(func_text, "<Option<bool> as Try>::branch");
                assert_eq!(*target, 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    /// `--emit=mir` for
    /// `fn decide(opt: Option<bool>) -> Option<bool> { let x = opt?; Some(x) }`.
    /// Has both `debug x => _4` (user) and `debug val => _4`
    /// (`?` desugaring) for the same local. The decomposer relies
    /// on first-wins so the user-facing name survives.
    const FIRST_WINS_DEBUG_MIR: &str = r#"
fn decide(_1: Option<bool>) -> Option<bool> {
    debug opt => _1;
    let _4: bool;
    scope 1 {
        debug x => _4;
    }
    scope 4 {
        debug val => _4;
    }

    bb0: {
        return;
    }
}
"#;

    // -----------------------------------------------------------------
    // Comparison-operator shapes
    // -----------------------------------------------------------------

    const COMPARE_GT_AND_MIR: &str = r#"
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

    #[test]
    fn parses_assign_binary_compare_gt() {
        let mir = parse_text(COMPARE_GT_AND_MIR).expect("parse ok");
        let bb0 = mir.functions[0]
            .blocks
            .iter()
            .find(|b| b.id == 0)
            .expect("bb0");
        match &bb0.statements[0] {
            MirStatement::AssignBinaryCompare {
                dst, op, lhs, rhs, ..
            } => {
                assert_eq!(*dst, 3);
                assert_eq!(*op, CompareOp::Gt);
                assert_eq!(lhs, &Operand::Copy(1));
                assert_eq!(rhs, &Operand::Const("50_i32".to_string()));
            }
            other => panic!("expected AssignBinaryCompare, got {other:?}"),
        }
    }

    #[test]
    fn compare_op_as_source_str_covers_all_six() {
        assert_eq!(CompareOp::Eq.as_source_str(), "==");
        assert_eq!(CompareOp::Ne.as_source_str(), "!=");
        assert_eq!(CompareOp::Lt.as_source_str(), "<");
        assert_eq!(CompareOp::Le.as_source_str(), "<=");
        assert_eq!(CompareOp::Gt.as_source_str(), ">");
        assert_eq!(CompareOp::Ge.as_source_str(), ">=");
    }

    #[test]
    fn parses_compare_with_two_var_operands() {
        let src = r#"
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
        let mir = parse_text(src).expect("parse ok");
        let bb0 = &mir.functions[0].blocks[0];
        match &bb0.statements[0] {
            MirStatement::AssignBinaryCompare {
                dst, op, lhs, rhs, ..
            } => {
                assert_eq!(*dst, 0);
                assert_eq!(*op, CompareOp::Lt);
                assert_eq!(lhs, &Operand::Copy(1));
                assert_eq!(rhs, &Operand::Copy(2));
            }
            other => panic!("expected AssignBinaryCompare, got {other:?}"),
        }
    }

    #[test]
    fn parses_all_six_compare_ops() {
        // Each op as a one-statement bb to confirm prefix matching.
        for (op_str, expected_op) in [
            ("Eq", CompareOp::Eq),
            ("Ne", CompareOp::Ne),
            ("Lt", CompareOp::Lt),
            ("Le", CompareOp::Le),
            ("Gt", CompareOp::Gt),
            ("Ge", CompareOp::Ge),
        ] {
            let src = format!(
                "fn f(_1: i32) -> bool {{
    let mut _0: bool;
    bb0: {{
        _0 = {op_str}(copy _1, const 0_i32);
        return;
    }}
}}\n"
            );
            let mir = parse_text(&src).expect("parse ok");
            match &mir.functions[0].blocks[0].statements[0] {
                MirStatement::AssignBinaryCompare { op, .. } => assert_eq!(*op, expected_op),
                other => panic!("op {op_str}: expected AssignBinaryCompare, got {other:?}"),
            }
        }
    }

    #[test]
    fn operand_const_display_strips_type_suffix() {
        assert_eq!(
            Operand::Const("50_i32".to_string()).const_display_value(),
            "50"
        );
        assert_eq!(
            Operand::Const("0_u32".to_string()).const_display_value(),
            "0"
        );
        assert_eq!(
            Operand::Const("1_u8".to_string()).const_display_value(),
            "1"
        );
        // No type suffix (e.g. a bare literal) — return as-is.
        assert_eq!(
            Operand::Const("true".to_string()).const_display_value(),
            "true"
        );
    }

    #[test]
    fn debug_name_first_wins_when_local_aliased() {
        // Regression: `?` desugaring emits `debug val => _N` in a
        // later scope for the same local the user named `x`. The
        // user's name must survive both.
        let mir = parse_text(FIRST_WINS_DEBUG_MIR).expect("parse ok");
        assert_eq!(mir.functions[0].debug_names.get(&4), Some(&"x".to_string()));
    }

    #[test]
    fn debug_names_inside_scope_blocks_are_captured() {
        // Regression: `scope N { ... }` bodies used to be swallowed,
        // dropping `debug` declarations for bindings introduced by
        // `if let` / `match`. The decomposer needs those names to
        // recover Condition leaves for the bound value.
        let mir = parse_text(IF_LET_SIMPLE_MIR).expect("parse ok");
        let names = &mir.functions[0].debug_names;
        assert_eq!(names.get(&1), Some(&"opt".to_string()));
        assert_eq!(names.get(&3), Some(&"x".to_string()));
    }

    #[test]
    fn fn_header_with_nested_parens_in_arg_type() {
        // Regression: `Result<bool, ()>` carries a `()` inside the
        // generic args; the old parser took the first `)` it saw
        // and aborted on the malformed remainder.
        let src = "fn decide(_1: Result<bool, ()>) -> bool {\n    bb0: { return; }\n}\n";
        let mir = parse_text(src).expect("parse ok");
        assert_eq!(mir.functions.len(), 1);
        let f = &mir.functions[0];
        assert_eq!(f.name, "decide");
        assert_eq!(f.args.len(), 1);
        assert_eq!(f.args[0].local, 1);
        assert_eq!(f.args[0].ty, "Result<bool, ()>");
        assert_eq!(f.return_ty, "bool");
    }

    /// Real headers from base64 (`decode_helper`, `encode_helper`,
    /// `do_roundtrip_test`) whose argument types carry a `->` return
    /// type: the `>` of the arrow was taken as a closing angle
    /// bracket, so the args list never balanced and the function was
    /// reported as unrecognised.
    #[test]
    fn fn_header_with_arrow_in_impl_fn_arg() {
        let cases: &[(&str, &str, &[&str], &str)] = &[
            (
                "fn decode_helper(_1: &[u8], _7: DecodePaddingMode, _8: impl FnOnce(&[u8], usize, &mut [u8]) -> (usize, usize)) -> Result<DecodeMetadata, decode::DecodeSliceError> {",
                "decode_helper",
                &["&[u8]", "DecodePaddingMode", "impl FnOnce(&[u8], usize, &mut [u8]) -> (usize, usize)"],
                "Result<DecodeMetadata, decode::DecodeSliceError>",
            ),
            (
                "fn encode_helper(_1: &[u8; 64], _4: impl FnOnce(&[u8], &mut [u8]) -> (usize, usize)) -> usize {",
                "encode_helper",
                &["&[u8; 64]", "impl FnOnce(&[u8], &mut [u8]) -> (usize, usize)"],
                "usize",
            ),
            (
                "fn do_roundtrip_test(_1: impl Fn(&mut rngs::SmallRng) -> E::Engine) -> () {",
                "do_roundtrip_test",
                &["impl Fn(&mut rngs::SmallRng) -> E::Engine"],
                "()",
            ),
        ];
        for (header, name, arg_tys, ret) in cases {
            let src = format!("{header}\n    bb0: {{ return; }}\n}}\n");
            let mir = parse_text(&src).expect("parse ok");
            assert!(mir.parse_problems().is_empty(), "{header}");
            assert_eq!(mir.functions.len(), 1, "{header}");
            let f = &mir.functions[0];
            assert_eq!(f.name, *name);
            let tys: Vec<&str> = f.args.iter().map(|a| a.ty.as_str()).collect();
            assert_eq!(tys, *arg_tys);
            assert_eq!(f.return_ty, *ret);
        }
    }

    /// Fn pointer, `for<'a>` binder and `dyn Fn` argument types, and
    /// an arrow inside generic args, next to nested `>>` closes.
    #[test]
    fn fn_header_with_fn_pointer_dyn_and_binder_arg_types() {
        let src = "fn f(_1: for<'a> fn(&'a str) -> Result<&'a str, ()>, _2: &dyn Fn(u8) -> Option<Vec<u8>>, _3: Box<dyn FnMut(&mut [u8]) -> usize + Send>, _4: Option<fn() -> u8>) -> for<'b> fn(&'b u8) -> &'b u8 {\n    bb0: { return; }\n}\n";
        let mir = parse_text(src).expect("parse ok");
        assert!(mir.parse_problems().is_empty());
        let f = &mir.functions[0];
        let tys: Vec<&str> = f.args.iter().map(|a| a.ty.as_str()).collect();
        assert_eq!(
            tys,
            [
                "for<'a> fn(&'a str) -> Result<&'a str, ()>",
                "&dyn Fn(u8) -> Option<Vec<u8>>",
                "Box<dyn FnMut(&mut [u8]) -> usize + Send>",
                "Option<fn() -> u8>",
            ]
        );
        assert_eq!(f.return_ty, "for<'b> fn(&'b u8) -> &'b u8");
    }

    #[test]
    fn parses_switchint_with_three_arms() {
        let mir = parse_text(IF_LET_SIMPLE_MIR).expect("parse ok");
        let bb0 = mir.functions[0]
            .blocks
            .iter()
            .find(|b| b.id == 0)
            .expect("bb0");
        match &bb0.terminator {
            MirTerminator::SwitchInt {
                discr,
                targets,
                otherwise,
                ..
            } => {
                assert_eq!(*discr, 2);
                assert_eq!(targets, &[(1u128, 1u32), (0u128, 2u32)]);
                assert_eq!(*otherwise, 4);
            }
            other => panic!("expected SwitchInt, got {other:?}"),
        }
    }

    #[test]
    fn statement_span_captured_from_spanned_mir() {
        let mir = parse_text(DECIDE_AND_MIR_WITH_SPANS).expect("parse ok");
        let bb1 = mir.functions[0]
            .blocks
            .iter()
            .find(|b| b.id == 1)
            .expect("bb1");
        match &bb1.statements[0] {
            MirStatement::AssignCopy {
                dst: 0,
                src: 2,
                deref: false,
                span: Some(s),
            } => {
                // location of `b` in `a && b`
                assert_eq!(s.start_line, 2);
                assert_eq!(s.start_col, 10);
                assert_eq!(s.end_col, 11);
            }
            other => panic!("expected AssignCopy{{dst:0,src:2,span:Some}}, got {other:?}"),
        }
    }

    /// Two functions separated by the allocation dump rustc prints
    /// for a `&str` constant. The dump's ASCII column ends in `\u{`
    /// (regex's `src/regex/bytes.rs` test `test_debug_output_ascii_escape`
    /// produces exactly this), so the line ends with `{`.
    const ALLOC_DUMP_BETWEEN_FNS_MIR: &str = r#"
fn bytes::f(_1: bool) -> bool {
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb1, otherwise: bb2];
    }

    bb1: {
        _0 = const false;
        return;
    }

    bb2: {
        _0 = const true;
        return;
    }
}

alloc146 (size: 88, align: 1) {
    0x00 │ 4d 61 74 63 68 20 7b 20 73 74 61 72 74 3a 20 30 │ Match { start: 0
    0x30 │ 21 5c 6e 54 68 69 73 20 69 73 20 61 20 5c 75 7b │ !\nThis is a \u{
    0x40 │ 31 62 7d 5b 33 31 6d 74 65 73 74 5c 75 7b 31 62 │ 1b}[31mtest\u{1b
    0x50 │ 7d 5b 30 6d 2e 22 20 7d                         │ }[0m." }
}

fn string::f(_1: bool) -> bool {
    let mut _0: bool;

    bb0: {
        switchInt(copy _1) -> [0: bb1, otherwise: bb2];
    }

    bb1: {
        _0 = const false;
        return;
    }

    bb2: {
        _0 = const true;
        return;
    }
}
"#;

    #[test]
    fn braces_in_an_alloc_dump_do_not_swallow_the_rest_of_the_file() {
        let mir = parse_text(ALLOC_DUMP_BETWEEN_FNS_MIR).expect("parse ok");
        let names: Vec<&str> = mir.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["bytes::f", "string::f"]);
        assert_eq!(mir.functions[1].blocks.len(), 3);
        assert!(mir.unterminated_item.is_none());
        assert!(mir.parse_problems().is_empty());
    }

    #[test]
    fn decisionless_fn_after_an_alloc_dump_is_still_stripped() {
        let mut body = String::from("fn g() -> () {\n    let mut _0: ();\n\n    bb0: {\n");
        for _ in 0..60 {
            body.push_str("        nop;\n");
        }
        body.push_str("        return;\n    }\n}\n");
        let input = format!("{ALLOC_DUMP_BETWEEN_FNS_MIR}\n{body}");
        let mir = parse_text(&input).expect("parse ok");
        assert_eq!(mir.functions.len(), 3);
        assert_eq!(mir.decisionless_functions, 1);
    }

    #[test]
    fn alloc_dump_header_shapes() {
        assert!(is_alloc_dump_header("alloc196 (size: 16, align: 8) {"));
        assert!(is_alloc_dump_header(
            "alloc7 (static: FOO, size: 4, align: 4) {"
        ));
        assert!(!is_alloc_dump_header("alloc (size: 16, align: 8) {"));
        assert!(!is_alloc_dump_header("allocate::f() -> () {"));
        assert!(!is_alloc_dump_header("alloc196 (size: 16, align: 8)"));
    }

    /// An async block's MIR opens with its coroutine layout, a
    /// brace block that is neither a scope nor a basic block.
    const ASYNC_BLOCK_WITH_COROUTINE_LAYOUT_MIR: &str = r#"
fn t::{closure#0}(_1: Pin<&mut {async block@t.rs:1:1: 1:9}>, _2: &mut std::task::Context<'_>) -> Poll<i32> {
    coroutine layout {
        variant_fields = {
            Unresumed(0): [],            // in scope 0 at t.rs:1:1: 1:1
            Returned (1): [],            // in scope 0 at t.rs:1:9: 1:9
        }
        storage_conflicts = BitMatrix(0x0) {}
    }
    debug _task_context => _5;
    let mut _0: std::task::Poll<i32>;
    let mut _3: u32;

    bb0: {
        switchInt(move _3) -> [0: bb1, otherwise: bb2];
    }

    bb1: {
        return;
    }

    bb2: {
        unreachable;
    }
}

fn after() -> () {
    bb0: {
        return;
    }
}
"#;

    #[test]
    fn coroutine_layout_block_is_skipped() {
        let mir = parse_text(ASYNC_BLOCK_WITH_COROUTINE_LAYOUT_MIR).expect("parse ok");
        let names: Vec<&str> = mir.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["t::{closure#0}", "after"]);
        assert_eq!(mir.functions[0].blocks.len(), 3);
        assert!(mir.parse_problems().is_empty());
    }

    #[test]
    fn unterminated_item_is_reported_not_silently_dropped() {
        let input = "fn a(_1: bool) -> bool {\n    bb0: {\n        return;\n    }\n}\n\
                     const X::promoted[0]: &str = {\n    bb0: {\n        return;\n    }\n\n\
                     fn b(_1: bool) -> bool {\n    bb0: {\n        return;\n    }\n}\n";
        let mir = parse_text(input).expect("parse ok");
        assert_eq!(mir.functions.len(), 1);
        assert_eq!(
            mir.unterminated_item,
            Some(UnterminatedItem {
                header: "const X::promoted[0]: &str = {".to_string(),
                functions_lost: 1,
            })
        );
        let problems = mir.parse_problems();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("no closing brace"), "{problems:?}");
    }
}
