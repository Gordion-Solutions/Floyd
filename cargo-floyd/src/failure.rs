//! Reports for the runtime-mode error paths: the instrumented build
//! failing, test discovery (`--list`) failing, and a per-test run
//! failing.
//!
//! Floyd drives cargo with `--message-format=json`, so rustc's
//! diagnostics arrive on cargo's stdout as `compiler-message` JSON
//! records and never reach the terminal on their own. This module
//! pulls the error diagnostics back out of that stream, recognises a
//! rustc internal compiler error in cargo's stderr, and renders the
//! JSON `error` object and the JUnit `<error>` document every error
//! exit emits.

use std::path::{Path, PathBuf};

/// Why the run stopped before an analysis. Serialised as the
/// `error` object of `--format=json`.
#[derive(serde::Serialize, Debug, Clone, Default, PartialEq)]
pub struct RunError {
    /// The step that failed (`cargo build --tests`, `list_tests`,
    /// `run_test_isolated`, ...).
    pub stage: String,
    /// One-line description of the failure.
    pub message: String,
    /// rustc's rendered error diagnostics, in the order cargo
    /// reported them, duplicates removed. Empty unless the build
    /// failed with compiler errors.
    pub diagnostics: Vec<String>,
    /// Present when rustc crashed (an internal compiler error).
    pub ice: Option<Ice>,
    /// The failing command's stderr with cargo's progress lines
    /// (`Compiling ...`, `Downloaded ...`) and backtrace frames
    /// removed, at most [`STDERR_LINES`] lines (the last ones).
    pub stderr: String,
}

/// A rustc internal compiler error found in the build's stderr.
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
pub struct Ice {
    /// rustc's panic location and message and its "internal
    /// compiler error" / "unexpectedly panicked" lines.
    pub lines: Vec<String>,
    /// The `rustc-ice-*.txt` report rustc wrote, when it exists.
    pub report_file: Option<String>,
}

/// How many stderr lines the `error.stderr` field keeps.
pub const STDERR_LINES: usize = 200;

/// Diagnostics recovered from a failed `cargo build --message-format=json`.
#[derive(Debug, Default, PartialEq)]
pub struct BuildFailure {
    /// Rendered `compiler-message` diagnostics of level `error`
    /// (including `error: internal compiler error`), deduplicated.
    pub diagnostics: Vec<String>,
    /// The internal compiler error, if rustc crashed.
    pub ice: Option<Ice>,
    /// Cargo's own `error: could not compile ...` lines (one per
    /// failed unit).
    pub failed_units: Vec<String>,
}

impl BuildFailure {
    /// Parse cargo's stdout (the JSON message stream) and stderr.
    /// `search_dirs` are the directories rustc may have written a
    /// `rustc-ice-*.txt` into (the package and workspace roots), used
    /// when the stderr does not name the file; only files modified at
    /// or after `since` are considered.
    pub fn parse(
        stdout: &str,
        stderr: &str,
        search_dirs: &[&Path],
        since: Option<std::time::SystemTime>,
    ) -> Self {
        let mut diagnostics: Vec<String> = Vec::new();
        for line in stdout.lines() {
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
                continue;
            }
            let Some(m) = msg.get("message") else {
                continue;
            };
            let level = m.get("level").and_then(|l| l.as_str()).unwrap_or("");
            if !level.starts_with("error") {
                continue;
            }
            let rendered = m
                .get("rendered")
                .and_then(|r| r.as_str())
                .map(str::to_string)
                .or_else(|| {
                    m.get("message")
                        .and_then(|r| r.as_str())
                        .map(|s| format!("{level}: {s}"))
                });
            if let Some(r) = rendered {
                let r = r.trim_end().to_string();
                // `aborting due to N previous errors` is a summary,
                // not a diagnostic; cargo reports the count itself.
                if r.starts_with("error: aborting due to") {
                    continue;
                }
                if !diagnostics.contains(&r) {
                    diagnostics.push(r);
                }
            }
        }

        let failed_units = stderr
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("error: could not compile"))
            .map(str::to_string)
            .collect();

        BuildFailure {
            ice: find_ice(stderr, search_dirs, since),
            diagnostics,
            failed_units,
        }
    }

    /// The one-line summary printed after the diagnostics and used as
    /// the JSON `error.message`.
    pub fn summary_line(&self) -> String {
        let units = if self.failed_units.is_empty() {
            String::new()
        } else {
            let names: Vec<&str> = self
                .failed_units
                .iter()
                .map(|l| {
                    l.trim_start_matches("error: could not compile ")
                        .split(" due to")
                        .next()
                        .unwrap_or(l)
                        .trim_end_matches(';')
                })
                .collect();
            format!(" ({})", names.join(", "))
        };
        if self.ice.is_some() {
            format!("instrumented build failed: rustc crashed (internal compiler error){units}")
        } else if !self.diagnostics.is_empty() {
            format!(
                "instrumented build failed: {} compiler error(s){units}",
                self.diagnostics.len()
            )
        } else {
            format!("instrumented build failed{units}")
        }
    }

    /// Write the diagnostics, the ICE lines and the summary to `out`
    /// (stderr), in that order.
    pub fn print(&self, out: &mut dyn std::io::Write) {
        for d in &self.diagnostics {
            let _ = writeln!(out, "{d}");
            let _ = writeln!(out);
        }
        if let Some(ice) = &self.ice {
            for l in &ice.lines {
                let _ = writeln!(out, "{l}");
            }
            match &ice.report_file {
                Some(f) => {
                    let _ = writeln!(out, "rustc ICE report: {f}");
                }
                None => {
                    let _ = writeln!(out, "rustc ICE report: none found");
                }
            }
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "cargo-floyd: {}", self.summary_line());
    }
}

/// Find a rustc internal compiler error in `stderr`.
fn find_ice(
    stderr: &str,
    search_dirs: &[&Path],
    since: Option<std::time::SystemTime>,
) -> Option<Ice> {
    let lines: Vec<&str> = stderr.lines().collect();
    let mut ice_lines: Vec<String> = Vec::new();
    let mut report_file: Option<String> = None;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with("thread 'rustc'") && t.contains("panicked at") {
            ice_lines.push(t.to_string());
            // The panic message is the next line.
            if let Some(next) = lines.get(i + 1) {
                let next = next.trim();
                if !next.is_empty() && !next.starts_with("stack backtrace") {
                    ice_lines.push(next.to_string());
                }
            }
        } else if t.contains("internal compiler error")
            || t.contains("the compiler unexpectedly panicked")
        {
            ice_lines.push(t.to_string());
        }
        if let Some(rest) = t.split("please attach the file at `").nth(1) {
            if let Some(path) = rest.split('`').next() {
                report_file = Some(path.to_string());
            }
        }
    }
    if ice_lines.is_empty() {
        return None;
    }
    ice_lines.dedup();
    let report_file = report_file
        .filter(|f| Path::new(f).exists())
        .or_else(|| newest_ice_file(search_dirs, since).map(|p| p.display().to_string()));
    Some(Ice {
        lines: ice_lines,
        report_file,
    })
}

/// The newest `rustc-ice-*.txt` in `dirs` modified at or after `since`.
fn newest_ice_file(dirs: &[&Path], since: Option<std::time::SystemTime>) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !(name.starts_with("rustc-ice-") && name.ends_with(".txt")) {
                continue;
            }
            let Ok(modified) = e.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if since.is_some_and(|s| modified < s) {
                continue;
            }
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, e.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

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
    let mut body = String::new();
    for d in &error.diagnostics {
        body.push_str(d);
        body.push_str("\n\n");
    }
    if let Some(ice) = &error.ice {
        for l in &ice.lines {
            body.push_str(l);
            body.push('\n');
        }
        if let Some(f) = &ice.report_file {
            let _ = writeln!(body, "rustc ICE report: {f}");
        }
    }
    if body.is_empty() {
        body.push_str(&error.stderr);
    }
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

    const STDOUT: &str = r#"{"reason":"compiler-artifact","package_id":"x"}
{"reason":"compiler-message","message":{"level":"warning","rendered":"warning: unused\n"}}
{"reason":"compiler-message","message":{"level":"error","rendered":"error: cannot find attribute `rstest` in this scope\n --> tests/main.rs:25:3\n"}}
{"reason":"compiler-message","message":{"level":"error","rendered":"error: cannot find attribute `rstest` in this scope\n --> tests/main.rs:25:3\n"}}
{"reason":"compiler-message","message":{"level":"error","rendered":"error: aborting due to 1 previous error\n"}}
not json
"#;

    #[test]
    fn keeps_error_diagnostics_only_once_and_drops_the_abort_summary() {
        let stderr = "   Compiling time v0.3.55\nerror: could not compile `time` (test \"tests\") due to 1 previous error; 1 warning emitted\n";
        let f = BuildFailure::parse(STDOUT, stderr, &[], None);
        assert_eq!(f.diagnostics.len(), 1);
        assert!(f.diagnostics[0].contains("rstest"));
        assert!(f.ice.is_none());
        assert_eq!(
            f.summary_line(),
            "instrumented build failed: 1 compiler error(s) (`time` (test \"tests\"))"
        );
        let mut out = Vec::new();
        f.print(&mut out);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("cannot find attribute `rstest`"));
        assert!(out.ends_with("cargo-floyd: instrumented build failed: 1 compiler error(s) (`time` (test \"tests\"))\n"));
    }

    #[test]
    fn recognises_an_ice_and_its_report_file() {
        let dir = std::env::temp_dir().join(format!("floyd-ice-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ice_file = dir.join("rustc-ice-2026-11-04T11_31_56-1.txt");
        std::fs::write(&ice_file, "backtrace").unwrap();
        let stderr = format!(
            "   Compiling heapless v0.9.3\n\n\
             thread 'rustc' (1961027) panicked at /rustc-dev/x/compiler/rustc_abi/src/layout.rs:28:1:\n\
             assertion failed: value <= (0xFFFF_FF00 as usize)\n\
             stack backtrace:\n   0:        0x10cdd4244 - <std::sys::backtrace>::fmt\n\
             error: the compiler unexpectedly panicked. This is a bug\n\n\
             note: please attach the file at `{}` to your bug report\n\
             error: could not compile `heapless` (lib test)\n",
            ice_file.display()
        );
        let f = BuildFailure::parse("", &stderr, &[], None);
        let ice = f.ice.as_ref().expect("ice found");
        assert_eq!(ice.lines.len(), 3, "{:?}", ice.lines);
        assert!(ice.lines[1].contains("0xFFFF_FF00"));
        assert_eq!(
            ice.report_file.as_deref(),
            Some(ice_file.display().to_string().as_str())
        );
        assert!(f.summary_line().contains("internal compiler error"));
        assert!(f.summary_line().contains("`heapless` (lib test)"));
        let trimmed = trimmed_stderr(&stderr);
        assert!(!trimmed.contains("Compiling"));
        assert!(!trimmed.contains("0x10cdd4244"));
        assert!(trimmed.contains("assertion failed"));

        // Without the note, the newest ICE file in the search dirs is used.
        let no_note: String = stderr
            .lines()
            .filter(|l| !l.contains("please attach"))
            .collect::<Vec<_>>()
            .join("\n");
        let f = BuildFailure::parse("", &no_note, &[dir.as_path()], None);
        assert_eq!(
            f.ice.unwrap().report_file.as_deref(),
            Some(ice_file.display().to_string().as_str())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_junit_is_one_errored_testcase() {
        let e = RunError {
            stage: "cargo build --tests".into(),
            message: "instrumented build failed: 1 compiler error(s)".into(),
            diagnostics: vec!["error: x < y".into()],
            ice: None,
            stderr: String::new(),
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
