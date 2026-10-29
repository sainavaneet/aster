//! Answering a blocked worker's prompt for Polaris, opt-in through
//! `[polaris.approvals] lead_may_approve`. The user can always answer in the
//! worker's pane. The keys sent and the answer are never logged beyond the
//! worker terminal and task ids.
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde_json::json;

use super::state::{DecisionMark, Delivery, ReportOrigin, Task};
use super::App;
use crate::api::schema::OrbitOutcome;
use crate::app::orbit_decisions::approval::{self, ApprovalAction, ApprovalInput, ApprovalVerdict};
use crate::app::orbit_decisions::{rules, wall_ms, Outcome};
use crate::detect::{Agent, AgentState};

/// How long a request waits for the worker to leave `Blocked` after the keys.
pub(crate) const ANSWER_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Approve,
    Deny,
}

/// Keys that answer `agent`'s approval prompt, or `None` when no map is known.
///
/// Claude Code permission dialogs (claude.toml `bash_permission_prompt` and
/// `generic_permission_prompt`: "Do you want to proceed?" over
/// `❯ 1. Yes` … `3. No, and tell Claude what to do differently (esc)` with an
/// "Esc to cancel" footer) select an option by its digit, and Esc declines.
/// `1` is used instead of Enter so a cursor the user moved cannot turn an
/// approval into a denial, and no stray Enter reaches the composer.
///
/// Codex approval overlays (codex.toml `weak_blocker` "yes (y)" and
/// `live_strong_blocker` "press enter to confirm or esc to cancel") accept on
/// `y` without Enter, and Esc declines ("No, and tell Codex what to do
/// differently (esc)").
///
/// Gemini CLI tool dialogs (gemini.toml `apply_or_allow_change`: "Allow
/// execution of [Shell]?" / "Apply this change?" over `● 1. Allow once` …
/// `2. No, suggest changes (esc)`) run a selection list whose keypress handler
/// (useSelectionList in the 0.64 bundle) sets the active index on a digit and
/// confirms it at once when no further digit could follow, so `1` is "Allow
/// once" and Esc declines.
pub(crate) fn answer_keys(agent: Agent, decision: Decision) -> Option<&'static [&'static str]> {
    match (agent, decision) {
        (Agent::Claude | Agent::Gemini, Decision::Approve) => Some(&["1"]),
        (Agent::Codex, Decision::Approve) => Some(&["y"]),
        (Agent::Claude | Agent::Codex | Agent::Gemini, Decision::Deny) => Some(&["esc"]),
        _ => None,
    }
}

/// Detection rules of the permission prompts `answer_keys` answers. Any other
/// blocked dialog (plan mode, folder trust, a picker) is refused, because the
/// same keys would answer a question the lead was never asked.
pub(crate) fn permission_prompt_rules(agent: Agent) -> &'static [&'static str] {
    match agent {
        Agent::Claude => &["bash_permission_prompt", "generic_permission_prompt"],
        Agent::Codex => &["live_strong_blocker", "weak_blocker"],
        Agent::Gemini => &["apply_or_allow_change"],
        _ => &[],
    }
}

/// A request waiting for its worker to leave `Blocked`.
pub(crate) struct Waiter {
    pub(crate) id: String,
    pub(crate) respond_to: Sender<String>,
    pub(crate) terminal_id: String,
    pub(crate) deadline: Instant,
}

#[derive(Default)]
pub(super) struct Approvals {
    pub(super) lead_may_approve: bool,
    waiters: Vec<Waiter>,
}

fn state_name(state: Option<AgentState>) -> &'static str {
    match state {
        Some(AgentState::Idle) => "idle",
        Some(AgentState::Working) => "working",
        Some(AgentState::Blocked) => "blocked",
        Some(AgentState::Unknown) | None => "unknown",
    }
}

impl App {
    pub(crate) fn orbit_lead_may_approve(&self) -> bool {
        self.orbit_notifications.approvals.lead_may_approve
    }

    /// Answers once the worker leaves `Blocked` or `ANSWER_WAIT` passes.
    pub(crate) fn await_orbit_answer(&mut self, waiter: Waiter) {
        self.orbit_notifications.approvals.waiters.push(waiter);
        self.poll_orbit_answers(Instant::now());
    }

    /// Resolves finished waits; cheap when none are pending.
    pub(super) fn poll_orbit_answers(&mut self, now: Instant) {
        if self.orbit_notifications.approvals.waiters.is_empty() {
            return;
        }
        let waiters = std::mem::take(&mut self.orbit_notifications.approvals.waiters);
        for waiter in waiters {
            let state = self
                .state
                .terminals
                .get(waiter.terminal_id.as_str())
                .map(|terminal| terminal.state);
            let answered = state.is_some_and(|state| state != AgentState::Blocked);
            if state == Some(AgentState::Blocked) && now < waiter.deadline {
                self.orbit_notifications.approvals.waiters.push(waiter);
                continue;
            }
            let response = json!({
                "id": waiter.id,
                "result": {"answered": answered, "state": state_name(state)},
            });
            let _ = waiter.respond_to.send(response.to_string());
        }
    }
}

/// Secret redaction for text the decision layer may send or journal.
pub(crate) fn redact_for_decisions(text: &str) -> String {
    super::redact::redact_secrets(text)
}

/// A server notice that a worker is blocked on a prompt.
pub(super) fn is_blocked_notice(task: &Task) -> bool {
    task.origin == Some(ReportOrigin::Server)
        && task
            .report
            .as_ref()
            .is_some_and(|report| report.outcome == OrbitOutcome::Blocked)
}

impl App {
    /// Starts the approval decision for blocked notice `index` when the lead may
    /// approve and the worker is on a permission prompt; whether one started.
    pub(super) fn start_orbit_approval_decision(&mut self, index: usize) -> bool {
        if !self.orbit_lead_may_approve() {
            return false;
        }
        let task = &self.orbit_notifications.tasks[index];
        let subject = task.task_id.clone();
        let worker_id = task.worker_id.clone();
        let assignment = task.notice_for.clone();
        let Some(agent) = self.orbit_permission_prompt_agent(&worker_id) else {
            return false;
        };
        let Some(screen) = self.orbit_worker_detection_screen(&worker_id) else {
            return false;
        };
        let command = rules::command_text(&screen);
        let task_prompt = assignment
            .as_deref()
            .and_then(|id| self.orbit_decisions.assignment_prompt(id))
            .map(redact_for_decisions);
        let executable = std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let input = ApprovalInput {
            subject,
            worker: self.orbit_worker_label(&worker_id).name,
            kind: crate::detect::agent_label(agent).to_owned(),
            task_prompt,
            prompt_excerpt: super::lifecycle::prompt_excerpt(&screen),
            command_redacted: command.as_deref().map(redact_for_decisions),
            command,
            executable,
        };
        let spawned = self.spawn_orbit_decision(move |config, provider| {
            let (verdict, entry) = approval::decide(input, config, provider, wall_ms());
            (Outcome::Approval(verdict), entry)
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot start an Orbit approval decision");
            self.orbit_notifications.tasks[index].live.decision_note =
                Some("Decision: ask the user (decision layer unavailable)".into());
            return false;
        }
        true
    }

    /// Sends the approval keys for an `approve` verdict whose prompt is still
    /// showing, then lets the notice deliver with the verdict's line.
    pub(crate) fn apply_orbit_approval_decision(&mut self, verdict: ApprovalVerdict) {
        let Some(index) = self.orbit_notifications.find(&verdict.subject) else {
            return;
        };
        let task = &self.orbit_notifications.tasks[index];
        if !matches!(task.live.decision, DecisionMark::Pending { .. })
            || task.delivery != Delivery::Queued
        {
            return;
        }
        let worker_id = task.worker_id.clone();
        let line = match verdict.action {
            ApprovalAction::AskUser => verdict.line,
            ApprovalAction::Approve => {
                match self.auto_approve_orbit_worker(&worker_id, verdict.command.as_deref()) {
                    Ok(()) => verdict.line,
                    Err(reason) => format!("Decision: ask the user (approval not sent: {reason})"),
                }
            }
        };
        let live = &mut self.orbit_notifications.tasks[index].live;
        live.decision = DecisionMark::Done;
        live.decision_note = Some(line);
    }

    /// A held notice whose decision did not arrive in time is delivered as is.
    pub(super) fn expire_orbit_decision(&mut self, index: usize) {
        let task = &mut self.orbit_notifications.tasks[index];
        task.live.decision = DecisionMark::Done;
        if is_blocked_notice(task) {
            task.live.decision_note = Some("Decision: ask the user (decision timed out)".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_key_map_covers_claude_and_codex_only() {
        assert_eq!(
            answer_keys(Agent::Claude, Decision::Approve),
            Some(&["1"][..])
        );
        assert_eq!(
            answer_keys(Agent::Claude, Decision::Deny),
            Some(&["esc"][..])
        );
        assert_eq!(
            answer_keys(Agent::Codex, Decision::Approve),
            Some(&["y"][..])
        );
        assert_eq!(
            answer_keys(Agent::Codex, Decision::Deny),
            Some(&["esc"][..])
        );
        assert_eq!(
            answer_keys(Agent::Gemini, Decision::Approve),
            Some(&["1"][..])
        );
        assert_eq!(
            answer_keys(Agent::Gemini, Decision::Deny),
            Some(&["esc"][..])
        );
        assert_eq!(answer_keys(Agent::Pi, Decision::Deny), None);
    }

    #[test]
    fn permission_prompt_rules_exist_in_the_bundled_manifests() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Gemini] {
            let rules = permission_prompt_rules(agent);
            assert!(!rules.is_empty());
            let explain = crate::detect::manifest::explain(agent, "");
            for id in rules {
                let rule = explain
                    .evaluated_rules
                    .iter()
                    .find(|rule| rule.id == *id)
                    .unwrap_or_else(|| panic!("{agent:?} manifest has no rule {id}"));
                assert_eq!(rule.state, AgentState::Blocked, "{id}");
            }
        }
        assert!(permission_prompt_rules(Agent::Pi).is_empty());
    }
}
