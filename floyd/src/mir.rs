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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// `_<dst> = copy _<src>;`
    AssignCopy {
        /// Destination local.
        dst: LocalId,
        /// Source local.
        src: LocalId,
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

/// Parse an `--emit=mir` text dump into a [`Mir`].
///
/// Phase 0 scope: function headers, `debug` annotations, `let` locals,
/// blocks with `copy`/`const bool` assignments, and `return` / `goto` /
/// `switchInt` terminators. Other shapes pass through as
/// [`MirStatement::Other`] / [`MirTerminator::Other`].
///
/// When the MIR was emitted with `-Zmir-include-spans`, the parser
/// also captures inline source-span comments of the form
/// `// scope <N> at <file>:<line>:<col>: <line>:<col>` and attaches
/// them to the corresponding statement / terminator. Span-less MIR is
/// also accepted; the `span` fields are then `None`.
pub fn parse_text(input: &str) -> Result<Mir, ParseError> {
    let mut mir = Mir::default();
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
    let mut current_fn: Option<MirFunction> = None;
    let mut current_block: Option<MirBlock> = None;

    for (i, raw) in input.lines().enumerate() {
        let line_no = i + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Lines that are *entirely* comments (preamble warnings, etc.)
        // are skipped. Lines with a trailing comment are split below.
        if trimmed.starts_with("//") {
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
            skip_depth = 1;
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
                let f = parse_fn_header(code).ok_or_else(|| ParseError {
                    line: line_no,
                    message: format!("expected fn header, got: {code}"),
                })?;
                current_fn = Some(f);
                depth = 1;
                continue;
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
                            f.debug_names.entry(local).or_insert(name);
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

    Ok(mir)
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

/// Find the matching closing bracket for an open bracket that is
/// considered to sit just before `s`. Returns the index inside `s` of
/// the matching close. Honours nested `()`, `<>`, `[]` and `{}`.
fn find_matching_close(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth: i32 = 1;
    for (i, c) in s.char_indices() {
        match c {
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
/// appear inside balanced `()`, `<>`, `[]` or `{}`.
fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
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
            return Some(MirStatement::AssignCopy { dst, src, span });
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
                return Some(MirStatement::AssignCopy { dst, src, span });
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
    } else if let Some(rest) = rhs.strip_prefix("Gt(") {
        (CompareOp::Gt, rest)
    } else {
        return None;
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

    let eq = lhs.find(" = ")?;
    let dst = lhs[..eq].trim().strip_prefix('_')?.parse::<u32>().ok()?;
    let call_expr = lhs[eq + 3..].trim();
    // Capture the function expression text (everything up to the
    // opening `(` of the argument list). Used by the `?` detector.
    let func_text = match call_expr.find('(') {
        Some(p) => call_expr[..p].trim().to_string(),
        None => call_expr.to_string(),
    };

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

    if let Some(call) = parse_call_terminator(s) {
        let _ = span.as_ref(); // keep `span` lifetime explicit for readability
        return Some(MirTerminator::Call {
            dst: call.dst,
            func_text: call.func_text,
            target: call.target,
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
}
