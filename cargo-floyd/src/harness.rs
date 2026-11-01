//! Detect cargo targets built with `harness = false`.
//!
//! Such targets are ordinary programs with their own `main`. They do
//! not understand libtest flags, so invoking one with `--list` runs
//! the program itself (cortex-m's `xtask/tests/ci.rs` runs the
//! crate's whole CI job). Cargo does not report `harness` in
//! `cargo metadata`, the unit graph, or `--message-format=json`
//! artifact messages, so it is read from the package manifest.

use std::path::Path;

/// Does the target named `target_name` with cargo kinds
/// `target_kinds` and root file `src_path` use the libtest harness,
/// according to the manifest text `manifest_toml` located in
/// `manifest_dir`?
///
/// Returns `true` (cargo's default) unless the matching target table
/// sets `harness = false`. A manifest that cannot be parsed also
/// yields `true`: cargo already accepted it, and the caller only
/// needs to know about the explicit opt-out.
pub fn target_uses_harness(
    manifest_toml: &str,
    manifest_dir: &Path,
    target_name: &str,
    target_kinds: &[String],
    src_path: &Path,
) -> bool {
    let Ok(doc) = manifest_toml.parse::<toml::Table>() else {
        return true;
    };
    let has_kind = |k: &str| target_kinds.iter().any(|t| t == k);
    let section = if has_kind("test") {
        "test"
    } else if has_kind("bench") {
        "bench"
    } else if has_kind("bin") {
        "bin"
    } else if has_kind("example") {
        "example"
    } else {
        "lib"
    };
    let harness_of = |t: &toml::Table| t.get("harness").and_then(|h| h.as_bool());
    let explicit = if section == "lib" {
        doc.get("lib")
            .and_then(|l| l.as_table())
            .and_then(harness_of)
    } else {
        doc.get(section)
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .filter_map(|e| e.as_table())
            .find(|t| {
                let by_name = t.get("name").and_then(|n| n.as_str()) == Some(target_name);
                let by_path = t
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| paths_equal(&manifest_dir.join(p), src_path))
                    .unwrap_or(false);
                by_name || by_path
            })
            .and_then(harness_of)
    };
    explicit.unwrap_or(true)
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(k: &str) -> Vec<String> {
        vec![k.to_string()]
    }

    #[test]
    fn harness_false_test_target_is_detected_by_name() {
        let m = "[package]\nname = \"xtask\"\n\n[[test]]\nname = \"ci\"\nharness = false\n";
        assert!(!target_uses_harness(
            m,
            Path::new("/nonexistent"),
            "ci",
            &kinds("test"),
            Path::new("/nonexistent/tests/ci.rs"),
        ));
        // A different test target in the same package keeps the harness.
        assert!(target_uses_harness(
            m,
            Path::new("/nonexistent"),
            "other",
            &kinds("test"),
            Path::new("/nonexistent/tests/other.rs"),
        ));
    }

    #[test]
    fn harness_false_is_matched_by_path_and_by_section() {
        let m = "[[test]]\npath = \"tests/ci.rs\"\nharness = false\n\n[lib]\nharness = false\n";
        assert!(!target_uses_harness(
            m,
            Path::new("/nonexistent"),
            "ci",
            &kinds("test"),
            Path::new("/nonexistent/tests/ci.rs"),
        ));
        assert!(!target_uses_harness(
            m,
            Path::new("/nonexistent"),
            "mylib",
            &kinds("lib"),
            Path::new("/nonexistent/src/lib.rs"),
        ));
        // A bin target named like the test is not affected.
        assert!(target_uses_harness(
            m,
            Path::new("/nonexistent"),
            "ci",
            &kinds("bin"),
            Path::new("/nonexistent/src/main.rs"),
        ));
    }

    #[test]
    fn default_and_unparseable_manifests_use_the_harness() {
        assert!(target_uses_harness(
            "[package]\nname = \"p\"\n",
            Path::new("/x"),
            "p",
            &kinds("lib"),
            Path::new("/x/src/lib.rs"),
        ));
        assert!(target_uses_harness(
            "not = [valid",
            Path::new("/x"),
            "p",
            &kinds("lib"),
            Path::new("/x/src/lib.rs"),
        ));
    }
}
