//! Steps of one task relayed across the members of a role. The task stays a
//! single ledger entry that Polaris sees once; each step records which member
//! worked on it and the handoff note it left for the next.
use serde::{Deserialize, Serialize};

use super::state::{Ack, LiveState, Submission, Task};
use crate::api::schema::{OrbitHandoff, OrbitOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StepState {
    Pending,
    Active,
    Done,
    HandedOff,
    Failed,
}

/// One member's turn on a relayed task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Step {
    /// Zero-based position in the relay.
    pub(super) index: usize,
    pub(super) member: String,
    pub(super) kind: String,
    pub(super) state: StepState,
    #[serde(default)]
    pub(super) started_at: Option<u64>,
    #[serde(default)]
    pub(super) finished_at: Option<u64>,
    /// The note this step left for the next one.
    #[serde(default)]
    pub(super) handoff: Option<OrbitHandoff>,
    /// Set for a step run by its own agent in its own checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) parallel: Option<super::parallel::Parallel>,
    /// Set for the step that integrates the parallel steps' patches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) integrate: Option<super::parallel::Integrate>,
}

impl Step {
    pub(super) fn active(index: usize, member: &str, kind: &str, now: u64) -> Self {
        Self {
            index,
            member: member.into(),
            kind: kind.into(),
            state: StepState::Active,
            started_at: Some(now),
            finished_at: None,
            handoff: None,
            parallel: None,
            integrate: None,
        }
    }

    /// A step that has not started.
    pub(super) fn pending(index: usize, member: &str, kind: &str, now: u64) -> Self {
        Self {
            state: StepState::Pending,
            started_at: None,
            ..Self::active(index, member, kind, now)
        }
    }
}

/// The member a task is relayed to.
pub(super) struct Relay {
    pub(super) worker_id: String,
    pub(super) member: String,
    pub(super) kind: String,
}

impl Task {
    /// The step a member is working on now.
    pub(super) fn current_step(&self) -> Option<&Step> {
        self.steps
            .iter()
            .rev()
            .find(|step| step.state == StepState::Active)
    }

    /// Starts step 1 for `member` on a task that has none yet.
    pub(super) fn begin_steps(&mut self, member: &str, kind: &str) {
        if self.steps.is_empty() {
            self.steps
                .push(Step::active(0, member, kind, self.created_at));
        }
    }

    /// Ends the active step with the worker's report: done on completion,
    /// failed otherwise.
    pub(super) fn close_step(&mut self, now: u64) {
        let state = match self.report.as_ref().map(|report| &report.outcome) {
            Some(OrbitOutcome::Completed) => StepState::Done,
            _ => StepState::Failed,
        };
        self.end_step(state, now);
    }

    /// Ends the active step as failed: Aster resolved the task without a report.
    pub(super) fn fail_step(&mut self, now: u64) {
        for step in &mut self.steps {
            if matches!(step.state, StepState::Active | StepState::Pending) {
                step.state = StepState::Failed;
                step.finished_at = Some(now);
            }
        }
    }

    /// Ends the active step as handed off, keeping the note.
    pub(super) fn end_step_with_handoff(&mut self, handoff: OrbitHandoff, now: u64) {
        if let Some(step) = self.active_step_mut() {
            step.handoff = Some(handoff);
        }
        self.end_step(StepState::HandedOff, now);
    }

    fn active_step_mut(&mut self) -> Option<&mut Step> {
        self.steps
            .iter_mut()
            .rev()
            .find(|step| step.state == StepState::Active)
    }

    fn end_step(&mut self, state: StepState, now: u64) {
        if let Some(step) = self.active_step_mut() {
            step.state = state;
            step.finished_at = Some(now);
        }
    }

    /// Hands the task to the next member: the active step ends as handed off
    /// with `handoff`, and a new step starts for `next`, whose assignment
    /// prompt is acknowledged through `ack`. The task keeps its token and stays
    /// awaiting a report; its submission and report deadline restart.
    pub(super) fn relay_to(&mut self, handoff: OrbitHandoff, next: Relay, ack: Ack, now: u64) {
        self.end_step_with_handoff(handoff, now);
        let index = self.steps.len();
        self.steps
            .push(Step::active(index, &next.member, &next.kind, now));
        tracing::info!(
            task_id = %self.task_id,
            from = %self.worker_id,
            to = %next.worker_id,
            member = %next.member,
            "orbit task relayed to the next member"
        );
        // Each step gets the assignment's full timeout from its own start. A
        // task from before `timeout_ms` was stored has not been relayed yet, so
        // its deadline still marks the original timeout.
        let timeout = self
            .timeout_ms
            .or_else(|| self.deadline.map(|d| d.saturating_sub(self.created_at)));
        self.timeout_ms = timeout;
        self.worker_id = next.worker_id;
        self.submission = Submission::Pending;
        self.error = None;
        self.nudged_at = None;
        self.deadline = timeout.map(|timeout| now + timeout);
        self.updated_at = now;
        self.live = LiveState {
            submission_ack: Some(ack),
            ..LiveState::default()
        };
    }

    /// `step 2/3 reviewer-codex`, for a task with steps; `total` is the longest
    /// relay known (the role's pipeline, or the steps so far).
    pub(super) fn step_line(&self, total: usize) -> Option<String> {
        let step = self.current_step().or_else(|| {
            self.steps
                .iter()
                .rev()
                .find(|step| step.state != StepState::Pending)
        })?;
        Some(format!(
            "step {}/{} {}",
            step.index + 1,
            total.max(self.steps.len()),
            step.member
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relayed_task() -> Task {
        serde_json::from_value(serde_json::json!({
            "task_id": "t", "worker_id": "w0", "lead_id": "l",
            "submission": "submitted", "delivery": "awaiting_report", "error": null,
            "created_at": 1_000, "deadline": 61_000, "timeout_ms": 60_000,
            "steps": [{"index": 0, "member": "a", "kind": "claude", "state": "active"}]
        }))
        .unwrap()
    }

    fn handoff() -> OrbitHandoff {
        OrbitHandoff {
            done: "d".into(),
            next: "n".into(),
            changed_files: Vec::new(),
            known_issues: None,
            to: None,
            parallel: None,
        }
    }

    fn relay(task: &mut Task, member: &str, now: u64) {
        let next = Relay {
            worker_id: format!("w-{member}"),
            member: member.into(),
            kind: "claude".into(),
        };
        task.relay_to(handoff(), next, std::sync::mpsc::channel().1, now);
    }

    #[test]
    fn every_relayed_step_gets_the_full_timeout_from_its_own_start() {
        let mut task = relayed_task();
        relay(&mut task, "b", 31_000);
        assert_eq!(task.deadline, Some(91_000));
        relay(&mut task, "a", 61_000);
        assert_eq!(task.deadline, Some(121_000));
        assert_eq!(task.steps.len(), 3);
        // A task stored before the timeout was recorded derives it once.
        let mut legacy = relayed_task();
        legacy.timeout_ms = None;
        relay(&mut legacy, "b", 31_000);
        assert_eq!(legacy.deadline, Some(91_000));
        // ...and keeps it for later relays instead of deriving it again.
        assert_eq!(legacy.timeout_ms, Some(60_000));
        relay(&mut legacy, "a", 61_000);
        assert_eq!(legacy.deadline, Some(121_000));
    }

    #[test]
    fn steps_use_snake_case_wire_names_and_tolerate_missing_times() {
        let step: Step = serde_json::from_value(serde_json::json!({
            "index": 1, "member": "rx", "kind": "codex", "state": "handed_off"
        }))
        .unwrap();
        assert_eq!(step.state, StepState::HandedOff);
        assert_eq!(step.started_at, None);
        assert_eq!(serde_json::to_value(StepState::Active).unwrap(), "active");
    }
}
