//! Which declined functions contain no Boolean decision at all.
//!
//! The decomposer declines every function it cannot read as one
//! decision. On real crates most of those functions contain no
//! decision the user wrote: their `switchInt`s come from a standard
//! library macro (`assert_eq!`), a derived impl, `?` or a `for` loop,
//! a plain enum `match`, an assertion, or drop flags. Reporting them
//! as an "unsupported shape" overstates what Floyd cannot analyse.
//!
//! [`classify_no_decision`] labels such a function, and only on
//! positive evidence for every `switchInt` it has. A function with
//! anything it cannot account for (a user `if`, `&&`, `||` or
//! `while`, an integer `match`, a `switchInt` the parser kept as raw
//! text, an unresolved discriminant) gets no label and keeps its
//! decomposer reason.
//!
//! The evidence:
//!
//! - **Branch markers.** rustc records a lower-case `coverage branch`
//!   marker ([`MirFunction::branch_markers`]) for every Boolean
//!   condition the source writes (`if`, `while`, `if let`, guards and
//!   each `&&` / `||` operand), but not for `match` arms, `?`, a
//!   `for` loop's `next()` test or drop flags. A marker in the
//!   workspace's own code disqualifies the function unless it lies
//!   inside an `assert!`-family or `ensure!` call.
//! - **Per `switchInt`**, one of: a drop flag (its discriminant is
//!   only ever assigned `const true` / `const false`); a standard
//!   library or dependency macro (the `switchInt` and every write of
//!   its discriminant carry spans in that code); `?` or a `for` loop
//!   (an enum discriminant of a `Try::branch` or `Iterator::next`
//!   result); an enum `match` (any other enum discriminant); an
//!   assertion (the `switchInt` lies inside an `assert!` call in the
//!   source).
//! - **Derived impls**: the function's `<impl at FILE:L:C ..>` points
//!   into a `#[derive(..)]` attribute in the source.

use crate::mir::{LocalId, MirFunction, MirStatement, MirTerminator, SourceSpan};

/// Why a function with a `switchInt` contains no Boolean decision
/// the user wrote (the `detail` of a `no_decision` `out_of_scope`
/// entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoDecisionDetail {
    /// Only `switchInt`s from standard-library or dependency macro
    /// expansions (`assert_eq!`, `assert_ne!`, `panic!` arguments,
    /// `matches!` internals over macro-local values).
    StdMacroOnly,
    /// Enum / `Option` / `Result` `match` or `if let` without
    /// `if let`-style branch markers: a pattern match, not a Boolean
    /// condition (supported decisions over patterns are a later lift).
    PatternMatchOnly,
    /// The body of a `#[derive(..)]` impl.
    DerivedImpl,
    /// Only `?` and `for`-loop `next()` tests.
    QuestionMarkOrLoopOnly,
    /// Only assertion conditions (`assert!`, `debug_assert!`,
    /// `assert_eq!`, `ensure!`, ..) written in the workspace.
    AssertOnly,
    /// Only drop flags: compiler-generated `bool`s that guard a
    /// `drop` on paths where a value may have been moved.
    DropFlagsOnly,
}

impl NoDecisionDetail {
    /// The detail as it appears in the JSON (`std_macro_only`, ..).
    pub fn as_str(self) -> &'static str {
        match self {
            NoDecisionDetail::StdMacroOnly => "std_macro_only",
            NoDecisionDetail::PatternMatchOnly => "pattern_match_only",
            NoDecisionDetail::DerivedImpl => "derived_impl",
            NoDecisionDetail::QuestionMarkOrLoopOnly => "question_mark_or_loop_only",
            NoDecisionDetail::AssertOnly => "assert_only",
            NoDecisionDetail::DropFlagsOnly => "drop_flags_only",
        }
    }
}

/// Source text lookup: line `line` (1-based) of `file`, a path as the
/// MIR prints it. `None` when the file or line cannot be read.
pub type SourceLine<'a> = dyn Fn(&str, u32) -> Option<String> + 'a;

/// Is `file` (a span path as rustc printed it) code outside the
/// workspace being analysed: the standard library (`/rustc/<hash>/
/// library/..` or a toolchain's `lib/rustlib/src/rust/library/..`)
/// or a registry / git dependency under `.cargo`?
pub fn is_external_path(file: &str) -> bool {
    is_std_library_path(file) || file.contains("/.cargo/registry/") || file.contains("/.cargo/git/")
}

/// Is `file` in the standard library (`core`, `std`, `alloc`), as
/// `/rustc/<hash>/library/..` or a toolchain's
/// `lib/rustlib/src/rust/library/..`?
pub fn is_std_library_path(file: &str) -> bool {
    file.starts_with("/rustc/")
        || file.contains("/rustlib/src/rust/library/")
        || file.contains("/library/core/")
        || file.contains("/library/std/")
        || file.contains("/library/alloc/")
}

fn is_external(span: &Option<SourceSpan>) -> bool {
    span.as_ref().is_some_and(|s| is_external_path(&s.file))
}

/// Does any `switchInt` of `f` test a value the workspace's code
/// computes: the `switchInt` itself, or a write of its discriminant,
/// carries a span in the workspace? A user `matches!(x, A | B)` does
/// (the discriminant read is at `x`), while an `assert_eq!` body
/// does not (the comparison and the test are both in `core`).
pub fn has_user_switch(f: &MirFunction) -> bool {
    user_switch_span(f).is_some()
}

/// The earliest workspace span among `f`'s `switchInt`s and the
/// writes of their discriminants (see [`has_user_switch`]): where
/// the user wrote the decision.
pub fn user_switch_span(f: &MirFunction) -> Option<SourceSpan> {
    let mut spans = Vec::new();
    for block in &f.blocks {
        if let MirTerminator::SwitchInt { discr, span, .. } = &block.terminator {
            spans.extend(span.clone());
            spans.extend(writes_of(f, *discr).into_iter().filter_map(|w| w.span));
        }
    }
    spans
        .into_iter()
        .filter(|s| !is_external_path(&s.file))
        .min_by_key(|s| (s.start_line, s.start_col))
}

/// Classify a function that has a `switchInt` but was not decomposed
/// (or whose decision lies wholly in another crate's macro). `Some`
/// only when every `switchInt` is accounted for; see the module docs.
pub fn classify_no_decision(f: &MirFunction, source: &SourceLine<'_>) -> Option<NoDecisionDetail> {
    classify_explained(f, source).ok()
}

/// [`classify_no_decision`], with what it could not account for when
/// it gives no label (for diagnosing the classifier).
#[doc(hidden)]
pub fn classify_explained(
    f: &MirFunction,
    source: &SourceLine<'_>,
) -> Result<NoDecisionDetail, String> {
    if is_derived_impl(&f.name, source) {
        return Ok(NoDecisionDetail::DerivedImpl);
    }
    // A switch the parser could not read is unaccounted for.
    if let Some(text) = f.blocks.iter().find_map(|b| match &b.terminator {
        MirTerminator::Other { text, .. } if text.trim_start().starts_with("switchInt") => {
            Some(text)
        }
        _ => None,
    }) {
        return Err(format!("unparsed switch `{text}`"));
    }
    let mut assert_marker = false;
    for marker in &f.branch_markers {
        if is_external_path(&marker.file) {
            continue;
        }
        if !in_assert_call(marker, source) {
            return Err(format!("user branch marker at {marker:?}"));
        }
        assert_marker = true;
    }
    let mut kinds = Vec::new();
    for block in &f.blocks {
        if let MirTerminator::SwitchInt { discr, span, .. } = &block.terminator {
            match classify_switch(f, *discr, span, source) {
                Some(kind) => kinds.push(kind),
                None => return Err(format!("bb{}: switch on _{discr} at {span:?}", block.id)),
            }
        }
    }
    if kinds.is_empty() {
        return Err("no switchInt".to_string());
    }
    if assert_marker {
        kinds.push(SwitchKind::Assert);
    }
    let has = |k: SwitchKind| kinds.contains(&k);
    Ok(if kinds.iter().all(|k| *k == SwitchKind::DropFlag) {
        NoDecisionDetail::DropFlagsOnly
    } else if has(SwitchKind::Pattern) {
        NoDecisionDetail::PatternMatchOnly
    } else if has(SwitchKind::Assert) {
        NoDecisionDetail::AssertOnly
    } else if has(SwitchKind::TryOrLoop) {
        NoDecisionDetail::QuestionMarkOrLoopOnly
    } else {
        NoDecisionDetail::StdMacroOnly
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchKind {
    DropFlag,
    StdMacro,
    TryOrLoop,
    Pattern,
    Assert,
}

fn classify_switch(
    f: &MirFunction,
    discr: LocalId,
    span: &Option<SourceSpan>,
    source: &SourceLine<'_>,
) -> Option<SwitchKind> {
    let writes = writes_of(f, discr);
    if is_drop_flag(f, discr, &writes) {
        return Some(SwitchKind::DropFlag);
    }
    // An enum discriminant: `?`, a `for` loop, or a pattern match.
    if let [Write {
        kind: WriteKind::Discriminant(scrutinee),
        ..
    }] = writes.as_slice()
    {
        let from_call = |needle: &[&str]| {
            f.blocks.iter().any(|b| {
                matches!(&b.terminator, MirTerminator::Call { dst, func_text, .. }
                    if Some(*dst) == *scrutinee && needle.iter().any(|n| func_text.contains(n)))
            })
        };
        if from_call(&["as Try>::branch", "as Iterator>::next"]) {
            return Some(SwitchKind::TryOrLoop);
        }
        // `matches!` / `match` on an enum: its arms carry no branch
        // marker, so it is a pattern match, not a Boolean condition.
        return Some(SwitchKind::Pattern);
    }
    if let Some(s) = span {
        if !is_external_path(&s.file) && in_assert_call(s, source) {
            return Some(SwitchKind::Assert);
        }
    }
    // A test inside another crate's macro on a value that macro
    // computed: every write of the discriminant is in that code.
    if is_external(span) && !writes.is_empty() && writes.iter().all(|w| is_external(&w.span)) {
        return Some(SwitchKind::StdMacro);
    }
    None
}

/// A statement or call that writes a whole local.
struct Write {
    kind: WriteKind,
    span: Option<SourceSpan>,
}

#[derive(PartialEq)]
enum WriteKind {
    ConstBool,
    /// `discriminant(_N)` (`Some(N)`), or of a place such as
    /// `(*_N)` or `(_N.0: T)` (`None`).
    Discriminant(Option<LocalId>),
    Other,
}

fn writes_of(f: &MirFunction, local: LocalId) -> Vec<Write> {
    let mut out = Vec::new();
    for block in &f.blocks {
        for stmt in &block.statements {
            let (dst, kind, span) = match stmt {
                MirStatement::AssignConstBool { dst, span, .. } => {
                    (*dst, WriteKind::ConstBool, span)
                }
                MirStatement::AssignDiscriminant { dst, src, span } => {
                    (*dst, WriteKind::Discriminant(Some(*src)), span)
                }
                MirStatement::AssignCopy { dst, span, .. }
                | MirStatement::AssignDowncast { dst, span, .. }
                | MirStatement::AssignCaptureRead { dst, span, .. }
                | MirStatement::AssignBinaryCompare { dst, span, .. }
                | MirStatement::AssignUnaryNot { dst, span, .. } => (*dst, WriteKind::Other, span),
                MirStatement::Other { text, span } => match other_dst(text) {
                    Some(dst) => {
                        let rhs = text.split_once('=').map_or("", |(_, r)| r.trim_start());
                        let kind = if rhs.starts_with("discriminant(") {
                            WriteKind::Discriminant(None)
                        } else {
                            WriteKind::Other
                        };
                        (dst, kind, span)
                    }
                    None => continue,
                },
            };
            if dst == local {
                out.push(Write {
                    kind,
                    span: span.clone(),
                });
            }
        }
        match &block.terminator {
            MirTerminator::Call { dst, span, .. }
            | MirTerminator::NoReturnCall { dst, span, .. }
                if *dst == local =>
            {
                out.push(Write {
                    kind: WriteKind::Other,
                    span: span.clone(),
                });
            }
            _ => {}
        }
    }
    out
}

/// The local an unparsed statement assigns as a whole (`_7 = ..`),
/// or a local whose projection it assigns (`(_7.0: T) = ..`), which
/// also counts as a write.
fn other_dst(text: &str) -> Option<LocalId> {
    let t = text.trim_start().trim_start_matches('(');
    let rest = t.strip_prefix('_')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let after = rest[digits.len()..].trim_start();
    if after.starts_with('=') && !after.starts_with("==") || after.starts_with('.') {
        digits.parse().ok()
    } else {
        None
    }
}

/// A drop flag: a `bool` temporary with no source name, never a
/// parameter, written only by `const true` / `const false`.
fn is_drop_flag(f: &MirFunction, discr: LocalId, writes: &[Write]) -> bool {
    !writes.is_empty()
        && writes.iter().all(|w| w.kind == WriteKind::ConstBool)
        && !f.debug_names.contains_key(&discr)
        && !f.args.iter().any(|a| a.local == discr)
        && crate::decision::local_type(f, discr) == Some("bool")
}

/// Is `span`'s start inside the parentheses of an `assert!`-family
/// or `ensure!` call in the source? Looks back at most 8 lines.
fn in_assert_call(span: &SourceSpan, source: &SourceLine<'_>) -> bool {
    const MACROS: &[&str] = &[
        "assert!",
        "assert_eq!",
        "assert_ne!",
        "debug_assert!",
        "debug_assert_eq!",
        "debug_assert_ne!",
        "ensure!",
    ];
    let first = span.start_line.saturating_sub(8).max(1);
    let mut text = String::new();
    for line in first..=span.start_line {
        let Some(mut l) = source(&span.file, line) else {
            return false;
        };
        if line == span.start_line {
            let cut = l
                .char_indices()
                .nth(span.start_col.saturating_sub(1) as usize)
                .map_or(l.len(), |(i, _)| i);
            // A marker spanning the whole call (`assert!(x.is_empty())`
            // when the condition is not a `bool` operator chain)
            // starts at the macro name.
            let at = &l[cut..];
            if MACROS.iter().any(|m| {
                at.strip_prefix(m)
                    .is_some_and(|rest| rest.trim_start().starts_with('('))
            }) {
                return true;
            }
            l.truncate(cut);
        }
        text.push_str(&l);
        text.push('\n');
    }
    let Some((pos, name)) = MACROS
        .iter()
        .filter_map(|m| last_macro_call(&text, m).map(|p| (p, *m)))
        .max_by_key(|(p, _)| *p)
    else {
        return false;
    };
    // Still inside the call's parentheses at the span's start.
    let after = text[pos + name.len()..].trim_start();
    let Some(args) = after.strip_prefix('(') else {
        return false;
    };
    paren_depth(args) > 0
}

/// Byte offset of the last `name` in `text` that is a whole macro
/// name (not the tail of a longer identifier).
fn last_macro_call(text: &str, name: &str) -> Option<usize> {
    let mut end = text.len();
    while let Some(p) = text[..end].rfind(name) {
        let prev = text[..p].chars().next_back();
        if !prev.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return Some(p);
        }
        end = p;
    }
    None
}

/// Nesting depth after `s`, starting inside one `(`; parentheses
/// inside string literals are skipped.
fn paren_depth(s: &str) -> i32 {
    let mut depth = 1;
    let mut in_str = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return 0;
                }
            }
            _ => {}
        }
    }
    depth
}

/// Is `name`'s innermost `<impl at FILE:L:C: ..>` a span inside a
/// `#[derive(..)]` attribute in the source?
fn is_derived_impl(name: &str, source: &SourceLine<'_>) -> bool {
    let Some(start) = name.rfind("<impl at ") else {
        return false;
    };
    let rest = &name[start + "<impl at ".len()..];
    let Some(end) = rest.find('>') else {
        return false;
    };
    let Some(span) = crate::mir::parse_span_literal(&rest[..end]) else {
        return false;
    };
    let first = span.start_line.saturating_sub(8).max(1);
    let mut text = String::new();
    for line in first..=span.start_line {
        let Some(mut l) = source(&span.file, line) else {
            return false;
        };
        if line == span.start_line {
            let cut = l
                .char_indices()
                .nth(span.start_col.saturating_sub(1) as usize)
                .map_or(l.len(), |(i, _)| i);
            l.truncate(cut);
        }
        text.push_str(&l);
        text.push('\n');
    }
    let Some(pos) = last_macro_call(&text, "derive") else {
        return false;
    };
    let after = text[pos + "derive".len()..].trim_start();
    // `#[derive(` or `#[cfg_attr(.., derive(`: still inside the list.
    let attr = text[..pos].trim_end();
    let Some(list) = after.strip_prefix('(') else {
        return false;
    };
    (attr.ends_with("#[") || attr.ends_with(',') || attr.ends_with('('))
        && paren_depth(list) > 0
        && !list.contains(']')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mir::parse_text;

    fn no_source(_: &str, _: u32) -> Option<String> {
        None
    }

    fn source_of(text: &'static str) -> impl Fn(&str, u32) -> Option<String> {
        move |_, line| text.lines().nth(line as usize - 1).map(str::to_string)
    }

    #[test]
    fn external_paths_are_the_standard_library_and_registry_crates() {
        assert!(is_external_path(
            "/rustc/abc/library/core/src/macros/mod.rs"
        ));
        assert!(is_external_path(
            "/Users/x/.rustup/toolchains/nightly/lib/rustlib/src/rust/library/core/src/macros/mod.rs"
        ));
        assert!(is_external_path(
            "/Users/x/.cargo/registry/src/index.crates.io-1/anyhow-1.0.0/src/ensure.rs"
        ));
        assert!(!is_external_path("src/lib.rs"));
        assert!(!is_external_path("url/src/parser.rs"));
    }

    /// `assert_eq!(a, b)` in a test: the comparison, its branch marker
    /// and the `switchInt` are all in `core`.
    const ASSERT_EQ_TEST: &str = r#"
fn t(_1: u32) -> () {
    let mut _0: ();
    let mut _2: bool;
    let mut _3: !;

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => /rustc/h/library/core/src/macros/mod.rs:47:20: 47:46 (#24)

    coverage Code { bcb: bcb0 } => tests/t.rs:1:1: 1:10 (#0);

    bb0: {
        _2 = Eq(copy _1, const 3_u32);   // scope 2 at /rustc/h/library/core/src/macros/mod.rs:47:21: 47:46
        switchInt(move _2) -> [0: bb2, otherwise: bb1]; // scope 2 at /rustc/h/library/core/src/macros/mod.rs:47:21: 47:46
    }

    bb1: {
        return;                          // scope 0 at tests/t.rs:3:2: 3:2
    }

    bb2: {
        _3 = core::panicking::assert_failed::<u32, u32>(const core::panicking::AssertKind::Eq) -> unwind continue; // scope 3 at /rustc/h/library/core/src/macros/mod.rs:52:21: 52:114
    }
}
"#;

    #[test]
    fn assert_eq_alone_is_std_macro_only() {
        let mir = parse_text(ASSERT_EQ_TEST).expect("parses");
        let f = &mir.functions[0];
        assert_eq!(f.branch_markers.len(), 1);
        assert!(!has_user_switch(f));
        assert_eq!(
            classify_no_decision(f, &no_source),
            Some(NoDecisionDetail::StdMacroOnly)
        );
    }

    /// A user `if a && b` that the decomposer declined: its markers
    /// are in the workspace, so it is never `no_decision`.
    const USER_IF: &str = r#"
fn g(_1: bool, _2: bool) -> u32 {
    debug a => _1;
    debug b => _2;
    let mut _0: u32;

    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/lib.rs:2:8: 2:9 (#0)
    coverage branch { true: BlockMarkerId(2), false: BlockMarkerId(3) } => src/lib.rs:2:13: 2:14 (#0)

    bb0: {
        switchInt(copy _1) -> [0: bb3, otherwise: bb1]; // scope 0 at src/lib.rs:2:8: 2:9
    }

    bb1: {
        switchInt(copy _2) -> [0: bb3, otherwise: bb2]; // scope 0 at src/lib.rs:2:13: 2:14
    }

    bb2: {
        _0 = foo() -> [return: bb4, unwind continue]; // scope 0 at src/lib.rs:2:17: 2:22
    }

    bb3: {
        _0 = const 0_u32;                // scope 0 at src/lib.rs:2:34: 2:35
        goto -> bb4;                     // scope 0 at src/lib.rs:2:34: 2:35
    }

    bb4: {
        return;                          // scope 0 at src/lib.rs:3:2: 3:2
    }
}
"#;

    #[test]
    fn a_user_condition_is_never_no_decision() {
        let mir = parse_text(USER_IF).expect("parses");
        let f = &mir.functions[0];
        assert!(has_user_switch(f));
        assert_eq!(classify_no_decision(f, &no_source), None);
        // Even inside an `assert!`-like line, a switch on a
        // parameter outside one is real.
        let src =
            source_of("fn g(a: bool, b: bool) -> u32 {\n    if a && b { foo() } else { 0 }\n}");
        assert_eq!(classify_no_decision(f, &src), None);
    }

    /// The same switches inside `assert!(a && b)`.
    #[test]
    fn a_condition_inside_assert_is_assert_only() {
        let text = USER_IF
            .replace("src/lib.rs:2:13: 2:14", "src/lib.rs:2:18: 2:19")
            .replace("src/lib.rs:2:8: 2:9", "src/lib.rs:2:13: 2:14");
        let mir = parse_text(&text).expect("parses");
        let f = &mir.functions[0];
        let src = source_of("fn g(a: bool, b: bool) -> u32 {\n    assert!(a && b, \"(\"); 0\n}");
        assert_eq!(
            classify_no_decision(f, &src),
            Some(NoDecisionDetail::AssertOnly)
        );
        // Without the source there is no evidence.
        assert_eq!(classify_no_decision(f, &no_source), None);
    }

    /// `drop(_3)` guarded by a drop flag, nothing else.
    const DROP_FLAG: &str = r#"
fn h(_1: String, _2: bool) -> () {
    debug s => _1;
    let mut _0: ();
    let mut _3: bool;

    bb0: {
        _3 = const false;                // scope 0 at src/lib.rs:1:1: 1:2
        _3 = const true;                 // scope 0 at src/lib.rs:1:1: 1:2
        switchInt(copy _3) -> [0: bb2, otherwise: bb1]; // scope 0 at src/lib.rs:3:1: 3:2
    }

    bb1: {
        drop(_1) -> [return: bb2, unwind continue]; // scope 0 at src/lib.rs:3:1: 3:2
    }

    bb2: {
        return;                          // scope 0 at src/lib.rs:3:2: 3:2
    }
}
"#;

    #[test]
    fn drop_flags_alone_are_drop_flags_only() {
        let mir = parse_text(DROP_FLAG).expect("parses");
        let f = &mir.functions[0];
        assert_eq!(
            classify_no_decision(f, &no_source),
            Some(NoDecisionDetail::DropFlagsOnly)
        );
    }

    /// `let x = true; if x { .. }` with a branch marker is a user
    /// decision even though `x` is only ever assigned constants.
    #[test]
    fn a_marked_constant_condition_is_not_a_drop_flag() {
        let text = DROP_FLAG.replace(
            "    bb0: {",
            "    coverage branch { true: BlockMarkerId(0), false: BlockMarkerId(1) } => src/lib.rs:2:8: 2:9 (#0)\n\n    bb0: {",
        );
        let mir = parse_text(&text).expect("parses");
        assert_eq!(classify_no_decision(&mir.functions[0], &no_source), None);
    }

    /// `let v = it.next()?`-style: a discriminant of a `Try::branch`
    /// result, and an enum `match`.
    const TRY_AND_MATCH: &str = r#"
fn k(_1: Option<u32>) -> Option<u32> {
    debug o => _1;
    let mut _0: Option<u32>;
    let mut _2: ControlFlow<Option<Infallible>, u32>;
    let mut _3: isize;
    let mut _4: isize;

    bb0: {
        _2 = <Option<u32> as Try>::branch(copy _1) -> [return: bb1, unwind continue]; // scope 0 at src/lib.rs:2:5: 2:7
    }

    bb1: {
        _3 = discriminant(_2);           // scope 0 at src/lib.rs:2:5: 2:7
        switchInt(move _3) -> [0: bb2, 1: bb3, otherwise: bb4]; // scope 0 at src/lib.rs:2:5: 2:7
    }

    bb2: {
        _4 = discriminant(_1);           // scope 0 at src/lib.rs:3:11: 3:12
        switchInt(move _4) -> [0: bb3, otherwise: bb3]; // scope 0 at src/lib.rs:3:5: 3:12
    }

    bb3: {
        return;                          // scope 0 at src/lib.rs:5:2: 5:2
    }

    bb4: {
        unreachable;                     // scope 0 at src/lib.rs:2:5: 2:7
    }
}
"#;

    #[test]
    fn try_and_enum_match_are_pattern_match_only_and_try_alone_is_question_mark() {
        let mir = parse_text(TRY_AND_MATCH).expect("parses");
        assert_eq!(
            classify_no_decision(&mir.functions[0], &no_source),
            Some(NoDecisionDetail::PatternMatchOnly)
        );
        let only_try = TRY_AND_MATCH.replace(
            "        _4 = discriminant(_1);           // scope 0 at src/lib.rs:3:11: 3:12\n        switchInt(move _4) -> [0: bb3, otherwise: bb3]; // scope 0 at src/lib.rs:3:5: 3:12",
            "        goto -> bb3;                     // scope 0 at src/lib.rs:3:5: 3:12",
        );
        let mir = parse_text(&only_try).expect("parses");
        assert_eq!(
            classify_no_decision(&mir.functions[0], &no_source),
            Some(NoDecisionDetail::QuestionMarkOrLoopOnly)
        );
    }

    #[test]
    fn an_integer_match_on_a_parameter_is_real() {
        let text = r#"
fn m(_1: u8) -> u8 {
    debug n => _1;
    let mut _0: u8;

    bb0: {
        switchInt(copy _1) -> [0: bb1, otherwise: bb2]; // scope 0 at src/lib.rs:2:5: 2:12
    }

    bb1: {
        _0 = const 1_u8;                 // scope 0 at src/lib.rs:3:14: 3:15
        return;                          // scope 0 at src/lib.rs:5:2: 5:2
    }

    bb2: {
        _0 = const 2_u8;                 // scope 0 at src/lib.rs:4:14: 4:15
        return;                          // scope 0 at src/lib.rs:5:2: 5:2
    }
}
"#;
        let mir = parse_text(text).expect("parses");
        assert_eq!(classify_no_decision(&mir.functions[0], &no_source), None);
    }

    #[test]
    fn derived_impls_are_found_from_the_attribute() {
        let src = source_of("/// doc\n#[derive(Clone, PartialEq,\n    Debug)]\npub struct S;\n");
        assert!(is_derived_impl(
            "m::<impl at src/lib.rs:2:17: 2:26>::eq",
            &src
        ));
        assert!(is_derived_impl(
            "m::<impl at src/lib.rs:3:5: 3:10>::fmt",
            &src
        ));
        // A hand-written impl on line 4.
        assert!(!is_derived_impl(
            "m::<impl at src/lib.rs:4:1: 4:14>::eq",
            &src
        ));
        assert!(!is_derived_impl("free_fn", &src));
        assert!(!is_derived_impl(
            "m::<impl at src/lib.rs:2:17: 2:26>::eq",
            &no_source
        ));
    }

    #[test]
    fn assert_context_needs_the_open_call() {
        let span = |line, col| SourceSpan {
            file: "src/lib.rs".into(),
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col + 1,
        };
        let src = source_of(
            "    assert!(x > 0);\n    if y { }\n    debug_assert!(\n        a ||\n        b\n    );\n",
        );
        assert!(in_assert_call(&span(1, 13), &src));
        // A span starting at the macro name itself.
        assert!(in_assert_call(&span(1, 5), &src));
        assert!(!in_assert_call(&span(2, 8), &src));
        assert!(in_assert_call(&span(5, 9), &src));
        // `my_assert!(..)` is not `assert!`.
        let src = source_of("    my_assert!(x);\n");
        assert!(!in_assert_call(&span(1, 16), &src));
    }
}
