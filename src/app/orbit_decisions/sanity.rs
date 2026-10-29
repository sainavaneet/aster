//! Report sanity: does a worker report answer its assignment? A flag only adds
//! a line to the delivered report and `sanity` to the task; it never blocks or
//! delays delivery beyond the call itself, and an error flags nothing.
use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::approval::TASK_PROMPT_CHARS;
use super::journal::{bounded, Entry, MAX_SUMMARY_BYTES};
use super::provider::{DecisionAnswers, DecisionProvider, DecisionRequest, Question};
use crate::config::{DecisionReportSanityConfig, PolarisDecisionsConfig};

const ANSWER_LEVELS: [&str; 5] = [
    "Does not answer",
    "Barely answers",
    "Partially answers",
    "Mostly answers",
    "Fully answers with specifics",
];

#[derive(Debug, Clone)]
pub(crate) struct SanityInput {
    pub(crate) subject: String,
    pub(crate) assignment: String,
    /// The report fields, with secrets already redacted.
    pub(crate) report: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SanityVerdict {
    pub(crate) subject: String,
    /// `{score, followup, flagged}`, or `None` when the check failed.
    pub(crate) sanity: Option<Value>,
    /// `Decision: report may be incomplete …` when flagged.
    pub(crate) line: Option<String>,
}

pub(crate) fn request(input: &SanityInput) -> DecisionRequest {
    let assignment: String = input.assignment.chars().take(TASK_PROMPT_CHARS).collect();
    DecisionRequest {
        state: json!({"assignment": assignment, "report": input.report}),
        questions: BTreeMap::from([
            (
                "answers_assignment".to_owned(),
                Question::score(
                    "How well does the report answer the assignment?",
                    &ANSWER_LEVELS,
                ),
            ),
            (
                "needs_followup".to_owned(),
                Question::noul(
                    "Does the lead need to follow up with the worker before accepting this report?",
                ),
            ),
        ]),
    }
}

/// `(score, followup, flagged)`.
pub(crate) fn judge(
    answers: &DecisionAnswers,
    config: &DecisionReportSanityConfig,
) -> (f64, f64, bool) {
    let score = answers.score_mean("answers_assignment").unwrap_or(0.0);
    let followup = answers.noul("needs_followup").unwrap_or(1.0);
    let flagged = followup >= config.followup_min || score < config.score_min;
    (score, followup, flagged)
}

pub(crate) fn decide(
    input: SanityInput,
    config: &PolarisDecisionsConfig,
    provider: &dyn DecisionProvider,
    now_ms: u64,
) -> (SanityVerdict, Entry) {
    let started = std::time::Instant::now();
    let sanity = &config.report_sanity;
    let mut entry = Entry {
        ts: now_ms,
        kind: "report_sanity".into(),
        provider: provider.name().into(),
        subject: Some(input.subject.clone()),
        inputs_summary: bounded(
            &format!("assignment: {}; report: {}", input.assignment, input.report),
            MAX_SUMMARY_BYTES,
        ),
        answers: None,
        thresholds: json!({"followup_min": sanity.followup_min, "score_min": sanity.score_min}),
        action: "none".into(),
        latency_ms: 0,
        error: None,
    };
    let mut verdict = SanityVerdict {
        subject: input.subject.clone(),
        sanity: None,
        line: None,
    };
    match provider.evaluate(request(&input)) {
        Ok(answers) => {
            let (score, followup, flagged) = judge(&answers, sanity);
            entry.answers = serde_json::to_value(&answers).ok();
            entry.action = if flagged { "flag" } else { "ok" }.into();
            verdict.sanity = Some(json!({
                "score": score,
                "followup": followup,
                "flagged": flagged,
                "provider": provider.name(),
            }));
            verdict.line = flagged.then(|| {
                format!("Decision: report may be incomplete (score {score:.1}, follow-up {followup:.2})")
            });
        }
        Err(error) => entry.error = Some(error.to_string()),
    }
    entry.latency_ms = started.elapsed().as_millis() as u64;
    (verdict, entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::orbit_decisions::provider::{DecisionError, Scripted};

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/decisions/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn input() -> SanityInput {
        SanityInput {
            subject: "task-1".into(),
            assignment: "Review commit 2713feb5 for bugs and run the tests".into(),
            report: json!({"outcome":"completed","summary":"Tests not run.","tests_run":false}),
        }
    }

    #[test]
    fn incomplete_report_is_flagged_and_a_thorough_one_is_not() {
        let config = PolarisDecisionsConfig::default();
        let provider = Scripted::fixture(&fixture("report_mid"));
        let (verdict, entry) = decide(input(), &config, &provider, 1);
        assert_eq!(
            verdict.line.as_deref(),
            Some("Decision: report may be incomplete (score 2.1, follow-up 0.89)")
        );
        assert_eq!(verdict.sanity.unwrap()["flagged"], true);
        assert_eq!(entry.action, "flag");
        let provider = Scripted::fixture(&fixture("report_good"));
        let (verdict, entry) = decide(input(), &config, &provider, 1);
        assert_eq!(verdict.line, None);
        assert_eq!(verdict.sanity.unwrap()["flagged"], false);
        assert_eq!(entry.action, "ok");
        let provider = Scripted::fixture(&fixture("report_junk"));
        assert!(decide(input(), &config, &provider, 1).0.line.is_some());
    }

    #[test]
    fn errors_flag_nothing() {
        let provider = Scripted::new(|_| Err(DecisionError::Timeout));
        let (verdict, entry) = decide(input(), &PolarisDecisionsConfig::default(), &provider, 1);
        assert_eq!(verdict.line, None);
        assert_eq!(verdict.sanity, None);
        assert_eq!(entry.error.as_deref(), Some("timed out"));
    }
}
