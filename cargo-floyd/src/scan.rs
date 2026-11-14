//! Pass 1 of a cargo run, per MIR: sort every function with a
//! `switchInt` into a decomposable candidate or an `out_of_scope`
//! entry with its reason.
//!
//! Floyd never drops such a function silently. Every one of them is
//! exactly one of: a candidate, an `out_of_scope` entry, a CTFE body
//! (counted in `ctfe_bodies_skipped`, its runtime twin is listed), or
//! a candidate left out by `--only`. [`FunctionScan`] carries all
//! four, and the tests check that they add up.

use floyd::correlate;
use floyd::decision::{self, DeclineReason};
use floyd::scope::{self, NoDecisionDetail, SourceLine};
use floyd::{DecisionProvenance, DecisionTree, Mir};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// A function with a `switchInt` that Floyd does not analyse.
#[derive(serde::Serialize, Clone, Debug)]
pub struct DeclinedFunction {
    pub name: String,
    pub source_file: Option<String>,
    pub line: Option<u32>,
    /// Why it is not analysed: `no_decision` when none of its
    /// `switchInt`s is a Boolean decision the user wrote (see
    /// `detail`), `unsupported_shape` when it has a decision in a
    /// shape the engine does not recognise yet, `non_bool_leaf` when
    /// an arm's value is a named local that is not `bool`,
    /// `no_conditions` when the tree tests no condition,
    /// `too_many_conditions` when the decision is too wide to analyse
    /// (see `conditions`).
    pub reason: DeclineReason,
    /// For `no_decision`: `std_macro_only`, `pattern_match_only`,
    /// `derived_impl`, `question_mark_or_loop_only`, `assert_only` or
    /// `drop_flags_only`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<NoDecisionDetail>,
    /// For `too_many_conditions`: the decision's condition count
    /// (or, when its tree grew too large to finish, the number of
    /// Boolean tests reachable from the entry).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<usize>,
}

/// One decomposable function of a MIR.
pub struct ScannedCandidate {
    pub fn_idx: usize,
    pub tree: DecisionTree,
    pub prov: DecisionProvenance,
}

/// What [`scan_functions`] found in one MIR.
#[derive(Default)]
pub struct FunctionScan {
    pub candidates: Vec<ScannedCandidate>,
    pub declined: Vec<DeclinedFunction>,
    /// `// MIR FOR CTFE` bodies with a `switchInt`, not listed: the
    /// runtime body of the same `const fn` is listed instead.
    pub ctfe_bodies_skipped: usize,
    /// Candidates left out because no `--only` pattern names them.
    pub only_filtered: usize,
}

/// Sort every function of `mir` that has a `switchInt`.
///
/// `source` reads workspace source lines for the `no_decision`
/// classifier (derive attributes, `assert!` calls); a function it
/// cannot find evidence for keeps its decomposer reason.
pub fn scan_functions(
    mir: &Mir,
    only_patterns: &[String],
    source: &SourceLine<'_>,
) -> FunctionScan {
    let mut scan = FunctionScan::default();
    for (fn_idx, f) in mir.functions.iter().enumerate() {
        // Without a `switchInt` there is no branch, so nothing to
        // measure: boolean passthroughs like `fn f(x: bool) -> bool
        // { x }` decompose to a bare `Condition`, but llvm-cov has no
        // branch region for them. They are neither candidates nor
        // out of scope.
        if !decision::has_switchint(f) {
            continue;
        }
        if f.is_ctfe {
            scan.ctfe_bodies_skipped += 1;
            continue;
        }
        let declined = |reason, detail, conditions| {
            let prov = locate_in_workspace(f, correlate::provenance_for(f));
            DeclinedFunction {
                name: prov.function_name.unwrap_or_default(),
                source_file: prov.source_file,
                line: prov.line,
                reason,
                detail,
                conditions,
            }
        };
        let (node, lift) = match decision::try_decompose_candidate_detailed(f) {
            Ok(decomposed) => decomposed,
            Err(d) => {
                let detail = match d.reason {
                    DeclineReason::UnsupportedShape | DeclineReason::NonBoolLeaf => {
                        scope::classify_no_decision(f, source)
                    }
                    _ => None,
                };
                let reason = if detail.is_some() {
                    DeclineReason::NoDecision
                } else {
                    d.reason
                };
                scan.declined.push(declined(reason, detail, d.conditions));
                continue;
            }
        };
        let mut prov = DecisionProvenance {
            lift,
            ..correlate::provenance_for(f)
        };
        // `--only=<pattern>` scopes the run to the functions named.
        if !only_patterns.is_empty() {
            let fn_name = prov.function_name.as_deref().unwrap_or(&f.name);
            if !only_patterns
                .iter()
                .any(|pat| fn_name.contains(pat.as_str()))
            {
                scan.only_filtered += 1;
                continue;
            }
        }
        if prov
            .source_file
            .as_deref()
            .is_some_and(scope::is_std_library_path)
        {
            if !scope::has_user_switch(f) {
                // The decision is wholly another crate's macro code:
                // `assert_eq!`'s `left_val == right_val` in a test.
                // Its "conditions" are the macro's own locals.
                if let Some(detail) = scope::classify_no_decision(f, source) {
                    scan.declined
                        .push(declined(DeclineReason::NoDecision, Some(detail), None));
                    continue;
                }
                // Not accounted for (`matches!(ch, '\t' | '\n')`
                // switches on the parameter itself): keep it as a
                // candidate; the coverage join classifies a decision
                // whose spans all lie outside the workspace as not
                // measurable.
            }
            prov = locate_in_workspace(f, prov);
        }
        scan.candidates.push(ScannedCandidate {
            fn_idx,
            tree: DecisionTree::from_function(f, node),
            prov,
        });
    }
    scan
}

/// `prov`, moved into the workspace when its earliest condition span
/// is in the standard library: a user macro call expanding to `core`
/// code (`matches!(self.host, HostInternal::None)`, `assert_eq!`)
/// puts that span in `core`'s macro definition, but the user wrote
/// the call in `f`. The new location is the first workspace span of
/// `f`'s tests, else of its body; `prov` is unchanged when there is
/// none.
fn locate_in_workspace(
    f: &floyd::mir::MirFunction,
    mut prov: DecisionProvenance,
) -> DecisionProvenance {
    if !prov
        .source_file
        .as_deref()
        .is_some_and(scope::is_std_library_path)
    {
        return prov;
    }
    let user = scope::user_switch_span(f).or_else(|| {
        f.coverage_spans
            .iter()
            .filter(|s| !scope::is_external_path(&s.file))
            .min_by_key(|s| (s.start_line, s.start_col))
            .cloned()
    });
    if let Some(span) = user {
        prov.source_file = Some(span.file);
        prov.line = Some(span.start_line);
    }
    prov
}

/// Reads workspace source lines for [`scan_functions`], caching each
/// file. Relative span paths resolve against the workspace root;
/// paths outside the workspace (the standard library, registry
/// crates) are never read.
pub struct SourceReader {
    root: PathBuf,
    files: RefCell<HashMap<String, Option<Vec<String>>>>,
}

impl SourceReader {
    pub fn new(root: &Path) -> Self {
        SourceReader {
            root: root.to_path_buf(),
            files: RefCell::new(HashMap::new()),
        }
    }

    /// Line `line` (1-based) of `file`.
    pub fn line(&self, file: &str, line: u32) -> Option<String> {
        if scope::is_external_path(file) || line == 0 {
            return None;
        }
        let mut files = self.files.borrow_mut();
        let lines = files.entry(file.to_string()).or_insert_with(|| {
            let text = std::fs::read_to_string(self.root.join(file)).ok()?;
            Some(text.lines().map(str::to_string).collect())
        });
        lines.as_ref()?.get(line as usize - 1).cloned()
    }
}

/// Counts of the `out_of_scope` list, by reason and, for
/// `no_decision`, by detail. Serialised as `out_of_scope_summary`.
#[derive(serde::Serialize, Debug, Default, PartialEq)]
pub struct OutOfScopeSummary {
    /// Entries in `out_of_scope`.
    pub total: usize,
    /// Entries per `reason`.
    pub by_reason: BTreeMap<&'static str, usize>,
    /// `no_decision` entries per `detail`.
    pub no_decision: BTreeMap<&'static str, usize>,
    /// `const fn` CTFE bodies not listed (their runtime bodies are).
    pub ctfe_bodies_skipped: usize,
}

impl OutOfScopeSummary {
    pub fn new(declined: &[DeclinedFunction], ctfe_bodies_skipped: usize) -> Self {
        let mut s = OutOfScopeSummary {
            total: declined.len(),
            ctfe_bodies_skipped,
            ..Default::default()
        };
        for d in declined {
            *s.by_reason.entry(reason_str(d.reason)).or_default() += 1;
            if let Some(detail) = d.detail {
                *s.no_decision.entry(detail.as_str()).or_default() += 1;
            }
        }
        s
    }

    /// Entries with a decision Floyd does not analyse (every reason
    /// but `no_decision`).
    pub fn declined_decisions(&self) -> usize {
        self.total - self.count("no_decision")
    }

    fn count(&self, reason: &str) -> usize {
        self.by_reason.get(reason).copied().unwrap_or(0)
    }

    /// The text report's "Out of scope" block, empty when nothing is
    /// out of scope.
    pub fn text(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.total == 0 {
            return out;
        }
        out.push(format!(
            "Out of scope: {} function(s) with a `switchInt` were not analysed. See \
             --format=json's `out_of_scope` for the list.",
            self.total
        ));
        let no_decision = self.count("no_decision");
        if no_decision > 0 {
            let details: Vec<String> = self
                .no_decision
                .iter()
                .map(|(d, n)| format!("{} {n}", d.replace('_', " ")))
                .collect();
            out.push(format!(
                "  {no_decision} with no Boolean decision the user wrote ({}).",
                details.join(", ")
            ));
        }
        for (reason, text) in [
            (
                "unsupported_shape",
                "with a decision in a shape the engine does not recognise yet",
            ),
            (
                "non_bool_leaf",
                "with a decision whose arm returns a non-`bool` named value",
            ),
            ("no_conditions", "whose decision tests no condition"),
            (
                "too_many_conditions",
                "with a decision too wide to analyse (more than 12 conditions)",
            ),
        ] {
            let n = self.count(reason);
            if n > 0 {
                out.push(format!("  {n} {text} ({reason})."));
            }
        }
        if self.declined_decisions() > 0 {
            out.push(
                "  The engine declined these decisions rather than report a wrong verdict."
                    .to_string(),
            );
        }
        if self.ctfe_bodies_skipped > 0 {
            out.push(format!(
                "  ({} `const fn` compile-time (CTFE) bodies are not listed separately; \
                 their runtime bodies are.)",
                self.ctfe_bodies_skipped
            ));
        }
        out
    }
}

/// The JSON name of `reason`.
pub fn reason_str(reason: DeclineReason) -> &'static str {
    match reason {
        DeclineReason::UnsupportedShape => "unsupported_shape",
        DeclineReason::NonBoolLeaf => "non_bool_leaf",
        DeclineReason::NoConditions => "no_conditions",
        DeclineReason::CtfeBody => "ctfe_body",
        DeclineReason::NoDecision => "no_decision",
        DeclineReason::TooManyConditions => "too_many_conditions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floyd::mir::parse_text;

    fn no_source(_: &str, _: u32) -> Option<String> {
        None
    }

    /// Every function with a `switchInt` is exactly one of: a
    /// candidate, an `out_of_scope` entry, a skipped CTFE body, or
    /// left out by `--only`.
    fn assert_accounted(mir: &Mir, scan: &FunctionScan) {
        let with_switch = mir
            .functions
            .iter()
            .filter(|f| decision::has_switchint(f))
            .count();
        assert_eq!(
            scan.candidates.len()
                + scan.declined.len()
                + scan.ctfe_bodies_skipped
                + scan.only_filtered,
            with_switch,
            "every function with a switchInt is listed exactly once"
        );
        let mut seen = std::collections::BTreeSet::new();
        for c in &scan.candidates {
            assert!(seen.insert(c.fn_idx), "candidate listed twice");
        }
    }

    /// A user `matches!` (url `Url::has_host`): the `switchInt` and
    /// the `matches!` arms are in `core`, the discriminant read is in
    /// the workspace. An `assert_eq!` test: everything is in `core`.
    /// A `const fn` with its CTFE twin.
    const MIXED: &str = r#"
fn <impl at url/src/lib.rs:317:1: 317:9>::has_host(_1: &Url) -> bool {
    debug self => _1;
    let mut _0: bool;
    let mut _2: bool;
    let mut _3: isize;

    coverage Code { bcb: bcb0 } => url/src/lib.rs:1121:5: 1121:35 (#0);

    bb0: {
        _3 = discriminant(((*_1).5: host::HostInternal)); // scope 1 at url/src/lib.rs:1122:19: 1122:28
        switchInt(move _3) -> [0: bb2, otherwise: bb1]; // scope 1 at /rustc/h/library/core/src/macros/mod.rs:434:9: 434:9
    }

    bb1: {
        _2 = const false;                // scope 1 at /rustc/h/library/core/src/macros/mod.rs:436:18: 436:23
        goto -> bb3;                     // scope 1 at /rustc/h/library/core/src/macros/mod.rs:436:18: 436:23
    }

    bb2: {
        _2 = const true;                 // scope 1 at /rustc/h/library/core/src/macros/mod.rs:435:39: 435:43
        goto -> bb3;                     // scope 1 at /rustc/h/library/core/src/macros/mod.rs:435:39: 435:43
    }

    bb3: {
        _0 = Not(move _2);               // scope 0 at url/src/lib.rs:1122:9: 1122:49
        return;                          // scope 0 at url/src/lib.rs:1123:6: 1123:6
    }
}

fn check_three(_1: u32) -> () {
    debug x => _1;
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
        _0 = const ();                   // scope 0 at tests/t.rs:3:2: 3:2
        return;                          // scope 0 at tests/t.rs:3:2: 3:2
    }

    bb2: {
        _3 = core::panicking::assert_failed::<u32, u32>(const core::panicking::AssertKind::Eq) -> unwind continue; // scope 3 at /rustc/h/library/core/src/macros/mod.rs:52:21: 52:114
    }
}

fn flag(_1: bool, _2: u32) -> u32 {
    debug deny => _1;
    debug bits => _2;
    let mut _0: u32;

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at src/lib.rs:2:8: 2:12
    }

    bb1: {
        _0 = BitOr(copy _2, const 1_u32); // scope 0 at src/lib.rs:2:15: 2:23
        goto -> bb3;                     // scope 0 at src/lib.rs:2:15: 2:23
    }

    bb2: {
        _0 = copy _2;                    // scope 0 at src/lib.rs:2:33: 2:37
        goto -> bb3;                     // scope 0 at src/lib.rs:2:33: 2:37
    }

    bb3: {
        return;                          // scope 0 at src/lib.rs:3:2: 3:2
    }
}

// MIR FOR CTFE
fn flag(_1: bool, _2: u32) -> u32 {
    debug deny => _1;
    debug bits => _2;
    let mut _0: u32;

    bb0: {
        switchInt(copy _1) -> [0: bb2, otherwise: bb1]; // scope 0 at src/lib.rs:2:8: 2:12
    }

    bb1: {
        _0 = BitOr(copy _2, const 1_u32); // scope 0 at src/lib.rs:2:15: 2:23
        goto -> bb3;                     // scope 0 at src/lib.rs:2:15: 2:23
    }

    bb2: {
        _0 = copy _2;                    // scope 0 at src/lib.rs:2:33: 2:37
        goto -> bb3;                     // scope 0 at src/lib.rs:2:33: 2:37
    }

    bb3: {
        return;                          // scope 0 at src/lib.rs:3:2: 3:2
    }
}

fn passthrough(_1: bool) -> bool {
    debug x => _1;
    let mut _0: bool;

    bb0: {
        _0 = copy _1;                    // scope 0 at src/lib.rs:5:5: 5:6
        return;                          // scope 0 at src/lib.rs:6:2: 6:2
    }
}
"#;

    #[test]
    fn user_matches_is_listed_assert_eq_is_no_decision_and_ctfe_is_folded() {
        let mir = parse_text(MIXED).expect("parses");
        assert_eq!(mir.functions.len(), 5);
        assert!(mir.functions[3].is_ctfe);
        let scan = scan_functions(&mir, &[], &no_source);
        assert_accounted(&mir, &scan);
        assert_eq!(scan.ctfe_bodies_skipped, 1);

        let has_host = scan
            .candidates
            .iter()
            .find(|c| {
                c.prov
                    .function_name
                    .as_deref()
                    .unwrap()
                    .ends_with("has_host")
            })
            .expect("the user matches! predicate is a candidate");
        // Located at the user's `matches!`, not in `core`.
        assert_eq!(has_host.prov.source_file.as_deref(), Some("url/src/lib.rs"));
        assert_eq!(has_host.prov.line, Some(1122));

        let check = scan
            .declined
            .iter()
            .find(|d| d.name == "check_three")
            .expect("assert_eq! test is listed");
        assert_eq!(check.reason, DeclineReason::NoDecision);
        assert_eq!(check.detail, Some(NoDecisionDetail::StdMacroOnly));

        // One `flag` entry (the runtime body, a value-arm candidate),
        // no `ctfe_body` entry.
        let flags = scan
            .candidates
            .iter()
            .filter(|c| c.prov.function_name.as_deref() == Some("flag"))
            .count();
        assert_eq!(flags, 1);
        assert!(scan.declined.iter().all(|d| d.name != "flag"));
        assert!(scan
            .declined
            .iter()
            .all(|d| d.reason != DeclineReason::CtfeBody));
        // No entry for the switch-free passthrough.
        assert!(scan.declined.iter().all(|d| d.name != "passthrough"));
        assert!(scan
            .candidates
            .iter()
            .all(|c| c.prov.function_name.as_deref() != Some("passthrough")));

        let summary = OutOfScopeSummary::new(&scan.declined, scan.ctfe_bodies_skipped);
        assert_eq!(summary.total, 1);
        assert_eq!(summary.by_reason.get("no_decision"), Some(&1));
        assert_eq!(summary.no_decision.get("std_macro_only"), Some(&1));
        assert_eq!(summary.declined_decisions(), 0);
        let text = summary.text().join("\n");
        assert!(text.contains("1 with no Boolean decision the user wrote (std macro only 1)"));
        assert!(!text.contains("declined these decisions"));
        assert!(text.contains("1 `const fn` compile-time (CTFE) bodies"));
        let mut declined = scan.declined.clone();
        declined.push(DeclinedFunction {
            name: "g".into(),
            source_file: None,
            line: None,
            reason: DeclineReason::NonBoolLeaf,
            detail: None,
            conditions: None,
        });
        let text = OutOfScopeSummary::new(&declined, 0).text().join("\n");
        assert!(text.contains("2 function(s) with a `switchInt` were not analysed"));
        assert!(text.contains("1 with a decision whose arm returns a non-`bool` named value"));
        assert!(text.contains("declined these decisions"));
        assert!(!text.contains("CTFE"));
    }

    #[test]
    fn only_filtered_candidates_are_accounted_for() {
        let mir = parse_text(MIXED).expect("parses");
        let scan = scan_functions(&mir, &["no_such_fn".to_string()], &no_source);
        assert_accounted(&mir, &scan);
        assert!(scan.candidates.is_empty());
        // `has_host` and the `assert_eq!` test both decompose.
        assert_eq!(scan.only_filtered, 2);
    }

    #[test]
    fn reason_names_match_the_json() {
        for r in [
            DeclineReason::UnsupportedShape,
            DeclineReason::NonBoolLeaf,
            DeclineReason::NoConditions,
            DeclineReason::CtfeBody,
            DeclineReason::NoDecision,
            DeclineReason::TooManyConditions,
        ] {
            assert_eq!(serde_json::to_value(r).unwrap(), reason_str(r));
        }
    }

    /// Every function with a `switchInt` in every MIR of the
    /// validation sweep is accounted for. Runs when the sweep's MIR
    /// is present (`FLOYD_SWEEP_MIR_DIR`, searched recursively).
    #[test]
    #[ignore = "needs the validation sweep's MIR dumps (FLOYD_SWEEP_MIR_DIR)"]
    fn sweep_mir_functions_are_all_accounted_for() {
        let Ok(dir) = std::env::var("FLOYD_SWEEP_MIR_DIR") else {
            eprintln!("FLOYD_SWEEP_MIR_DIR not set; nothing to check");
            return;
        };
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "mir")
                    && e.metadata().is_ok_and(|m| m.len() < 50 * 1024 * 1024)
                {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(Path::new(&dir), &mut files);
        for path in files {
            let text = std::fs::read_to_string(&path).expect("read MIR");
            let Ok(mir) = parse_text(&text) else { continue };
            let scan = scan_functions(&mir, &[], &no_source);
            assert_accounted(&mir, &scan);
        }
    }
}
