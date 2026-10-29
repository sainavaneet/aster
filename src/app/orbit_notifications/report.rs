//! `orbit.agent_report`: scope check, summary and facts validation, then token-checked
//! admission into the ledger. Only refused identity or token checks are rate
//! limited, so reports spoofing a worker's ID cannot block its genuine report.
use std::time::Instant;

use serde_json::json;

use super::approvals::redact_for_decisions;
use super::ledger::REPORT_REJECTED;
use super::state::{DecisionMark, Delivery};
use super::{response_error, App, Notifications};
use crate::api::schema::OrbitAgentReportParams;
use crate::app::orbit_decisions::sanity::{self, SanityInput, SanityVerdict};
use crate::app::orbit_decisions::{wall_ms, Outcome};

/// Refused report calls (wrong task, worker, or token) answered per worker
/// terminal per sliding window; later refusals in the window are rate limited.
pub(super) const REPORT_RATE_LIMIT: usize = 30;
pub(super) const REPORT_RATE_WINDOW_MS: u64 = 60_000;
const MAX_SUMMARY_BYTES: usize = 4000;
/// Prefix of every delivery marker; a summary must never be able to forge one.
pub(super) const DELIVERY_MARKER_PREFIX: &str = "[orbit-delivery";
pub(super) const INVALID_SUMMARY: &str =
    "Summary must be 1–4000 bytes without terminal control characters or delivery markers";
pub(super) const OUT_OF_SCOPE: &str = "Reporting worker is not inside the bound Orbit tab";
pub(super) const RATE_LIMITED: &str = "too many reports from this worker; wait a minute";

fn valid_summary(summary: &str) -> bool {
    !summary.trim().is_empty()
        && summary.len() <= MAX_SUMMARY_BYTES
        && !summary
            .chars()
            .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
        && !summary.contains(DELIVERY_MARKER_PREFIX)
}

impl Notifications {
    /// Charges one refused call to the worker's sliding window; `false` once the
    /// window is full. Runtime-only. Only in-scope worker terminals are tracked,
    /// and idle windows are dropped, so the map stays bounded.
    pub(super) fn charge_refused_report(&mut self, worker_id: &str, now: u64) -> bool {
        let window_start = now.saturating_sub(REPORT_RATE_WINDOW_MS);
        self.report_calls.retain(|_, calls| {
            while calls.front().is_some_and(|at| *at <= window_start) {
                calls.pop_front();
            }
            !calls.is_empty()
        });
        let calls = self.report_calls.entry(worker_id.to_owned()).or_default();
        if calls.len() >= REPORT_RATE_LIMIT {
            return false;
        }
        calls.push_back(now);
        true
    }
}

impl App {
    pub(crate) fn handle_orbit_agent_report(
        &mut self,
        id: String,
        mut params: OrbitAgentReportParams,
    ) -> String {
        if !self.orbit_terminal_in_scope(&params.worker_id) {
            return response_error(id, OUT_OF_SCOPE);
        }
        let now = self.orbit_notifications.now_ms(Instant::now());
        if !valid_summary(&params.summary) {
            return response_error(id, INVALID_SUMMARY);
        }
        if let Err(error) = super::facts::validate(&params) {
            return response_error(id, error);
        }
        self.ensure_orbit_ledger(now);
        // Reap the assignment ACK before accepting a fast worker's report.
        self.orbit_notifications.reap_acks(now);
        let worker_id = params.worker_id.clone();
        let answered = match self.parallel_step_report(&params, now) {
            Some(result) => Some(result),
            None => self.relay_orbit_handoff(&id, &mut params, now),
        };
        let result = match answered {
            Some(result) => result,
            None => self.orbit_notifications.accept_report(params, now),
        };
        self.persist_orbit_ledger();
        match result {
            Ok(result) => json!({"id":id,"result":result}).to_string(),
            Err(error) if error == REPORT_REJECTED => {
                let notifications = &mut self.orbit_notifications;
                if notifications.charge_refused_report(&worker_id, now) {
                    response_error(id, error)
                } else {
                    response_error(id, RATE_LIMITED)
                }
            }
            Err(error) => response_error(id, error),
        }
    }
}

/// The report fields a sanity check sends, with secrets redacted.
fn sanity_report(report: &OrbitAgentReportParams) -> serde_json::Value {
    json!({
        "outcome": report.outcome,
        "summary": redact_for_decisions(&report.summary),
        "tests_run": report.tests_run,
        "checks": report.checks,
        "changed_files": report.changed_files.as_ref().map(Vec::len),
        "blocked_on": report.blocked_on.as_deref().map(redact_for_decisions),
    })
}

impl App {
    /// Starts a sanity check of worker report `index` against its assignment
    /// when enabled; whether one started. Never blocks delivery for longer than
    /// the call, and an unknown assignment prompt skips the check.
    pub(super) fn start_orbit_sanity_decision(&mut self, index: usize) -> bool {
        let decisions = &self.orbit_decisions;
        if !decisions.config().report_sanity.enabled || !decisions.has_provider() {
            return false;
        }
        let task = &self.orbit_notifications.tasks[index];
        let (Some(report), Some(prompt)) = (
            task.report.as_ref(),
            decisions.assignment_prompt(&task.task_id),
        ) else {
            return false;
        };
        let input = SanityInput {
            subject: task.task_id.clone(),
            assignment: redact_for_decisions(prompt),
            report: sanity_report(report),
        };
        let spawned = self.spawn_orbit_decision(move |config, provider| {
            let (verdict, entry) = sanity::decide(input, config, provider, wall_ms());
            (Outcome::Sanity(verdict), entry)
        });
        if let Err(error) = &spawned {
            tracing::warn!(%error, "cannot start an Orbit report check");
        }
        spawned.is_ok()
    }

    /// Records a report check; a flag adds its line to the delivered report.
    pub(crate) fn apply_orbit_sanity_decision(&mut self, verdict: SanityVerdict) {
        let Some(index) = self.orbit_notifications.find(&verdict.subject) else {
            return;
        };
        let task = &mut self.orbit_notifications.tasks[index];
        if task.delivery == Delivery::Queued
            && matches!(task.live.decision, DecisionMark::Pending { .. })
        {
            task.live.decision = DecisionMark::Done;
            task.live.decision_note = verdict.line;
        }
        task.live.sanity = verdict.sanity;
    }

    /// Report check results by task id, for `orbit.activity`.
    pub(crate) fn orbit_task_sanity(&self) -> Vec<(String, serde_json::Value)> {
        self.orbit_notifications
            .tasks
            .iter()
            .filter_map(|task| Some((task.task_id.clone(), task.live.sanity.clone()?)))
            .collect()
    }
}
