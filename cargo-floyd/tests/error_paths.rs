//! End-to-end checks of `cargo floyd test`'s error paths and
//! options, on standalone fixtures under `fixtures/`:
//!
//! - a proc-macro crate's test binary (which links `libstd`
//!   dynamically) is listed and run.
//!
//! Tagged `#[ignore]`: needs the nightly toolchain with
//! `llvm-tools-preview`. Run with
//! `cargo test -p cargo-floyd --test error_paths -- --ignored`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not JSON ({e}):\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name)
}

/// `cargo floyd test <args..> fixtures/<name>` with `env` set. Each
/// fixture gets its own target directory.
fn floyd(name: &str, args: &[&str], env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cargo-floyd"));
    cmd.args(["floyd", "test"])
        .args(args)
        .arg(fixture(name))
        .env(
            "CARGO_TARGET_DIR",
            Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-target")),
        );
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run cargo-floyd");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().expect("exit code"),
    }
}

#[test]
#[ignore = "requires nightly Rust + llvm-tools-preview component"]
fn proc_macro_test_binary_is_listed_and_run() {
    let run = floyd("proc-macro-test", &["--format=json"], &[]);
    let j = run.json();
    assert_eq!(j["verdict"], "analysed", "{}", run.stderr);
    assert_eq!(j["tests_discovered"].as_array().unwrap().len(), 3);
    assert_eq!(j["summary"]["percentage"], 100.0);
    assert_eq!(run.code, 0);
}
