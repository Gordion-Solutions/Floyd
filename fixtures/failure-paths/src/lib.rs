//! Fixture for `cargo floyd test` error paths (see
//! `cargo-floyd/tests/error_paths.rs`).
//!
//! - Without `--features builds` the crate does not compile, so the
//!   instrumented build fails with the `compile_error!` below.
//! - With `--features builds` it analyses `gate` (feature passthrough).
//! - `FLOYD_FIXTURE_FAIL_LIST` set: the lib test binary exits with
//!   status 3 before `main`, so `--list` fails (test discovery).
//! - `FLOYD_FIXTURE_NO_PROFILE` set: the test `both` deletes its
//!   profile (`LLVM_PROFILE_FILE`, `%p` expanded) and aborts, so
//!   the run leaves no coverage profile (test run). (Aborting alone
//!   is not enough: the profile runtime creates the file, empty, at
//!   startup.)

#[cfg(not(feature = "builds"))]
compile_error!("failure-paths fixture: build with --features builds");

/// The decision Floyd analyses.
pub fn gate(a: bool, b: bool) -> bool {
    a && b
}

/// Runs before `main` in the test binary, so before libtest parses
/// `--list`.
#[cfg(test)]
extern "C" fn fail_list() {
    if std::env::var_os("FLOYD_FIXTURE_FAIL_LIST").is_some() {
        eprintln!("failure-paths fixture: FLOYD_FIXTURE_FAIL_LIST is set, refusing to start");
        std::process::exit(3);
    }
}

#[cfg(test)]
#[used]
#[cfg_attr(target_os = "macos", link_section = "__DATA,__mod_init_func")]
#[cfg_attr(not(target_os = "macos"), link_section = ".init_array")]
static FAIL_LIST: extern "C" fn() = fail_list;

#[cfg(test)]
mod tests {
    #[test]
    fn both() {
        if std::env::var_os("FLOYD_FIXTURE_NO_PROFILE").is_some() {
            if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
                // Floyd names the file with `%p` (the runtime's pid
                // placeholder) so that processes a test spawns write
                // profiles of their own.
                let own = profile.replace("%p", &std::process::id().to_string());
                let _ = std::fs::remove_file(own);
            }
            std::process::abort();
        }
        assert!(super::gate(true, true));
    }

    #[test]
    fn first_false() {
        assert!(!super::gate(false, true));
    }

    #[test]
    fn second_false() {
        assert!(!super::gate(true, false));
    }
}
