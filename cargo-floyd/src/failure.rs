//! Reports for the runtime-mode error paths: the instrumented build
//! failing, test discovery (`--list`) failing, and a per-test run
//! failing.
//!
//! This module holds the JSON `error` object and renders the JUnit
//! `<error>` document every error exit emits.

/// Why the run stopped before an analysis. Serialised as the
/// `error` object of `--format=json`.
#[derive(serde::Serialize, Debug, Clone, Default, PartialEq)]
pub struct RunError {
    /// The step that failed (`cargo build --tests`, `list_tests`,
    /// `run_test_isolated`, ...).
    pub stage: String,
    /// One-line description of the failure.
    pub message: String,
    /// The failing command's stderr with cargo's progress lines
    /// (`Compiling ...`, `Downloaded ...`) and backtrace frames
    /// removed, at most [`STDERR_LINES`] lines (the last ones).
    pub stderr: String,
}

/// How many stderr lines the `error.stderr` field keeps.
pub const STDERR_LINES: usize = 200;

/// `stderr` without cargo's progress lines and backtrace frames,
/// keeping the last [`STDERR_LINES`] lines.
pub fn trimmed_stderr(stderr: &str) -> String {
    const PROGRESS: &[&str] = &[
        "Compiling ",
        "Checking ",
        "Downloaded ",
        "Downloading ",
        "Updating ",
        "Locking ",
        "Adding ",
        "Fresh ",
        "Blocking ",
        "Building ",
    ];
    let kept: Vec<&str> = stderr
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !PROGRESS.iter().any(|p| t.starts_with(p)) && !is_backtrace_frame(t)
        })
        .collect();
    let start = kept.len().saturating_sub(STDERR_LINES);
    kept[start..].join("\n")
}

/// A line of a Rust backtrace: `12:   0x10a37bc8c - symbol` or the
/// `at path:line` continuation.
fn is_backtrace_frame(t: &str) -> bool {
    if let Some((n, _)) = t.split_once(':') {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    t.starts_with("at ") && t.contains(".rs:")
}

/// A JUnit document for a run that stopped with `error`: one
/// `<testcase>` carrying an `<error>` whose `type` is the verdict.
pub fn render_error_junit(source: &str, verdict: &str, error: &RunError) -> String {
    use std::fmt::Write as _;
    let esc = crate::xml_escape;
    let body = &error.stderr;
    let mut out = String::new();
    let _ = writeln!(out, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    let _ = writeln!(
        out,
        r#"<testsuites name="floyd" tests="1" failures="0" errors="1">"#
    );
    let _ = writeln!(
        out,
        r#"  <testsuite name="{}" tests="1" failures="0" errors="1">"#,
        esc(source)
    );
    let _ = writeln!(out, "    <properties>");
    let _ = writeln!(
        out,
        r#"      <property name="floyd.verdict" value="{}"/>"#,
        esc(verdict)
    );
    let _ = writeln!(out, "    </properties>");
    let _ = writeln!(
        out,
        r#"    <testcase name="{}" classname="floyd.run">"#,
        esc(&error.stage)
    );
    let _ = writeln!(
        out,
        r#"      <error message="{}" type="{}">{}</error>"#,
        esc(&error.message),
        esc(verdict),
        esc(body.trim_end())
    );
    let _ = writeln!(out, "    </testcase>");
    let _ = writeln!(out, "  </testsuite>");
    let _ = writeln!(out, "</testsuites>");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_junit_is_one_errored_testcase() {
        let e = RunError {
            stage: "cargo build --tests".into(),
            message: "instrumented build failed: 1 compiler error(s)".into(),
            stderr: "error: x < y".into(),
        };
        let x = render_error_junit("/p/Cargo.toml", "build_failed", &e);
        assert!(x.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#));
        assert!(x.contains(r#"<testsuites name="floyd" tests="1" failures="0" errors="1">"#));
        assert!(x.contains(r#"<property name="floyd.verdict" value="build_failed"/>"#));
        assert!(x.contains(
            r#"<error message="instrumented build failed: 1 compiler error(s)" type="build_failed">error: x &lt; y</error>"#
        ));
        assert!(x.trim_end().ends_with("</testsuites>"));
    }
}
