//! Coverage data ingested from `llvm-cov export`.
//!
//! Per [ADR-0002], Floyd's runtime pipeline shells out to
//! `llvm-profdata merge` then `llvm-cov export --format=text` from the
//! `llvm-tools-preview` rustup component, and ingests the resulting
//! JSON. This module converts that JSON into typed Rust structures
//! the rest of the engine consumes — chiefly [`CoverageReport`] and
//! the per-function [`Branch`] entries that the `correlate` module
//! (planned per ADR-0002) will join against MIR decisions by source
//! span.
//!
//! ## Schema
//!
//! `llvm-cov export --format=text` emits one top-level object per
//! invocation:
//!
//! ```json
//! {
//!   "data":    [ { "files": [...], "functions": [...], "totals": {...} } ],
//!   "type":    "llvm.coverage.json.export",
//!   "version": "..."
//! }
//! ```
//!
//! Within `functions[].branches`, each entry is a positionally-encoded
//! 9-tuple:
//!
//! ```text
//! [start_line, start_col, end_line, end_col,
//!  true_counter, false_counter,
//!  file_id, expanded_file_id, kind]
//! ```
//!
//! - `start_line, start_col, end_line, end_col`: source span of the
//!   atomic boolean condition.
//! - `true_counter, false_counter`: execution counts for the true /
//!   false outcomes. When the binary is executed in isolation per
//!   test (planned `runner` module, per ADR-0002), these are per-test
//!   counts.
//! - `file_id`: index into `functions[].filenames`.
//! - `expanded_file_id`: relevant for macro expansions; ignored in
//!   Phase 1.
//! - `kind`: `4` = branch region. Other kinds (regions, expansions,
//!   gaps) are filtered out of the parsed `branches` collection — see
//!   [`Branch::KIND_BRANCH`].
//!
//! [ADR-0002]: ../../../architecture/decisions/0002-runtime-pipeline.md

use crate::mir::SourceSpan;

/// A parsed coverage report.
#[derive(Debug, Default, Clone)]
pub struct CoverageReport {
    /// Per-function coverage entries.
    pub functions: Vec<FunctionCoverage>,
}

/// Coverage data for one function.
#[derive(Debug, Clone)]
pub struct FunctionCoverage {
    /// The function name as emitted by rustc (typically Rust-mangled).
    /// Demangling is deferred to the report stage.
    pub name: String,
    /// Number of times the function was entered.
    pub count: u64,
    /// Source file paths this function references; entries in
    /// [`Branch`] use indexes into this list.
    pub filenames: Vec<String>,
    /// Branch regions (one per atomic boolean condition). Only
    /// [`Branch::KIND_BRANCH`] entries are retained.
    pub branches: Vec<Branch>,
}

/// One branch region — typically one atomic boolean condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// Source location of the condition being tested.
    pub span: SourceSpan,
    /// Number of times the condition evaluated true.
    pub true_count: u64,
    /// Number of times the condition evaluated false.
    pub false_count: u64,
    /// Region kind. Branch regions are [`Self::KIND_BRANCH`] (`4`).
    pub kind: u32,
}

impl Branch {
    /// `kind` value LLVM uses for branch regions.
    pub const KIND_BRANCH: u32 = 4;
}

/// A parse error.
#[derive(Debug, Clone)]
pub struct ParseError {
    /// Human-readable description.
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "coverage parse error: {}", self.message)
    }
}

impl std::error::Error for ParseError {}

impl From<serde_json::Error> for ParseError {
    fn from(e: serde_json::Error) -> Self {
        ParseError {
            message: format!("JSON: {e}"),
        }
    }
}

/// Parse `llvm-cov export --format=text` JSON into a [`CoverageReport`].
///
/// All non-branch regions are filtered out at parse time; the
/// `branches` collections on the returned [`FunctionCoverage`] values
/// contain only [`Branch::KIND_BRANCH`] entries.
pub fn parse(json: &str) -> Result<CoverageReport, ParseError> {
    let raw: RawExport = serde_json::from_str(json)?;
    if raw.data.is_empty() {
        return Ok(CoverageReport::default());
    }
    let mut functions = Vec::new();
    for raw_data in raw.data {
        for raw_fn in raw_data.functions {
            functions.push(convert_function(raw_fn)?);
        }
    }
    Ok(CoverageReport { functions })
}

fn convert_function(raw: RawFunction) -> Result<FunctionCoverage, ParseError> {
    let mut branches = Vec::with_capacity(raw.branches.len());
    for arr in &raw.branches {
        if let Some(b) = convert_branch(arr, &raw.filenames) {
            if b.kind == Branch::KIND_BRANCH {
                branches.push(b);
            }
        }
    }
    Ok(FunctionCoverage {
        name: raw.name,
        count: raw.count,
        filenames: raw.filenames,
        branches,
    })
}

/// Decode one positional 9-tuple branch array. Returns `None` if the
/// array is malformed (too short, or `file_id` is out of range).
fn convert_branch(arr: &[i64], filenames: &[String]) -> Option<Branch> {
    if arr.len() < 9 {
        return None;
    }
    let start_line = u32::try_from(arr[0]).ok()?;
    let start_col = u32::try_from(arr[1]).ok()?;
    let end_line = u32::try_from(arr[2]).ok()?;
    let end_col = u32::try_from(arr[3]).ok()?;
    let true_count = u64::try_from(arr[4]).ok()?;
    let false_count = u64::try_from(arr[5]).ok()?;
    let file_id = usize::try_from(arr[6]).ok()?;
    let kind = u32::try_from(arr[8]).ok()?;
    let file = filenames.get(file_id).cloned()?;
    Some(Branch {
        span: SourceSpan {
            file,
            start_line,
            start_col,
            end_line,
            end_col,
        },
        true_count,
        false_count,
        kind,
    })
}

// ---------------------------------------------------------------------------
// Raw JSON schema (private). We model only the fields Floyd consumes;
// the others are skipped by serde.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct RawExport {
    data: Vec<RawData>,
    #[serde(rename = "type", default)]
    _ty: String,
    #[serde(default)]
    _version: String,
}

#[derive(serde::Deserialize)]
struct RawData {
    #[serde(default)]
    functions: Vec<RawFunction>,
}

#[derive(serde::Deserialize)]
struct RawFunction {
    name: String,
    #[serde(default)]
    count: u64,
    #[serde(default)]
    filenames: Vec<String>,
    #[serde(default)]
    branches: Vec<Vec<i64>>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `llvm-cov export --format=text` output captured during ADR-0002
    /// scouting against the corpus 001-simple-and pattern with two
    /// tests (`tests::ff` and `tests::tt`) executed. Reduced from the
    /// full export by trimming the empty `mcdc_records` and the file
    /// summary blocks — Floyd doesn't consume those.
    const SAMPLE_EXPORT: &str = r#"
    {
      "data": [
        {
          "files": [],
          "functions": [
            {
              "name": "_RNvCsb6BSQ92hH2b_19floyd_runtime_scout6decide",
              "count": 2,
              "filenames": ["/tmp/floyd-runtime-scout/src/lib.rs"],
              "regions": [[1, 1, 1, 40, 2, 0, 0, 0]],
              "branches": [
                [2, 5, 2, 6, 1, 1, 0, 0, 4],
                [2, 10, 2, 11, 1, 0, 0, 0, 4]
              ],
              "mcdc_records": []
            }
          ],
          "totals": {}
        }
      ],
      "type": "llvm.coverage.json.export",
      "version": "2.0.1"
    }
    "#;

    #[test]
    fn parses_one_function() {
        let r = parse(SAMPLE_EXPORT).expect("parses");
        assert_eq!(r.functions.len(), 1);
        let f = &r.functions[0];
        assert!(f.name.ends_with("6decide"));
        assert_eq!(f.count, 2);
        assert_eq!(f.filenames, vec!["/tmp/floyd-runtime-scout/src/lib.rs"]);
    }

    #[test]
    fn parses_two_branches_for_decide() {
        let r = parse(SAMPLE_EXPORT).expect("parses");
        let f = &r.functions[0];
        assert_eq!(f.branches.len(), 2);

        let b_a = &f.branches[0];
        assert_eq!(b_a.span.start_line, 2);
        assert_eq!(b_a.span.start_col, 5);
        assert_eq!(b_a.span.end_line, 2);
        assert_eq!(b_a.span.end_col, 6);
        assert_eq!(b_a.true_count, 1);
        assert_eq!(b_a.false_count, 1);
        assert_eq!(b_a.kind, Branch::KIND_BRANCH);

        let b_b = &f.branches[1];
        assert_eq!(b_b.span.start_line, 2);
        assert_eq!(b_b.span.start_col, 10);
        assert_eq!(b_b.span.end_line, 2);
        assert_eq!(b_b.span.end_col, 11);
        assert_eq!(b_b.true_count, 1);
        // The headline finding from ADR-0002 scouting: b's false_count
        // is 0 because the ff test short-circuited through &&.
        assert_eq!(b_b.false_count, 0);
    }

    #[test]
    fn span_file_resolves_via_filenames_array() {
        let r = parse(SAMPLE_EXPORT).expect("parses");
        let f = &r.functions[0];
        for b in &f.branches {
            assert_eq!(b.span.file, "/tmp/floyd-runtime-scout/src/lib.rs");
        }
    }

    #[test]
    fn filters_non_branch_kinds() {
        // Add a non-branch (kind=0) entry to the array. The parser
        // should discard it.
        let json = r#"
        {
          "data": [{
            "functions": [{
              "name": "f",
              "count": 1,
              "filenames": ["x.rs"],
              "regions": [],
              "branches": [
                [1, 1, 1, 2, 0, 0, 0, 0, 0],
                [1, 3, 1, 4, 1, 0, 0, 0, 4]
              ],
              "mcdc_records": []
            }]
          }],
          "type": "llvm.coverage.json.export",
          "version": "2.0.1"
        }
        "#;
        let r = parse(json).expect("parses");
        assert_eq!(r.functions[0].branches.len(), 1);
        assert_eq!(r.functions[0].branches[0].kind, Branch::KIND_BRANCH);
    }

    #[test]
    fn empty_data_yields_empty_report() {
        let json = r#"{ "data": [], "type": "x", "version": "1" }"#;
        let r = parse(json).expect("parses");
        assert!(r.functions.is_empty());
    }

    #[test]
    fn malformed_branch_array_is_dropped() {
        let json = r#"
        {
          "data": [{
            "functions": [{
              "name": "f",
              "count": 0,
              "filenames": ["x.rs"],
              "regions": [],
              "branches": [
                [1, 2],
                [1, 3, 1, 4, 1, 0, 0, 0, 4]
              ],
              "mcdc_records": []
            }]
          }],
          "type": "x",
          "version": "1"
        }
        "#;
        let r = parse(json).expect("parses");
        assert_eq!(r.functions[0].branches.len(), 1);
    }

    #[test]
    fn invalid_json_returns_error() {
        let err = parse("{ not json").unwrap_err();
        assert!(err.to_string().contains("JSON"));
    }
}
