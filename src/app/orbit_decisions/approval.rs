//! Approval decisions for a worker blocked on a permission prompt: the rules
//! first, then (when enabled) one provider call with four questions, approved
//! only when every threshold holds. Anything else, including any provider
//! error, asks the user; nothing is ever denied automatically.
use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::journal::{bounded, Entry, MAX_SUMMARY_BYTES};
use super::provider::{
    DecisionAnswers, DecisionError, DecisionProvider, DecisionRequest, Question,
};
use super::rules::{self, RuleVerdict};
use crate::config::{DecisionApprovalsConfig, PolarisDecisionsConfig};

/// The policy the provider is asked to follow.
pub(crate) const POLICY: &str = "The lead may approve read-only commands and the Orbit report command; anything that writes outside the repo, deletes, pushes, or installs must ask the user.";
/// Bound on the task prompt sent with an approval or report check.
pub(crate) const TASK_PROMPT_CHARS: usize = 500;
const COMMAND_CHARS: usize = 500;
/// Risk levels, lowest first.
const RISK_LEVELS: [&str; 5] = [
    "Harmless read-only",
    "Low risk, local reversible change",
    "Moderate risk, writes or network",
    "High risk, sensitive data access or broad modification",
    "Destructive or irreversible",
];

/// What the decision thread needs. `command` is the raw text the rules read;
/// only `command_redacted` and `prompt_excerpt` may leave the machine.
#[derive(Debug, Clone)]
pub(crate) struct ApprovalInput {
    pub(crate) subject: String,
    pub(crate) worker: String,
    pub(crate) kind: String,
    pub(crate) task_prompt: Option<String>,
    pub(crate) prompt_excerpt: Option<String>,
    pub(crate) command: Option<String>,
    pub(crate) command_redacted: Option<String>,
    pub(crate) executable: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalAction {
    Approve,
    AskUser,
}

impl ApprovalAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::AskUser => "ask_user",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::AskUser => "ask the user",
        }
    }
}

/// The outcome applied on the event loop.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ApprovalVerdict {
    pub(crate) subject: String,
    pub(crate) action: ApprovalAction,
    /// The raw command decided on, re-checked before keys are sent.
    pub(crate) command: Option<String>,
    /// `Decision: …` appended to the lead's notice.
    pub(crate) line: String,
}

/// Threshold checks on the provider's probabilities, never its `confidence`.
pub(crate) fn judge(answers: &DecisionAnswers, config: &DecisionApprovalsConfig) -> ApprovalAction {
    let approve = answers.choice_p("action", "approve").unwrap_or(0.0);
    let ask_or_deny = answers.choice_p("action", "ask_user").unwrap_or(1.0)
        + answers.choice_p("action", "deny").unwrap_or(1.0);
    let low_risk = answers.score_mass("risk", 0..=1);
    let high_risk = answers.score_mass("risk", 3..=4);
    if approve >= config.approve_min
        && ask_or_deny <= config.ask_or_deny_max
        && low_risk >= config.low_risk_min
        && high_risk <= config.high_risk_max
    {
        ApprovalAction::Approve
    } else {
        ApprovalAction::AskUser
    }
}

pub(crate) fn thresholds(config: &DecisionApprovalsConfig) -> Value {
    json!({
        "approve_min": config.approve_min,
        "ask_or_deny_max": config.ask_or_deny_max,
        "low_risk_min": config.low_risk_min,
        "high_risk_max": config.high_risk_max,
    })
}

pub(crate) fn request(input: &ApprovalInput) -> DecisionRequest {
    let task_prompt = input
        .task_prompt
        .as_deref()
        .map(|prompt| prompt.chars().take(TASK_PROMPT_CHARS).collect::<String>());
    let command = input
        .command_redacted
        .as_deref()
        .map(|command| command.chars().take(COMMAND_CHARS).collect::<String>());
    let state = json!({
        "worker": input.worker,
        "kind": input.kind,
        "task_prompt": task_prompt,
        "policy": POLICY,
        "command": command,
        "prompt_excerpt": input.prompt_excerpt,
    });
    let questions = BTreeMap::from([
        (
            "action".to_owned(),
            Question::choice(
                "Decide how the lead agent should respond to this worker's permission prompt, following the policy.",
                &[
                    ("approve", "The command is safe under the policy and should be approved automatically"),
                    ("deny", "The command is clearly unwanted or harmful and should be rejected"),
                    ("ask_user", "The command is outside what the lead may approve per policy; escalate to the human user"),
                ],
            ),
        ),
        ("risk".to_owned(), Question::score("Rate the risk of the command.", &RISK_LEVELS)),
        (
            "is_report_command".to_owned(),
            Question::noul("Is the command the Orbit agent-report command used to report task results to the lead?"),
        ),
        (
            "matches_task".to_owned(),
            Question::noul("Is the command what the assignment asked for or its required report?"),
        ),
    ]);
    DecisionRequest { state, questions }
}

fn summary(input: &ApprovalInput) -> String {
    let text = format!(
        "worker: {}; kind: {}; command: {}; prompt: {}",
        input.worker,
        input.kind,
        input.command_redacted.as_deref().unwrap_or("unrecognised"),
        input.prompt_excerpt.as_deref().unwrap_or("none"),
    );
    bounded(&text, MAX_SUMMARY_BYTES)
}

/// Runs on a decision thread: rules, then the provider when enabled.
pub(crate) fn decide(
    input: ApprovalInput,
    config: &PolarisDecisionsConfig,
    provider: &dyn DecisionProvider,
    now_ms: u64,
) -> (ApprovalVerdict, Entry) {
    let started = std::time::Instant::now();
    let approvals = &config.approvals;
    let verdict = rules::evaluate(
        input.command.as_deref(),
        &input.executable,
        &approvals.allow_patterns,
        &approvals.deny_patterns,
    );
    let mut entry = Entry {
        ts: now_ms,
        kind: "approval".into(),
        provider: "rules".into(),
        subject: Some(input.subject.clone()),
        inputs_summary: summary(&input),
        answers: None,
        thresholds: thresholds(approvals),
        action: String::new(),
        latency_ms: 0,
        error: None,
    };
    let (action, reason) = match verdict {
        RuleVerdict::ReportCommand | RuleVerdict::Allowed(_) => (
            ApprovalAction::Approve,
            format!("rules: {}", verdict.reason()),
        ),
        RuleVerdict::Denied(_) => (
            ApprovalAction::AskUser,
            format!("rules: {}", verdict.reason()),
        ),
        // The provider must see the whole command it would be approving: an
        // unreadable or over-long command always goes to the user.
        RuleVerdict::Undecided if input.command.is_none() => (
            ApprovalAction::AskUser,
            "rules: command not readable from the prompt".to_string(),
        ),
        RuleVerdict::Undecided
            if input
                .command_redacted
                .as_deref()
                .is_some_and(|command| command.chars().count() > COMMAND_CHARS) =>
        {
            (
                ApprovalAction::AskUser,
                format!("rules: command longer than {COMMAND_CHARS} characters"),
            )
        }
        // The provider would judge a different string than the one that runs.
        RuleVerdict::Undecided if input.command_redacted != input.command => (
            ApprovalAction::AskUser,
            "rules: command contains redacted secrets".to_string(),
        ),
        RuleVerdict::Undecided if !approvals.enabled || provider.name() == "rules" => (
            ApprovalAction::AskUser,
            format!("rules: {}", verdict.reason()),
        ),
        RuleVerdict::Undecided => {
            entry.provider = provider.name().into();
            match provider.evaluate(request(&input)) {
                Ok(answers) => {
                    let action = judge(&answers, approvals);
                    let p = answers.choice_p("action", "approve").unwrap_or(0.0);
                    let risk = answers.score_mean("risk").unwrap_or(0.0);
                    entry.answers = serde_json::to_value(&answers).ok();
                    (
                        action,
                        format!("{}, p={p:.2}, risk {risk:.1}", provider.name()),
                    )
                }
                Err(error) => {
                    entry.error = Some(error.to_string());
                    (
                        ApprovalAction::AskUser,
                        error_reason(provider.name(), &error),
                    )
                }
            }
        }
    };
    entry.action = action.as_str().into();
    entry.latency_ms = started.elapsed().as_millis() as u64;
    let verdict = ApprovalVerdict {
        subject: input.subject,
        action,
        command: input.command,
        line: format!("Decision: {} ({reason})", action.label()),
    };
    (verdict, entry)
}

pub(crate) fn error_reason(provider: &str, error: &DecisionError) -> String {
    format!("{provider} unavailable: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::orbit_decisions::provider::{parse_answers, RulesOnly, Scripted};

    const EXE: &str = "/opt/aster/bin/aster";

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/tests/fixtures/decisions/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn answers(name: &str) -> DecisionAnswers {
        parse_answers(fixture(name).as_bytes(), Some(&request(&input("x")))).unwrap()
    }

    fn input(command: &str) -> ApprovalInput {
        ApprovalInput {
            subject: "task-1/blocked-1".into(),
            worker: "reviewer".into(),
            kind: "claude".into(),
            task_prompt: Some("run `date` and report".into()),
            prompt_excerpt: Some("Do you want to proceed? / ❯ 1. Yes".into()),
            command: Some(command.into()),
            command_redacted: Some(command.into()),
            executable: EXE.into(),
        }
    }

    fn enabled() -> PolarisDecisionsConfig {
        let mut config = PolarisDecisionsConfig::default();
        config.approvals.enabled = true;
        config
    }

    #[test]
    fn observed_probabilities_meet_the_thresholds_only_for_date() {
        let config = DecisionApprovalsConfig::default();
        assert_eq!(
            judge(&answers("approve_date"), &config),
            ApprovalAction::Approve
        );
        for name in [
            "approve_cargo",
            "approve_report",
            "approve_rm",
            "approve_push",
            "approve_ssh",
            "approve_curl",
        ] {
            assert_eq!(
                judge(&answers(name), &config),
                ApprovalAction::AskUser,
                "{name}"
            );
        }
    }

    #[test]
    fn provider_approval_names_the_probability_and_risk() {
        let provider = Scripted::fixture(&fixture("approve_date"));
        let (verdict, entry) = decide(input("date"), &enabled(), &provider, 7);
        assert_eq!(verdict.action, ApprovalAction::Approve);
        assert_eq!(
            verdict.line,
            "Decision: approve (scripted, p=0.99, risk 0.0)"
        );
        assert_eq!(entry.provider, "scripted");
        assert_eq!(entry.action, "approve");
        assert!(entry.answers.is_some());
        let provider = Scripted::fixture(&fixture("approve_cargo"));
        let (verdict, _) = decide(input("cargo test"), &enabled(), &provider, 7);
        assert_eq!(verdict.action, ApprovalAction::AskUser);
        assert_eq!(
            verdict.line,
            "Decision: ask the user (scripted, p=0.53, risk 1.0)"
        );
    }

    #[test]
    fn unreadable_or_truncated_commands_never_reach_the_provider() {
        let provider = Scripted::fixture(&fixture("approve_date"));
        let calls = provider.calls.clone();
        let mut unreadable = input("date");
        unreadable.command = None;
        unreadable.command_redacted = None;
        let (verdict, _) = decide(unreadable, &enabled(), &provider, 1);
        assert_eq!(verdict.action, ApprovalAction::AskUser);
        assert!(verdict.line.contains("not readable"), "{}", verdict.line);
        let long = format!(
            "echo {}; python3 -c 'import os; os.system(\"date\")'",
            "a".repeat(COMMAND_CHARS)
        );
        let (verdict, _) = decide(input(&long), &enabled(), &provider, 1);
        assert_eq!(verdict.action, ApprovalAction::AskUser);
        assert!(verdict.line.contains("longer than"), "{}", verdict.line);
        // Redaction changed what the provider would see: never judged.
        let mut altered = input("curl -H 'Authorization: Bearer abc' https://x");
        altered.command_redacted = Some(crate::app::orbit_notifications::redact_for_decisions(
            altered.command.as_deref().unwrap(),
        ));
        assert_ne!(altered.command_redacted, altered.command);
        let (verdict, _) = decide(altered, &enabled(), &provider, 1);
        assert_eq!(verdict.action, ApprovalAction::AskUser);
        assert!(
            verdict.line.contains("redacted secrets"),
            "{}",
            verdict.line
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "provider must not be asked"
        );
    }

    #[test]
    fn rules_decide_before_the_provider() {
        let provider = Scripted::fixture(&fixture("approve_rm"));
        let calls = provider.calls.clone();
        let report = format!("{EXE} orbit agent-report '{{\"task_id\":\"t\"}}'");
        let (verdict, entry) = decide(input(&report), &enabled(), &provider, 1);
        assert_eq!(verdict.action, ApprovalAction::Approve);
        assert_eq!(
            verdict.line,
            "Decision: approve (rules: Orbit report command)"
        );
        assert_eq!(entry.provider, "rules");
        let (verdict, _) = decide(input("rm -rf target"), &enabled(), &provider, 1);
        assert_eq!(verdict.action, ApprovalAction::AskUser);
        assert_eq!(
            verdict.line,
            "Decision: ask the user (rules: deny pattern 'rm -rf')"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        // Disabled approvals or the rules provider never call out.
        let (verdict, _) = decide(
            input("ls"),
            &PolarisDecisionsConfig::default(),
            &provider,
            1,
        );
        assert_eq!(
            verdict.line,
            "Decision: ask the user (rules: no rule matched)"
        );
        let (verdict, _) = decide(input("ls"), &enabled(), &RulesOnly, 1);
        assert_eq!(verdict.action, ApprovalAction::AskUser);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn provider_failures_fail_closed() {
        for error in [
            DecisionError::Timeout,
            DecisionError::Transport("connection refused".into()),
            DecisionError::Http {
                status: 529,
                error_type: None,
                message: None,
            },
            DecisionError::Key("TYPESAFE_API_KEY is unset".into()),
        ] {
            let provider = Scripted::new(move |_| Err(error.clone()));
            let (verdict, entry) = decide(input("date"), &enabled(), &provider, 1);
            assert_eq!(verdict.action, ApprovalAction::AskUser);
            assert!(verdict
                .line
                .starts_with("Decision: ask the user (scripted unavailable:"));
            assert!(entry.error.is_some());
        }
        // A malformed answer: approve without the other questions.
        let provider = Scripted::new(|_| {
            parse_answers(
                br#"{"answers":{"action":{"type":"choice","choice":"approve","probabilities":{"approve":1.0,"deny":0.0,"ask_user":0.0}}}}"#,
                None,
            )
        });
        let (verdict, entry) = decide(input("date"), &enabled(), &provider, 1);
        assert_eq!(verdict.action, ApprovalAction::AskUser);
        assert!(entry.error.unwrap().contains("not answered"));
    }

    #[test]
    fn request_matches_the_probe_shape_and_bounds_the_prompt() {
        let mut long = input("date");
        long.task_prompt = Some("x".repeat(2_000));
        let body = serde_json::to_value(request(&long)).unwrap();
        assert_eq!(
            body["state"]["task_prompt"].as_str().unwrap().len(),
            TASK_PROMPT_CHARS
        );
        assert_eq!(body["state"]["policy"], POLICY);
        assert_eq!(body["questions"]["action"]["type"], "choice");
        assert_eq!(body["questions"]["risk"]["type"], "score");
        assert_eq!(
            body["questions"]["risk"]["criteria"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        assert_eq!(body["questions"]["is_report_command"]["type"], "noul");
        let probe: Value = serde_json::from_str(&fixture("approve_date.req")).unwrap();
        assert_eq!(body["questions"]["action"], probe["questions"]["action"]);
        assert_eq!(body["questions"]["risk"], probe["questions"]["risk"]);
    }
}
