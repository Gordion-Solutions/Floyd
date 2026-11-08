//! `RUSTC_WRAPPER` shim that scopes Floyd's instrumentation flags
//! to the primary package only.
//!
//! Without this shim, `--emit=mir,link` and `-Cinstrument-coverage`
//! applied via `RUSTFLAGS` would also apply to EVERY dependency
//! rustc builds. (The flags that are inert on dependencies,
//! `floyd::instrument::FINGERPRINTED_RUSTFLAGS`, do go through
//! cargo's rustflags, so that cargo fingerprints them.) On crates
//! with deep generic dep trees (`heapless`, embedded Rust in
//! general), that explodes the MIR text and crashes rustc under
//! 30GB RAM + 48GB swap on real builds.
//!
//! Cargo invokes `RUSTC_WRAPPER` as `<wrapper> <rustc_path>
//! <rustc_args...>`, and sets `CARGO_PRIMARY_PACKAGE=1` only when
//! compiling the package named in `Cargo.toml`. We inspect that
//! env var, append the instrumentation flags for the primary
//! package only, and exec the real rustc with the resulting
//! args. Dep packages, and the primary package's build script, pass
//! through unchanged.
//!
//! The shim must be fast and side-effect-free: cargo invokes it
//! once per compilation unit, including internal probe builds.

use std::env;
use std::ffi::OsString;
use std::process::{exit, Command};

fn main() {
    let mut args = env::args_os().skip(1);
    let Some(rustc_path) = args.next() else {
        eprintln!(
            "floyd-rustc-shim: expected the real rustc path as the \
             first argument. This binary is designed to be invoked \
             by cargo via `RUSTC_WRAPPER`; it is not a user-facing \
             tool."
        );
        exit(2);
    };
    let mut rustc_args: Vec<OsString> = args.collect();

    // CARGO_PRIMARY_PACKAGE=1 is set by cargo for the primary
    // package being built. Dep crates do NOT get this env var.
    let is_primary = env::var_os("CARGO_PRIMARY_PACKAGE").is_some();

    if is_primary && !is_build_script(&rustc_args) {
        // Floyd's MIR-text + coverage-branch instrumentation. Only
        // added for the primary package so dep crates (serde,
        // critical-section, hash32, …) do not also dump MIR and
        // get coverage-instrumented.
        rustc_args.push(OsString::from("-Cinstrument-coverage"));
        // `-Zcoverage-options` and `-Zmir-include-spans` normally
        // arrive through cargo's rustflags, which cargo fingerprints
        // (`floyd::instrument::FINGERPRINTED_RUSTFLAGS`). Add them
        // here only when they did not, e.g. a project whose
        // `target.<triple>.rustflags` makes cargo ignore the
        // `build.rustflags` Floyd passes.
        for flag in floyd::instrument::FINGERPRINTED_RUSTFLAGS {
            if !rustc_args.iter().any(|a| a == *flag) {
                rustc_args.push(OsString::from(*flag));
            }
        }
        rustc_args.push(OsString::from("--emit=mir,link"));
        // Memory-pressure guards for crates with heavy
        // monomorphisation (e.g. `heapless` with its const-generic
        // specialisation). Floyd uses MIR text spans, not DWARF,
        // so emitting zero debug info has no cost to the engine's
        // analysis. codegen-units=1 reduces rustc's peak memory
        // by compiling the whole crate as a single unit — slower
        // wall-clock, but avoids the parallel-codegen heap
        // explosion that OOM'd our 30GB runner on heapless
        // (336GB virtual, 30GB RSS before SIGKILL). These are
        // appended AFTER cargo's own `-C` flags so they win
        // when rustc merges duplicate codegen options.
        rustc_args.push(OsString::from("-Cdebuginfo=0"));
        rustc_args.push(OsString::from("-Ccodegen-units=1"));
    }

    let status = Command::new(&rustc_path)
        .args(&rustc_args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!(
                "floyd-rustc-shim: failed to exec real rustc at {}: {}",
                rustc_path.to_string_lossy(),
                e
            );
            exit(2);
        });
    exit(status.code().unwrap_or(1));
}

/// Is this the compilation of a build script? Cargo sets
/// `CARGO_PRIMARY_PACKAGE` for the primary package's build script
/// too, and names its crate `build_script_<file stem>`
/// (`build_script_build` for `build.rs`).
///
/// A build script is not instrumented: it holds none of the code
/// Floyd analyses, and cargo runs it in the package directory without
/// `LLVM_PROFILE_FILE`, so an instrumented one leaves a
/// `default_*.profraw` in the user's project.
fn is_build_script(rustc_args: &[OsString]) -> bool {
    rustc_args
        .windows(2)
        .find(|w| w[0] == "--crate-name")
        .and_then(|w| w[1].to_str())
        .is_some_and(|name| name.starts_with("build_script_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn build_scripts_are_recognised_by_crate_name() {
        assert!(is_build_script(&args(&[
            "--crate-name",
            "build_script_build",
            "--edition=2021",
            "build.rs",
            "--crate-type",
            "bin",
        ])));
        assert!(is_build_script(&args(&[
            "--crate-name",
            "build_script_gen"
        ])));
        assert!(!is_build_script(&args(&[
            "--crate-name",
            "anyhow",
            "src/lib.rs",
            "--crate-type",
            "lib",
        ])));
        assert!(!is_build_script(&args(&["-vV"])));
    }
}
