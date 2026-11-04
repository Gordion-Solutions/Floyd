//! Class 7: a `harness = false` test target.
//!
//! Floyd must list this target in `skipped_no_harness` and never run
//! it, not even with `--list`. When `MCDC_CASES_NO_HARNESS_MARKER` is
//! set, the program writes that file, so a run can prove it did not
//! execute. It is the only caller of `harness_free_and`.

fn main() {
    if let Some(marker) = std::env::var_os("MCDC_CASES_NO_HARNESS_MARKER") {
        std::fs::write(marker, "no_harness ran\n").expect("write marker");
    }
    assert!(mcdc_cases::not_measurable::harness_free_and(true, true));
    assert!(!mcdc_cases::not_measurable::harness_free_and(false, true));
}
