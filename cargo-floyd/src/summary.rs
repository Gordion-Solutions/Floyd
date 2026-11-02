//! The run-level MC/DC summary shared by the text, JSON and JUnit
//! reports, and the exit code derived from it.
//!
//! Floyd never states a coverage percentage it cannot stand behind:
//! when any condition's status is unknown (its only evidence came
//! from tests that evaluated the decision more than once), the
//! summary carries no percentage and the run does not pass.

use floyd::correlate::{NotMeasurable, NotMeasurableReason};
use floyd::masking::{ConditionStatus, RuntimeAnalysis};

/// Process exit code for a run that completed its analysis but did
/// not demonstrate MC/DC for every condition (at least one condition
/// is not exercised or unknown). Exit code 1 stays reserved for
/// errors and for the decline paths (no decision analysed).
pub const EXIT_MCDC_NOT_DEMONSTRATED: u8 = 2;

/// How many test names a condition status line or a JUnit failure
/// message lists before summarising the rest as `(+N more)`.
pub const TESTS_SHOWN: usize = 3;

/// Counts for one analysed decision. Serialised as the `summary`
/// object of `--format=json`.
#[derive(serde::Serialize, Debug, Clone, PartialEq)]
pub struct CoverageSummary {
    /// Conditions in the analysed decision (`M`). Not-measurable
    /// candidates are never part of it.
    pub conditions_total: usize,
    /// Conditions with an observed independence pair.
    pub exercised: usize,
    /// Conditions with no observed independence pair and no
    /// ambiguous evidence.
    pub not_exercised: usize,
    /// Conditions whose only evidence came from ambiguous
    /// observations: neither demonstrated nor shown to be missing.
    pub unknown: usize,
    /// Candidates whose execution coverage cannot measure. Counted
    /// separately; never part of `conditions_total`.
    pub not_measurable: usize,
    /// `exercised / conditions_total * 100`, or `null` whenever
    /// `unknown > 0` or there are no conditions.
    pub percentage: Option<f64>,
}

impl CoverageSummary {
    /// Summarise `analysis` (`None` on the decline paths, where no
    /// decision was analysed). A condition with no computed status
    /// has no valid independence pair and counts as not exercised,
    /// as the JUnit report already treats it.
    pub fn new(analysis: Option<&RuntimeAnalysis>, not_measurable: usize) -> Self {
        let (mut exercised, mut not_exercised, mut unknown) = (0, 0, 0);
        let conditions: &[String] = analysis.map_or(&[], |a| &a.matrix.conditions);
        for cond in conditions {
            match analysis.and_then(|a| a.condition_status.get(cond)) {
                Some(ConditionStatus::Exercised(_)) => exercised += 1,
                Some(ConditionStatus::Ambiguous { .. }) => unknown += 1,
                Some(ConditionStatus::Unexercised) | None => not_exercised += 1,
            }
        }
        let total = conditions.len();
        let percentage =
            (total > 0 && unknown == 0).then(|| exercised as f64 / total as f64 * 100.0);
        CoverageSummary {
            conditions_total: total,
            exercised,
            not_exercised,
            unknown,
            not_measurable,
            percentage,
        }
    }

    /// The `MC/DC coverage: ...` line of the text report, or `None`
    /// when there are no conditions. `ambiguous_observations` is
    /// [`RuntimeAnalysis::ambiguous_observations`].
    pub fn coverage_line(&self, ambiguous_observations: usize) -> Option<String> {
        let (e, t, u) = (self.exercised, self.conditions_total, self.unknown);
        if t == 0 {
            return None;
        }
        let Some(pct) = self.percentage else {
            let noun = if u == 1 { "condition" } else { "conditions" };
            return Some(format!(
                "MC/DC coverage: {e} exercised, {} not exercised, {u} unknown of {t} \
                 conditions (no percentage: {u} {noun} could not be determined)",
                self.not_exercised
            ));
        };
        Some(if ambiguous_observations == 0 {
            format!("MC/DC coverage: {e} of {t} conditions exercised ({pct:.0}%)")
        } else {
            format!(
                "MC/DC coverage: {e} of {t} conditions exercised ({pct:.0}%; \
                 {u} unknown, {ambiguous_observations} ambiguous observation(s) excluded; add \
                 single-evaluation tests or see the README section \
                 \"Tests that evaluate a decision more than once\")"
            )
        })
    }

    /// True when every condition was exercised: the run demonstrated
    /// MC/DC for the analysed decision.
    pub fn demonstrated(&self) -> bool {
        self.conditions_total > 0 && self.exercised == self.conditions_total
    }

    /// Exit code for an analysed run: 0 when MC/DC was demonstrated,
    /// [`EXIT_MCDC_NOT_DEMONSTRATED`] when any condition is not
    /// exercised or unknown.
    pub fn exit_code(&self) -> u8 {
        if self.demonstrated() {
            0
        } else {
            EXIT_MCDC_NOT_DEMONSTRATED
        }
    }
}

/// The hint printed under the coverage line when a percentage was
/// withheld: how many observations were excluded and how to resolve it.
pub fn unknown_hint(ambiguous_observations: usize) -> String {
    format!(
        "  {ambiguous_observations} ambiguous observation(s) excluded; unknown conditions \
         block a pass. Add tests that each evaluate the decision once, or see the README \
         section \"Tests that evaluate a decision more than once\"."
    )
}

/// The `Not measurable: ...` line of the text report, or `None` when
/// no candidate is not measurable. Reasons are listed with their
/// counts in a fixed order.
pub fn not_measurable_line<'a>(
    items: impl IntoIterator<Item = &'a NotMeasurable>,
) -> Option<String> {
    let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
    let mut total = 0usize;
    for nm in items {
        total += 1;
        *counts.entry(reason_label(&nm.reason)).or_default() += 1;
    }
    if total == 0 {
        return None;
    }
    let reasons: Vec<String> = counts.iter().map(|(r, n)| format!("{r}: {n}")).collect();
    Some(format!(
        "Not measurable: {total} candidate(s), not counted in the conditions above \
         ({}); coverage cannot tell whether they ran. See `not_measurable` in \
         --format=json's `candidates` list.",
        reasons.join(", ")
    ))
}

/// The JSON spelling of a [`NotMeasurableReason`].
pub fn reason_label(reason: &NotMeasurableReason) -> &'static str {
    match reason {
        NotMeasurableReason::Uninstrumented => "uninstrumented",
        NotMeasurableReason::ExternalMacro => "external_macro",
        NotMeasurableReason::NoCoverageRecord => "no_coverage_record",
        NotMeasurableReason::AmbiguousSpans => "ambiguous_spans",
        NotMeasurableReason::NoBranchRegion => "no_branch_region",
    }
}

/// Join the first [`TESTS_SHOWN`] of `tests` (each rendered by
/// `label`) and summarise the rest as `(+N more)`.
pub fn cap_tests(tests: &[String], label: impl Fn(&str) -> String) -> String {
    let shown: Vec<String> = tests.iter().take(TESTS_SHOWN).map(|t| label(t)).collect();
    let mut out = shown.join(", ");
    if tests.len() > TESTS_SHOWN {
        out.push_str(&format!(" (+{} more)", tests.len() - TESTS_SHOWN));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use floyd::masking::{IndependenceMatrix, IndependencePair, TruthTableRow};
    use std::collections::BTreeMap;

    fn row(result: bool) -> TruthTableRow {
        TruthTableRow {
            inputs: BTreeMap::new(),
            result,
        }
    }

    fn analysis(statuses: &[(&str, ConditionStatus)], ambiguous: usize) -> RuntimeAnalysis {
        RuntimeAnalysis {
            matrix: IndependenceMatrix {
                conditions: statuses.iter().map(|(c, _)| c.to_string()).collect(),
                ..Default::default()
            },
            condition_status: statuses
                .iter()
                .map(|(c, s)| (c.to_string(), s.clone()))
                .collect(),
            provenance: None,
            ambiguous_observations: ambiguous,
        }
    }

    fn exercised() -> ConditionStatus {
        ConditionStatus::Exercised(IndependencePair {
            test_1: row(false),
            test_2: row(true),
        })
    }

    fn ambiguous(n: usize) -> ConditionStatus {
        ConditionStatus::Ambiguous {
            tests: (0..n).map(|i| format!("t{i}")).collect(),
        }
    }

    #[test]
    fn no_unknowns_keeps_the_percentage_wording() {
        let a = analysis(
            &[
                ("a", exercised()),
                ("b", exercised()),
                ("c", ConditionStatus::Unexercised),
            ],
            0,
        );
        let s = CoverageSummary::new(Some(&a), 0);
        assert_eq!(
            s.coverage_line(0).unwrap(),
            "MC/DC coverage: 2 of 3 conditions exercised (67%)"
        );
        assert_eq!(s.exit_code(), EXIT_MCDC_NOT_DEMONSTRATED);

        let full = analysis(&[("a", exercised())], 0);
        let s = CoverageSummary::new(Some(&full), 0);
        assert_eq!(
            s.coverage_line(0).unwrap(),
            "MC/DC coverage: 1 of 1 conditions exercised (100%)"
        );
        assert_eq!(s.percentage, Some(100.0));
        assert_eq!(s.exit_code(), 0);
    }

    #[test]
    fn ambiguous_observations_without_unknowns_keep_the_current_wording() {
        let a = analysis(&[("a", exercised()), ("b", exercised())], 4);
        let s = CoverageSummary::new(Some(&a), 0);
        assert_eq!(
            s.coverage_line(4).unwrap(),
            "MC/DC coverage: 2 of 2 conditions exercised (100%; 0 unknown, 4 ambiguous \
             observation(s) excluded; add single-evaluation tests or see the README section \
             \"Tests that evaluate a decision more than once\")"
        );
    }

    #[test]
    fn unknowns_withhold_the_percentage() {
        let a = analysis(
            &[("byte <= 126", ambiguous(44)), ("byte >= 32", exercised())],
            44,
        );
        let s = CoverageSummary::new(Some(&a), 0);
        assert_eq!(s.percentage, None);
        assert_eq!(
            s.coverage_line(44).unwrap(),
            "MC/DC coverage: 1 exercised, 0 not exercised, 1 unknown of 2 conditions \
             (no percentage: 1 condition could not be determined)"
        );
        let a = analysis(
            &[
                ("a", ambiguous(1)),
                ("b", ambiguous(2)),
                ("c", ConditionStatus::Unexercised),
            ],
            2,
        );
        let line = CoverageSummary::new(Some(&a), 0).coverage_line(2).unwrap();
        assert_eq!(
            line,
            "MC/DC coverage: 0 exercised, 1 not exercised, 2 unknown of 3 conditions \
             (no percentage: 2 conditions could not be determined)"
        );
        assert!(!line.contains('%'));
    }

    #[test]
    fn an_unknown_condition_fails_the_run_like_an_unexercised_one() {
        let unknown = analysis(&[("a", exercised()), ("b", ambiguous(1))], 1);
        let unexercised = analysis(
            &[("a", exercised()), ("b", ConditionStatus::Unexercised)],
            0,
        );
        let u = CoverageSummary::new(Some(&unknown), 0);
        let x = CoverageSummary::new(Some(&unexercised), 0);
        assert!(!u.demonstrated());
        assert_eq!(u.exit_code(), EXIT_MCDC_NOT_DEMONSTRATED);
        assert_eq!(u.exit_code(), x.exit_code());
        assert_ne!(EXIT_MCDC_NOT_DEMONSTRATED, 0);
        assert_ne!(EXIT_MCDC_NOT_DEMONSTRATED, 1);
    }

    #[test]
    fn json_summary_has_null_percentage_when_unknown() {
        let a = analysis(&[("a", exercised()), ("b", ambiguous(3))], 3);
        let v = serde_json::to_value(CoverageSummary::new(Some(&a), 5)).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "conditions_total": 2,
                "exercised": 1,
                "not_exercised": 0,
                "unknown": 1,
                "not_measurable": 5,
                "percentage": null,
            })
        );
        let a = analysis(
            &[("a", exercised()), ("b", ConditionStatus::Unexercised)],
            0,
        );
        let v = serde_json::to_value(CoverageSummary::new(Some(&a), 0)).unwrap();
        assert_eq!(v["percentage"], 50.0);
        assert_eq!(v["not_exercised"], 1);
        let v = serde_json::to_value(CoverageSummary::new(None, 2)).unwrap();
        assert_eq!(v["conditions_total"], 0);
        assert_eq!(v["not_measurable"], 2);
        assert!(v["percentage"].is_null());
    }

    #[test]
    fn not_measurable_line_counts_reasons_and_is_absent_when_none() {
        assert_eq!(not_measurable_line(&[]), None);
        let nm = |reason| NotMeasurable {
            reason,
            detail: String::new(),
        };
        let items = [
            nm(NotMeasurableReason::Uninstrumented),
            nm(NotMeasurableReason::AmbiguousSpans),
            nm(NotMeasurableReason::Uninstrumented),
        ];
        assert_eq!(
            not_measurable_line(&items).unwrap(),
            "Not measurable: 3 candidate(s), not counted in the conditions above \
             (ambiguous_spans: 1, uninstrumented: 2); coverage cannot tell whether they \
             ran. See `not_measurable` in --format=json's `candidates` list."
        );
    }

    #[test]
    fn reason_labels_match_the_json_spelling() {
        for r in [
            NotMeasurableReason::Uninstrumented,
            NotMeasurableReason::ExternalMacro,
            NotMeasurableReason::NoCoverageRecord,
            NotMeasurableReason::AmbiguousSpans,
            NotMeasurableReason::NoBranchRegion,
        ] {
            assert_eq!(serde_json::to_value(r).unwrap(), reason_label(&r));
        }
    }

    #[test]
    fn cap_tests_names_three_then_counts_the_rest() {
        let many: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        assert_eq!(cap_tests(&many, str::to_string), "t0, t1, t2 (+2 more)");
        assert_eq!(cap_tests(&many[..3], str::to_string), "t0, t1, t2");
    }
}
