//! Source-span index over `llvm-cov export` records.
//!
//! Floyd joins a MIR candidate to its coverage record by **source
//! location**, never by name. The two sides do not share a usable
//! name: rustc prints an impl method in MIR as
//! `offset::fixed::<impl at src/offset/fixed.rs:34:1: 34:17>::east_opt`
//! (no self type), free functions with trimmed paths and generic
//! functions without arguments, while `llvm-cov` names each record
//! by its mangled symbol, one per monomorphisation
//! (`<chrono::offset::fixed::FixedOffset>::east_opt`,
//! `set_if_consistent::<u32>`, ...). Both sides do carry the same
//! source spans: a MIR condition's `// scope N at <file>:<L>:<C>:
//! <L>:<C>` span is the span of the record's branch region (and
//! usually of a code region) for that condition.
//!
//! [`CoverageIndex`] therefore keys every record by its set of
//! source spans:
//!
//! - **Key.** The record's code regions ([`crate::profile::Region::KIND_CODE`]) and
//!   branch regions, each as a [`SourceSpan`] whose file has been
//!   put in canonical form by [`PathNormaliser`]. Spans in
//!   `/rustc/<hash>/...` (standard-library code) are dropped.
//! - **Merge.** Records whose keys are equal describe the same
//!   source function: the monomorphisations of one generic function
//!   (`set_if_consistent::<u32>`, `::<i64>`, ...) and the same
//!   function compiled into several crates of one binary. They are
//!   merged into one [`CoveredFunction`] whose entry count and branch
//!   counters are the **sums** over the merged records, so a generic
//!   candidate is credited with the union of its instantiations.
//! - **Lookup.** [`CoverageIndex::resolve`] takes a candidate's
//!   condition spans and returns the one merged function containing
//!   any of them, [`FunctionMatch::Missing`] if none does, and
//!   [`FunctionMatch::Ambiguous`] if the spans land in more than one
//!   merged function or the merged function's records demangle to
//!   more than one function path (distinct functions sharing the
//!   same spans, as macro-generated code can).
//!
//! There is no name-based fallback. A display name is derived from
//! the merged records' symbols for reports only (see
//! [`display_name`]).

use crate::masking::BranchCounts;
use crate::mir::SourceSpan;
use crate::profile::{Branch, CoverageReport};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// Puts source paths from MIR and from `llvm-cov` in one canonical
/// form so spans from the two sides compare with `==`.
///
/// rustc prints MIR spans with the path it was given: relative to
/// the workspace root for workspace members (cargo runs rustc in the
/// workspace root), absolute for code outside it (registry crates,
/// path dependencies elsewhere). `llvm-cov` joins the compilation
/// directory onto that path and reports it absolute. Standard-library
/// code carries `/rustc/<commit-hash>/library/...` paths.
///
/// [`Self::normalise`] resolves a relative path against the root,
/// removes `.` and `..` components lexically, and returns the path
/// relative to the root when it lies under it (absolute otherwise).
/// `/rustc/` paths normalise to `None`: they never match anything.
#[derive(Debug, Clone, Default)]
pub struct PathNormaliser {
    root: Option<PathBuf>,
}

impl PathNormaliser {
    /// A normaliser for the given workspace root (cargo mode: the
    /// `workspace_root` from `cargo metadata`; single-file mode: the
    /// directory rustc ran in). `None` compares paths as given,
    /// after lexical clean-up only.
    pub fn new(root: Option<&Path>) -> Self {
        PathNormaliser {
            root: root.map(lexical_clean),
        }
    }

    /// Canonical form of `file`, or `None` for a `/rustc/` path.
    pub fn normalise(&self, file: &str) -> Option<String> {
        if file.starts_with("/rustc/") {
            return None;
        }
        let path = Path::new(file);
        let absolute = match (&self.root, path.is_absolute()) {
            (Some(root), false) => lexical_clean(&root.join(path)),
            _ => lexical_clean(path),
        };
        if let Some(root) = &self.root {
            if let Ok(rel) = absolute.strip_prefix(root) {
                return Some(rel.to_string_lossy().into_owned());
            }
        }
        Some(absolute.to_string_lossy().into_owned())
    }

    /// `true` if `file` lies outside the root (or is a `/rustc/`
    /// path). Always `false` without a root.
    pub fn is_outside_root(&self, file: &str) -> bool {
        if self.root.is_none() {
            return false;
        }
        match self.normalise(file) {
            Some(p) => Path::new(&p).is_absolute(),
            None => true,
        }
    }

    /// `span` with its file in canonical form, or `None` for a
    /// `/rustc/` span.
    pub fn span(&self, span: &SourceSpan) -> Option<SourceSpan> {
        Some(SourceSpan {
            file: self.normalise(&span.file)?,
            ..span.clone()
        })
    }
}

/// Remove `.` components and resolve `..` against the preceding
/// component, without touching the filesystem (the source tree of a
/// copied crate need not exist where `llvm-cov` recorded it).
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether a span in a record's key is a code region or a branch
/// region. A code region and a branch region at the same span are
/// distinct key entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SpanRole {
    /// A [`crate::profile::Region`] of kind [`crate::profile::Region::KIND_CODE`].
    Code,
    /// A [`Branch`] region.
    Branch,
}

/// The merge key of a [`CoveredFunction`]: its sorted
/// `(role, normalised span)` pairs.
pub type FunctionKey = Vec<(SpanRole, SourceSpan)>;

/// One source function's coverage: every `llvm-cov` record with the
/// same span key, merged.
///
/// Records are merged when their keys (normalised code-region and
/// branch spans) are equal. For a generic function that is one
/// record per monomorphisation, all with the same spans; the merged
/// [`Self::count`] and branch counters are the sums over them, so the
/// function is credited with the union of its instantiations.
#[derive(Debug, Clone)]
pub struct CoveredFunction {
    /// Mangled symbols of the merged records, in report order.
    pub symbols: Vec<String>,
    /// Sum of the merged records' entry counts.
    pub count: u64,
    /// Branch regions with normalised spans, sorted by span; each
    /// counter is the sum over the merged records.
    pub branches: Vec<Branch>,
    /// The merge key.
    key: FunctionKey,
}

impl CoveredFunction {
    /// The merge key: this function's sorted `(role, span)` pairs.
    /// Equal keys in two indexes (two tests' coverage) identify the
    /// same source function.
    pub fn key(&self) -> &[(SpanRole, SourceSpan)] {
        &self.key
    }

    /// The distinct [`display_name`]s of the merged symbols.
    pub fn display_names(&self) -> BTreeSet<String> {
        self.symbols.iter().map(|s| display_name(s)).collect()
    }

    /// The display name when all merged symbols agree on one.
    pub fn display_name(&self) -> Option<String> {
        let names = self.display_names();
        if names.len() == 1 {
            names.into_iter().next()
        } else {
            None
        }
    }

    /// Summed counters of the first branch region at exactly `span`
    /// (normalised), if any.
    pub fn branch_counts(&self, span: &SourceSpan) -> Option<BranchCounts> {
        self.branches
            .iter()
            .find(|b| b.span == *span)
            .map(|b| BranchCounts {
                true_count: b.true_count,
                false_count: b.false_count,
            })
    }
}

/// Outcome of looking a candidate's spans up in a [`CoverageIndex`].
#[derive(Debug, Clone)]
pub enum FunctionMatch<'i> {
    /// No merged function contains any of the spans.
    Missing,
    /// Exactly one merged function contains the spans, and all its
    /// records demangle to one function path.
    Found(&'i CoveredFunction),
    /// The spans cannot be attributed to one source function. The
    /// string says why, for reports.
    Ambiguous(String),
}

/// Per-test coverage indexed by source span. See the [module
/// documentation](self) for the key, merge rule and lookup.
#[derive(Debug, Clone, Default)]
pub struct CoverageIndex {
    functions: Vec<CoveredFunction>,
    by_span: HashMap<SourceSpan, Vec<usize>>,
}

impl CoverageIndex {
    /// Index every record of `coverage`.
    pub fn build(coverage: &CoverageReport, paths: &PathNormaliser) -> Self {
        Self::build_inner(coverage, paths, None)
    }

    /// Index only the records that contain at least one span in
    /// `wanted` (normalised spans, as [`PathNormaliser::span`]
    /// returns them). Lookups of spans in `wanted` give the same
    /// result as on the full index; Pass 3 uses this to keep one
    /// small index per test instead of every record of every test.
    pub fn build_for(
        coverage: &CoverageReport,
        paths: &PathNormaliser,
        wanted: &HashSet<SourceSpan>,
    ) -> Self {
        Self::build_inner(coverage, paths, Some(wanted))
    }

    fn build_inner(
        coverage: &CoverageReport,
        paths: &PathNormaliser,
        wanted: Option<&HashSet<SourceSpan>>,
    ) -> Self {
        let mut functions: Vec<CoveredFunction> = Vec::new();
        let mut by_key: HashMap<FunctionKey, usize> = HashMap::new();
        for record in &coverage.functions {
            let mut branches: Vec<Branch> = record
                .branches
                .iter()
                .filter_map(|b| {
                    Some(Branch {
                        span: paths.span(&b.span)?,
                        ..b.clone()
                    })
                })
                .collect();
            branches.sort_by(|a, b| a.span.cmp(&b.span));
            let mut key: FunctionKey = record
                .regions
                .iter()
                .filter_map(|r| Some((SpanRole::Code, paths.span(&r.span)?)))
                .chain(branches.iter().map(|b| (SpanRole::Branch, b.span.clone())))
                .collect();
            if key.is_empty() {
                continue;
            }
            if let Some(wanted) = wanted {
                if !key.iter().any(|(_, s)| wanted.contains(s)) {
                    continue;
                }
            }
            key.sort();
            match by_key.get(&key) {
                Some(&idx) => {
                    // Equal keys hold equal branch spans, so the
                    // sorted branch lists line up position by
                    // position.
                    let merged = &mut functions[idx];
                    merged.symbols.push(record.name.clone());
                    merged.count = merged.count.saturating_add(record.count);
                    for (into, from) in merged.branches.iter_mut().zip(&branches) {
                        into.true_count = into.true_count.saturating_add(from.true_count);
                        into.false_count = into.false_count.saturating_add(from.false_count);
                    }
                }
                None => {
                    by_key.insert(key.clone(), functions.len());
                    functions.push(CoveredFunction {
                        symbols: vec![record.name.clone()],
                        count: record.count,
                        branches,
                        key,
                    });
                }
            }
        }
        let mut by_span: HashMap<SourceSpan, Vec<usize>> = HashMap::new();
        for (idx, f) in functions.iter().enumerate() {
            for (_, span) in &f.key {
                let ids = by_span.entry(span.clone()).or_default();
                if ids.last() != Some(&idx) {
                    ids.push(idx);
                }
            }
        }
        CoverageIndex { functions, by_span }
    }

    /// The merged functions, in first-seen record order.
    pub fn functions(&self) -> &[CoveredFunction] {
        &self.functions
    }

    /// Look up the source function containing `spans` (normalised).
    ///
    /// A merged function contains a span when the span is one of its
    /// code or branch regions. Exactly one containing function whose
    /// records all demangle to one path is a match; more than one
    /// containing function, or one whose records name different
    /// functions, is [`FunctionMatch::Ambiguous`].
    pub fn resolve<'s>(
        &self,
        spans: impl IntoIterator<Item = &'s SourceSpan>,
    ) -> FunctionMatch<'_> {
        let hits: BTreeSet<usize> = spans
            .into_iter()
            .filter_map(|s| self.by_span.get(s))
            .flatten()
            .copied()
            .collect();
        let mut hits = hits.into_iter();
        let Some(first) = hits.next() else {
            return FunctionMatch::Missing;
        };
        let rest: Vec<usize> = hits.collect();
        if !rest.is_empty() {
            let names: BTreeSet<String> = std::iter::once(first)
                .chain(rest.iter().copied())
                .flat_map(|i| self.functions[i].display_names())
                .collect();
            return FunctionMatch::Ambiguous(format!(
                "the decision's spans fall in {} coverage functions with different \
                 span sets ({})",
                rest.len() + 1,
                names.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        let f = &self.functions[first];
        let names = f.display_names();
        if names.len() > 1 {
            return FunctionMatch::Ambiguous(format!(
                "coverage records for {} differently named functions share the \
                 decision's spans ({}); Floyd cannot tell one generic function from \
                 distinct macro-generated ones",
                names.len(),
                names.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        FunctionMatch::Found(f)
    }
}

/// Report name for a coverage symbol: `rustc_demangle`'s alternate
/// form (`{:#}`, which omits crate disambiguators such as
/// `chrono[ab58c862027fa3d9]` and legacy `::h<hash>` suffixes) with
/// generic arguments removed, so every monomorphisation of a generic
/// function has the same name.
///
/// `_RNvXs_NtNtCseI4Ere5mNTx_6chrono5naive4timeNtB4_9NaiveTimeNtNtB8_6traits8Timelike4hourB8_`
/// becomes `<chrono::naive::time::NaiveTime as chrono::traits::Timelike>::hour`.
pub fn display_name(symbol: &str) -> String {
    strip_generic_args(&format!("{:#}", rustc_demangle::demangle(symbol)))
}

/// Remove generic argument lists (`::<..>` turbofish and `Type<..>`)
/// from a demangled path, keeping qualified-path brackets
/// (`<T as Trait>::m`, `<Type>::m`). A `<` opens an argument list
/// when it follows `::` or an identifier character; otherwise it
/// opens a qualified path. The `>` of `->` (fn pointer types) is not
/// a bracket.
fn strip_generic_args(path: &str) -> String {
    let chars: Vec<char> = path.chars().collect();
    let mut out = String::with_capacity(path.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let opens_args = c == '<'
            && (out.ends_with("::")
                || out
                    .chars()
                    .next_back()
                    .is_some_and(|p| p.is_alphanumeric() || p == '_'));
        if !opens_args {
            out.push(c);
            i += 1;
            continue;
        }
        if out.ends_with("::") {
            out.truncate(out.len() - 2);
        }
        let mut depth = 0usize;
        while i < chars.len() {
            match chars[i] {
                '<' => depth += 1,
                '>' if i > 0 && chars[i - 1] == '-' => {}
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile;

    const ROOT: &str = "/home/ubuntu/work/verify-span/chrono/src-copy";

    fn chrono_paths() -> PathNormaliser {
        PathNormaliser::new(Some(Path::new(ROOT)))
    }

    fn span(file: &str, l1: u32, c1: u32, l2: u32, c2: u32) -> SourceSpan {
        SourceSpan {
            file: file.to_string(),
            start_line: l1,
            start_col: c1,
            end_line: l2,
            end_col: c2,
        }
    }

    fn export(functions: &[&str]) -> CoverageReport {
        let json = format!(
            r#"{{"data": [{{"files": [], "functions": [{}], "totals": {{}}}}],
                "type": "llvm.coverage.json.export", "version": "2.0.1"}}"#,
            functions.join(",")
        );
        profile::parse(&json).expect("coverage parses")
    }

    // Real records from chrono's lib unit-test binary, whole suite,
    // `-Cinstrument-coverage -Zcoverage-options=branch,condition`.

    /// `<chrono::offset::fixed::FixedOffset>::east_opt` (inherent
    /// impl method).
    const EAST_OPT: &str = r#"{"name": "_RNvMNtNtCseI4Ere5mNTx_6chrono6offset5fixedNtB2_11FixedOffset8east_opt", "count": 2339, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/offset/fixed.rs"], "regions": [[62, 5, 62, 60, 2339, 0, 0, 0], [63, 12, 63, 26, 2339, 0, 0, 0], [63, 30, 63, 43, 2338, 0, 0, 0], [64, 13, 64, 56, 2337, 0, 0, 0], [66, 13, 66, 17, 2, 0, 0, 0], [68, 5, 68, 6, 2339, 0, 0, 0]], "branches": [[63, 12, 63, 26, 2338, 1, 0, 0, 4], [63, 30, 63, 43, 2337, 1, 0, 0, 4]]}"#;

    /// `<NaiveTime as Timelike>::with_nanosecond` (trait impl
    /// method).
    const WITH_NANOSECOND: &str = r#"{"name": "_RNvXs_NtNtCseI4Ere5mNTx_6chrono5naive4timeNtB4_9NaiveTimeNtNtB8_6traits8Timelike15with_nanosecondB8_", "count": 66, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/naive/time/mod.rs"], "regions": [[1109, 5, 1109, 62, 66, 0, 0, 0], [1110, 12, 1110, 33, 66, 0, 0, 0], [1111, 13, 1111, 24, 0, 0, 0, 0], [1112, 9, 1112, 10, 66, 0, 0, 0], [1113, 9, 1113, 48, 66, 0, 0, 0], [1114, 5, 1114, 6, 66, 0, 0, 0]], "branches": [[1110, 12, 1110, 33, 0, 66, 0, 0, 4]]}"#;

    /// `set_if_consistent::<T>`, four monomorphisations.
    const SET_IF_CONSISTENT: [&str; 4] = [
        r#"{"name": "_RINvNtNtCseI4Ere5mNTx_6chrono6format6parsed17set_if_consistentNtNtB6_7weekday7WeekdayEB6_", "count": 114, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/format/parsed.rs"], "regions": [[180, 1, 180, 83, 114, 0, 0, 0], [181, 11, 181, 14, 114, 0, 0, 0], [182, 22, 182, 33, 0, 0, 0, 0], [182, 37, 182, 52, 0, 0, 0, 0], [184, 13, 184, 29, 114, 0, 0, 0], [185, 13, 185, 19, 114, 0, 0, 0], [188, 1, 188, 2, 114, 0, 0, 0]], "branches": [[182, 22, 182, 33, 0, 0, 0, 0, 4]]}"#,
        r#"{"name": "_RINvNtNtCseI4Ere5mNTx_6chrono6format6parsed17set_if_consistentlEB6_", "count": 890, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/format/parsed.rs"], "regions": [[180, 1, 180, 83, 890, 0, 0, 0], [181, 11, 181, 14, 890, 0, 0, 0], [182, 22, 182, 33, 16, 0, 0, 0], [182, 37, 182, 52, 9, 0, 0, 0], [184, 13, 184, 29, 881, 0, 0, 0], [185, 13, 185, 19, 881, 0, 0, 0], [188, 1, 188, 2, 890, 0, 0, 0]], "branches": [[182, 22, 182, 33, 9, 7, 0, 0, 4]]}"#,
        r#"{"name": "_RINvNtNtCseI4Ere5mNTx_6chrono6format6parsed17set_if_consistentmEB6_", "count": 2597, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/format/parsed.rs"], "regions": [[180, 1, 180, 83, 2597, 0, 0, 0], [181, 11, 181, 14, 2597, 0, 0, 0], [182, 22, 182, 33, 31, 0, 0, 0], [182, 37, 182, 52, 14, 0, 0, 0], [184, 13, 184, 29, 2583, 0, 0, 0], [185, 13, 185, 19, 2583, 0, 0, 0], [188, 1, 188, 2, 2597, 0, 0, 0]], "branches": [[182, 22, 182, 33, 14, 17, 0, 0, 4]]}"#,
        r#"{"name": "_RINvNtNtCseI4Ere5mNTx_6chrono6format6parsed17set_if_consistentxEB6_", "count": 14, "filenames": ["/home/ubuntu/work/verify-span/chrono/src-copy/src/format/parsed.rs"], "regions": [[180, 1, 180, 83, 14, 0, 0, 0], [181, 11, 181, 14, 14, 0, 0, 0], [182, 22, 182, 33, 2, 0, 0, 0], [182, 37, 182, 52, 2, 0, 0, 0], [184, 13, 184, 29, 12, 0, 0, 0], [185, 13, 185, 19, 12, 0, 0, 0], [188, 1, 188, 2, 14, 0, 0, 0]], "branches": [[182, 22, 182, 33, 2, 0, 0, 0, 4]]}"#,
    ];

    /// num-traits' `float_impl!`-generated `FloatCore` methods: two
    /// distinct functions per float type, all with the macro body's
    /// spans (chrono probe export, all crates instrumented).
    const FLOAT_CORE: [&str; 2] = [
        r#"{"name": "_RNvXNtCsbHyJ5vkuTKm_10num_traits5floatfNtB2_9FloatCore10to_degreesB4_", "count": 0, "filenames": ["/home/ubuntu/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/num-traits-0.2.19/src/macros.rs"], "regions": [[9, 13, 9, 57, 0, 0, 0, 0], [10, 17, 10, 50, 0, 0, 0, 0], [11, 13, 11, 14, 0, 0, 0, 0]], "branches": []}"#,
        r#"{"name": "_RNvXNtCsbHyJ5vkuTKm_10num_traits5floatfNtB2_9FloatCore10to_radiansB4_", "count": 0, "filenames": ["/home/ubuntu/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/num-traits-0.2.19/src/macros.rs"], "regions": [[9, 13, 9, 57, 0, 0, 0, 0], [10, 17, 10, 50, 0, 0, 0, 0], [11, 13, 11, 14, 0, 0, 0, 0]], "branches": []}"#,
    ];

    #[test]
    fn normalises_absolute_path_under_root_to_relative() {
        let p = chrono_paths();
        assert_eq!(
            p.normalise(&format!("{ROOT}/src/offset/fixed.rs"))
                .as_deref(),
            Some("src/offset/fixed.rs")
        );
        // MIR prints the path rustc was given: relative.
        assert_eq!(
            p.normalise("src/offset/fixed.rs").as_deref(),
            Some("src/offset/fixed.rs")
        );
        assert_eq!(
            p.normalise("./src/naive/../offset/fixed.rs").as_deref(),
            Some("src/offset/fixed.rs")
        );
    }

    #[test]
    fn normalises_workspace_member_paths() {
        // cargo runs rustc in the workspace root and passes member
        // sources relative to it; llvm-cov reports them absolute.
        let p = PathNormaliser::new(Some(Path::new("/ws")));
        let mir = p.normalise("crates/core/src/lib.rs");
        let cov = p.normalise("/ws/crates/core/src/lib.rs");
        assert_eq!(mir.as_deref(), Some("crates/core/src/lib.rs"));
        assert_eq!(mir, cov);
        // Not a suffix match: another member's lib.rs stays apart.
        assert_ne!(p.normalise("/ws/crates/other/src/lib.rs"), mir);
        assert_ne!(p.normalise("src/lib.rs"), mir);
    }

    #[test]
    fn keeps_paths_outside_root_absolute() {
        let p = chrono_paths();
        let reg = "/home/ubuntu/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/insta-1.49.0/src/macros.rs";
        assert_eq!(p.normalise(reg).as_deref(), Some(reg));
        assert!(p.is_outside_root(reg));
        assert!(!p.is_outside_root("src/lib.rs"));
        assert!(!PathNormaliser::new(None).is_outside_root(reg));
    }

    #[test]
    fn rustc_library_paths_never_match() {
        let p = chrono_paths();
        let core = "/rustc/4d91de4e48198da2e33413efdcd9cd2cc0c46688/library/core/src/macros/mod.rs";
        assert_eq!(p.normalise(core), None);
        assert_eq!(p.span(&span(core, 1, 1, 1, 2)), None);
        assert!(p.is_outside_root(core));
        // A record whose only spans are std spans is not indexed.
        let cov = export(&[r#"{"name": "f", "count": 1,
            "filenames": ["/rustc/4d91de4e48198da2e33413efdcd9cd2cc0c46688/library/core/src/macros/mod.rs"],
            "regions": [[1, 1, 1, 2, 1, 0, 0, 0]], "branches": [[1, 1, 1, 2, 1, 0, 0, 0, 4]]}"#]);
        assert!(CoverageIndex::build(&cov, &p).functions().is_empty());
    }

    #[test]
    fn normalises_without_root_lexically_only() {
        let p = PathNormaliser::new(None);
        assert_eq!(
            p.normalise("/tmp/x/./src/../src/lib.rs").as_deref(),
            Some("/tmp/x/src/lib.rs")
        );
        assert_eq!(p.normalise("src/lib.rs").as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn impl_method_resolves_by_condition_span() {
        // MIR names it `offset::fixed::<impl at src/offset/fixed.rs:34:1: 34:17>::east_opt`;
        // no name join can reach the record, the span join does.
        let index = CoverageIndex::build(&export(&[EAST_OPT, WITH_NANOSECOND]), &chrono_paths());
        let cond = span("src/offset/fixed.rs", 63, 12, 63, 26);
        let FunctionMatch::Found(f) = index.resolve([&cond]) else {
            panic!("east_opt not found");
        };
        assert_eq!(f.count, 2339);
        assert_eq!(
            f.display_name().as_deref(),
            Some("<chrono::offset::fixed::FixedOffset>::east_opt")
        );
        let c = f.branch_counts(&cond).expect("branch at condition span");
        assert_eq!((c.true_count, c.false_count), (2338, 1));
        let c = f
            .branch_counts(&span("src/offset/fixed.rs", 63, 30, 63, 43))
            .expect("second condition");
        assert_eq!((c.true_count, c.false_count), (2337, 1));
    }

    #[test]
    fn trait_impl_method_resolves_by_condition_span() {
        let index = CoverageIndex::build(&export(&[EAST_OPT, WITH_NANOSECOND]), &chrono_paths());
        let cond = span("src/naive/time/mod.rs", 1110, 12, 1110, 33);
        let FunctionMatch::Found(f) = index.resolve([&cond]) else {
            panic!("with_nanosecond not found");
        };
        assert_eq!(f.count, 66);
        assert_eq!(
            f.display_name().as_deref(),
            Some("<chrono::naive::time::NaiveTime as chrono::traits::Timelike>::with_nanosecond")
        );
        let c = f.branch_counts(&cond).expect("branch");
        assert_eq!((c.true_count, c.false_count), (0, 66));
    }

    #[test]
    fn generic_instantiations_merge_by_summing_counts() {
        let index = CoverageIndex::build(&export(&SET_IF_CONSISTENT), &chrono_paths());
        assert_eq!(index.functions().len(), 1, "one source function");
        let cond = span("src/format/parsed.rs", 182, 22, 182, 33);
        let FunctionMatch::Found(f) = index.resolve([&cond]) else {
            panic!("set_if_consistent not found");
        };
        assert_eq!(f.symbols.len(), 4);
        assert_eq!(f.count, 114 + 890 + 2597 + 14);
        let c = f.branch_counts(&cond).expect("branch");
        assert_eq!(c.true_count, 9 + 14 + 2);
        assert_eq!(c.false_count, 7 + 17);
        assert_eq!(
            f.display_name().as_deref(),
            Some("chrono::format::parsed::set_if_consistent")
        );
    }

    #[test]
    fn two_instantiations_merge_even_when_one_is_unexecuted() {
        // `Weekday` instantiation (114 calls, 0 branch evaluations)
        // plus the `i64` one: the sum, not the first record.
        let index = CoverageIndex::build(
            &export(&[SET_IF_CONSISTENT[0], SET_IF_CONSISTENT[3]]),
            &chrono_paths(),
        );
        let cond = span("src/format/parsed.rs", 182, 22, 182, 33);
        let FunctionMatch::Found(f) = index.resolve([&cond]) else {
            panic!("not found");
        };
        assert_eq!(f.count, 128);
        let c = f.branch_counts(&cond).expect("branch");
        assert_eq!((c.true_count, c.false_count), (2, 0));
    }

    #[test]
    fn distinct_functions_sharing_spans_are_ambiguous() {
        let p = PathNormaliser::new(Some(Path::new("/home/ubuntu/work/probe-chrono")));
        let index = CoverageIndex::build(&export(&FLOAT_CORE), &p);
        let body = span(
            "/home/ubuntu/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/num-traits-0.2.19/src/macros.rs",
            10,
            17,
            10,
            50,
        );
        match index.resolve([&body]) {
            FunctionMatch::Ambiguous(why) => {
                assert!(
                    why.contains("to_degrees") && why.contains("to_radians"),
                    "{why}"
                );
            }
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn spans_in_two_functions_are_ambiguous() {
        let index = CoverageIndex::build(&export(&[EAST_OPT, WITH_NANOSECOND]), &chrono_paths());
        let a = span("src/offset/fixed.rs", 63, 12, 63, 26);
        let b = span("src/naive/time/mod.rs", 1110, 12, 1110, 33);
        assert!(matches!(
            index.resolve([&a, &b]),
            FunctionMatch::Ambiguous(_)
        ));
    }

    #[test]
    fn unknown_span_is_missing_and_files_must_match_exactly() {
        let index = CoverageIndex::build(&export(&[EAST_OPT]), &chrono_paths());
        // Right lines, wrong file.
        let other = span("src/offset/local/mod.rs", 63, 12, 63, 26);
        assert!(matches!(index.resolve([&other]), FunctionMatch::Missing));
        // Right file, shifted column.
        let shifted = span("src/offset/fixed.rs", 63, 13, 63, 26);
        assert!(matches!(index.resolve([&shifted]), FunctionMatch::Missing));
    }

    #[test]
    fn build_for_keeps_only_wanted_functions() {
        let wanted: HashSet<SourceSpan> = [span("src/offset/fixed.rs", 63, 12, 63, 26)].into();
        let index = CoverageIndex::build_for(
            &export(&[EAST_OPT, WITH_NANOSECOND]),
            &chrono_paths(),
            &wanted,
        );
        assert_eq!(index.functions().len(), 1);
        assert_eq!(index.functions()[0].count, 2339);
    }

    #[test]
    fn display_name_keeps_segments_after_h() {
        // The old `rsplit_once("::h")` legacy-hash strip truncated
        // this v0 name to `<NaiveTime as Timelike>`.
        assert_eq!(
            display_name("_RNvXs_NtNtCseI4Ere5mNTx_6chrono5naive4timeNtB4_9NaiveTimeNtNtB8_6traits8Timelike4hourB8_"),
            "<chrono::naive::time::NaiveTime as chrono::traits::Timelike>::hour"
        );
        // Legacy mangling: the alternate form drops the hash itself.
        assert_eq!(
            display_name("_ZN4core3fmt5write17h0123456789abcdefE"),
            "core::fmt::write"
        );
    }

    #[test]
    fn display_name_strips_generic_arguments() {
        assert_eq!(
            display_name(
                "_RINvNtNtCseI4Ere5mNTx_6chrono6format6parsed17set_if_consistentNtNtB6_7weekday7WeekdayEB6_"
            ),
            "chrono::format::parsed::set_if_consistent"
        );
        // Generic impl: both self-type instantiations give one name.
        let utc = display_name("_RNvXsc_NtCseI4Ere5mNTx_6chrono8datetimeINtB5_8DateTimeNtNtNtB7_6offset3utc3UtcENtNtB7_6traits8Timelike15with_nanosecondB7_");
        let fixed = display_name("_RNvXsc_NtCseI4Ere5mNTx_6chrono8datetimeINtB5_8DateTimeNtNtNtB7_6offset5fixed11FixedOffsetENtNtB7_6traits8Timelike15with_nanosecondB7_");
        assert_eq!(
            utc,
            "<chrono::datetime::DateTime as chrono::traits::Timelike>::with_nanosecond"
        );
        assert_eq!(utc, fixed);
        assert_eq!(
            strip_generic_args("<a::LocalResult<a::Utc>>::and_then::<b::DateTime<a::Utc>, <a::Utc as a::TimeZone>::from_local::{closure#0}>"),
            "<a::LocalResult>::and_then"
        );
        assert_eq!(
            strip_generic_args("<fn(u8) -> bool as a::Pred>::check"),
            "<fn(u8) -> bool as a::Pred>::check"
        );
    }
}
