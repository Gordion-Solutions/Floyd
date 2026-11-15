//! A build script, so the fixture covers the processes cargo runs
//! while Floyd builds it. Cargo runs it in the package directory; if
//! it were coverage-instrumented it would leave a
//! `default_*.profraw` there (see `src/child_process.rs`).
//!
//! It also checks that `RUSTC_BOOTSTRAP` does not reach it. With a
//! stable or beta compiler Floyd sets it on `rustc` alone, through
//! its `RUSTC_WRAPPER` shim; a build script that saw it would let
//! the probes it compiles use unstable features. The fixture tests
//! run cargo-floyd without `RUSTC_BOOTSTRAP`, so a value here can
//! only come from Floyd.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTC_BOOTSTRAP");
    if let Some(v) = std::env::var_os("RUSTC_BOOTSTRAP") {
        panic!(
            "RUSTC_BOOTSTRAP={} reached the build script of mcdc-cases; Floyd must \
             set it on the instrumented rustc only",
            v.to_string_lossy()
        );
    }
}
