//! A build script, so the fixture covers the processes cargo runs
//! while Floyd builds it. Cargo runs it in the package directory; if
//! it were coverage-instrumented it would leave a
//! `default_*.profraw` there (see `src/child_process.rs`).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
