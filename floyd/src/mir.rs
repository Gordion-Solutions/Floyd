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
    /// Basic blocks in source order.
    pub blocks: Vec<MirBlock>,
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
    /// A statement shape Phase 0 doesn't yet parse. Preserved verbatim.
    Other {
        /// Original text of the statement.
        text: String,
        /// Source span, if present in the MIR.
        span: Option<SourceSpan>,
    },
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
                if let Some(f) = current_fn.take() {
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
                    if let Some((name, local)) = parse_debug(code) {
                        // First-wins: rustc emits desugaring bindings
                        // for the same local in later scopes (e.g. the
                        // `val` binding the `?` operator synthesizes for
                        // a local the user named `x`). The user's name
                        // is always first in source order.
                        f.debug_names.entry(local).or_insert(name);
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

fn parse_debug(line: &str) -> Option<(String, LocalId)> {
    // debug a => _1;
    let s = line.strip_prefix("debug ")?.strip_suffix(';')?;
    let arrow = s.find("=>")?;
    let name = s[..arrow].trim().to_string();
    let local = s[arrow + 2..]
        .trim()
        .strip_prefix('_')?
        .parse::<u32>()
        .ok()?;
    Some((name, local))
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
    None
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
