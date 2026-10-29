//! The ledger tick for parallel steps: launch a step's checkout and agent when
//! its files are free, prompt the agent once it is idle, watch it, and release
//! its agent and checkout when the step ends. All Git and copy work runs on
//! background threads; the tick only starts it and polls the results.
use std::sync::mpsc::{self, TryRecvError};

use std::path::PathBuf;

use serde_json::Value;

use super::request::earlier_step_note;
use super::{shared_path, Live, Phase};
use crate::api::schema::{AgentPromptParams, OrbitAgentStartParams, OrbitWorktreeRequest};
use crate::app::orbit_notifications::assign::{
    assignment_instructions, report_command, DEFAULT_TIMEOUT_MS,
};
use crate::app::orbit_notifications::state::{Delivery, Task};
use crate::app::orbit_notifications::steps::{Step, StepState};
use crate::app::App;
use crate::events::ParallelLaunch;

/// How long a step's checkout and agent may take to start, and then to be
/// ready for its prompt.
const LAUNCH_TIMEOUT_MS: u64 = 3 * 60 * 1000;
const READY_TIMEOUT_MS: u64 = 3 * 60 * 1000;
/// Start errors that clear by themselves: the step keeps waiting.
const WAITING_CODES: [&str; 2] = ["orbit_worker_limit", "review_initializing"];

pub(super) fn error_text(response: &Value) -> Option<String> {
    let error = response.get("error")?;
    Some(
        error["message"]
            .as_str()
            .unwrap_or("the request failed")
            .to_owned(),
    )
}

/// The patches of earlier steps this step builds on, in order: steps that
/// finished with a patch and are linked to it by overlapping files, directly or
/// through each other. A step's own patch is relative to these.
pub(super) fn chain_patches(task: &Task, step: usize) -> Vec<(usize, PathBuf)> {
    let files = |index: usize| {
        task.steps[index]
            .parallel
            .as_ref()
            .map(|p| p.files.as_slice())
    };
    let patch = |index: usize| task.steps[index].parallel.as_ref()?.patch.as_ref();
    let mut chain = vec![step];
    loop {
        let linked = (0..step).find(|earlier| {
            !chain.contains(earlier)
                && patch(*earlier).is_some()
                && chain.iter().any(|member| {
                    files(*earlier)
                        .zip(files(*member))
                        .is_some_and(|(a, b)| shared_path(a, b).is_some())
                })
        });
        match linked {
            Some(earlier) => chain.push(earlier),
            None => break,
        }
    }
    chain.sort_unstable();
    chain
        .into_iter()
        .filter(|member| *member != step)
        .filter_map(|member| Some((member, patch(member)?.path.clone())))
        .collect()
}

impl App {
    /// Advances every task that still holds parallel steps' agents or checkouts.
    pub(in crate::app::orbit_notifications) fn drive_parallel_steps(&mut self, now: u64) {
        super::integrate_apply::drain_abandoned(self);
        self.poll_root_normalization();
        let tasks: Vec<usize> = (0..self.orbit_notifications.tasks.len())
            .filter(|index| self.orbit_notifications.tasks[*index].needs_driving())
            .collect();
        for index in tasks {
            self.drive_parallel_task(index, now);
        }
    }

    fn drive_parallel_task(&mut self, index: usize, now: u64) {
        let task = &self.orbit_notifications.tasks[index];
        let open = task.delivery == Delivery::AwaitingReport;
        let timed_out = task.deadline.is_some_and(|deadline| now >= deadline);
        if open && timed_out && task.has_unfinished_steps() {
            let task_id = task.task_id.clone();
            let summary = "Parallel steps did not all report before the assignment timeout";
            self.resolve_unreported_assignment(
                &task_id,
                Delivery::TimedOut,
                summary.into(),
                "assignment deadline passed",
                now,
            );
        }
        for step in 0..self.orbit_notifications.tasks[index].steps.len() {
            let task = &self.orbit_notifications.tasks[index];
            let Some(parallel) = &task.steps[step].parallel else {
                continue;
            };
            match task.steps[step].state {
                StepState::Pending | StepState::Active if open => {
                    self.advance_parallel_step(index, step, parallel.phase, now);
                }
                StepState::Pending | StepState::Active => {}
                _ => self.settle_parallel_step(index, step, now),
            }
        }
        if open {
            self.drive_integration(index, now);
        }
        self.finish_fanout_if_done(index, now);
    }

    /// Runs `change` on a step of task `index`, marking the ledger dirty.
    pub(super) fn edit_parallel_step<R>(
        &mut self,
        index: usize,
        step: usize,
        change: impl FnOnce(&mut Step) -> R,
    ) -> Option<R> {
        let step = self
            .orbit_notifications
            .task_mut(index)
            .steps
            .get_mut(step)?;
        Some(change(step))
    }

    fn fail_parallel_step(&mut self, index: usize, step: usize, reason: String, now: u64) {
        tracing::warn!(step, %reason, "orbit parallel step failed");
        self.edit_parallel_step(index, step, |step| step.fail(reason, now));
    }

    fn advance_parallel_step(&mut self, index: usize, step: usize, phase: Phase, now: u64) {
        match phase {
            Phase::Waiting => self.launch_parallel_step(index, step, now),
            Phase::Launching => self.poll_parallel_launch(index, step, now),
            Phase::Starting => self.prompt_parallel_step(index, step, now),
            Phase::Running => self.watch_parallel_worker(index, step, now),
            Phase::Capturing => self.poll_parallel_capture(index, step, now),
        }
    }

    /// The reason a waiting step may not start yet: an earlier unfinished step
    /// of its task, or a running step of another task, shares files with it.
    fn parallel_blocker(&self, index: usize, step: usize) -> Option<String> {
        let tasks = &self.orbit_notifications.tasks;
        let mine = &tasks[index].steps[step].parallel.as_ref()?.files;
        let unfinished = |step: &Step| matches!(step.state, StepState::Pending | StepState::Active);
        for (earlier, other) in tasks[index].steps[..step].iter().enumerate() {
            let files = other.parallel.as_ref().filter(|_| unfinished(other));
            if let Some(path) = files.and_then(|p| shared_path(mine, &p.files)) {
                return Some(earlier_step_note(earlier + 1, &path));
            }
        }
        for task in tasks
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != index)
            .map(|(_, task)| task)
        {
            for (number, other) in task.steps.iter().enumerate() {
                let running = other
                    .parallel
                    .as_ref()
                    .filter(|_| other.state == StepState::Active);
                if let Some(path) = running.and_then(|p| shared_path(mine, &p.files)) {
                    return Some(format!(
                        "overlaps step {} of {} on {path}; waits for it",
                        number + 1,
                        task.task_id
                    ));
                }
            }
        }
        None
    }

    fn launch_parallel_step(&mut self, index: usize, step: usize, now: u64) {
        if let Some(note) = self.parallel_blocker(index, step) {
            self.edit_parallel_step(index, step, |s| s.note(note));
            return;
        }
        let task = &self.orbit_notifications.tasks[index];
        let Some(parallel) = &task.steps[step].parallel else {
            return;
        };
        let task_id = task.task_id.clone();
        let label = format!("par{:x}{step}", now & 0x0fff_ffff);
        let params = OrbitAgentStartParams {
            name: label.clone(),
            kind: task.steps[step].kind.clone(),
            pane_id: None,
            model: parallel.model.clone(),
            reuse: Some(false),
            worktree: Some(OrbitWorktreeRequest::Enabled(true)),
            permission_mode: parallel.permission_mode.clone(),
        };
        let apply = chain_patches(task, step)
            .into_iter()
            .map(|(_, path)| path)
            .collect();
        let plan = ParallelLaunch {
            task_id: task_id.clone(),
            step,
            label: label.clone(),
            apply,
            checkout: None,
        };
        let (respond_to, response) = mpsc::channel();
        let request_id = format!("parallel:{task_id}:{step}");
        match self.start_parallel_orbit_worker(&request_id, params, plan, &respond_to) {
            Ok(()) => {
                let live = &mut self.orbit_notifications.parallel.live;
                live.insert((task_id, step), Live::Launching(response));
                self.edit_parallel_step(index, step, |s| s.start_launch(&label, now));
            }
            Err(error) => {
                let value: Value = serde_json::from_str(&error).unwrap_or_default();
                let code = value["error"]["code"].as_str().unwrap_or_default();
                let message = error_text(&value).unwrap_or(error.clone());
                if WAITING_CODES.contains(&code) {
                    self.edit_parallel_step(index, step, |s| {
                        s.note(format!("waiting to start: {message}"))
                    });
                } else {
                    self.fail_parallel_step(
                        index,
                        step,
                        format!("could not start: {message}"),
                        now,
                    );
                }
            }
        }
    }

    fn poll_parallel_launch(&mut self, index: usize, step: usize, now: u64) {
        let key = (self.orbit_notifications.tasks[index].task_id.clone(), step);
        let live = &mut self.orbit_notifications.parallel.live;
        let Some(Live::Launching(response)) = live.remove(&key) else {
            let reason = "the server restarted while the step's agent was starting".to_owned();
            self.fail_parallel_step(index, step, reason, now);
            return;
        };
        match response.try_recv() {
            Ok(text) => self.finish_parallel_launch(index, step, &text, now),
            Err(TryRecvError::Empty) => {
                let started = self.orbit_notifications.tasks[index].steps[step].started_at;
                // Kept even when the step fails below: a late result still names
                // an agent and a checkout that must be released.
                let live = &mut self.orbit_notifications.parallel.live;
                live.insert(key, Live::Launching(response));
                if started.is_some_and(|at| now.saturating_sub(at) > LAUNCH_TIMEOUT_MS) {
                    let reason = "the step's agent did not start in time".into();
                    self.fail_parallel_step(index, step, reason, now);
                }
            }
            Err(TryRecvError::Disconnected) => {
                self.fail_parallel_step(
                    index,
                    step,
                    "the agent start ended without a response".into(),
                    now,
                );
            }
        }
    }

    fn finish_parallel_launch(&mut self, index: usize, step: usize, text: &str, now: u64) {
        let value: Value = serde_json::from_str(text).unwrap_or_default();
        if let Some(message) = error_text(&value) {
            self.fail_parallel_step(index, step, format!("could not start: {message}"), now);
            return;
        }
        let terminal_id = value["result"]["agent"]["terminal_id"]
            .as_str()
            .map(str::to_owned);
        let Some(terminal_id) = terminal_id else {
            self.fail_parallel_step(
                index,
                step,
                "the agent start returned no terminal".into(),
                now,
            );
            return;
        };
        self.edit_parallel_step(index, step, |s| s.launched(terminal_id));
    }

    /// The tracked prompt a step's agent receives.
    fn parallel_prompt(&self, index: usize, step: usize, worker_id: &str) -> Option<String> {
        let task = &self.orbit_notifications.tasks[index];
        let parallel = task.steps[step].parallel.as_ref()?;
        let workdir = parallel.checkout.as_ref()?.workdir();
        let mut text = format!(
            "{}\n\nThis is step {} of a parallel task. Work only inside {}, your own isolated checkout of the project: edit there, do not commit or push, and do not leave it. Aster saves your changes as a patch when you report.",
            parallel.prompt,
            step + 1,
            workdir.display()
        );
        for (earlier, _) in chain_patches(task, step) {
            text.push_str(&format!(
                "\n\nStep {}'s changes to files you also declared are already applied in your checkout; build on them.",
                earlier + 1
            ));
        }
        let command = report_command(&task.task_id, worker_id, Some(step)).ok()?;
        text.push_str(&assignment_instructions(&task.task_id, &command, false));
        Some(text)
    }

    fn prompt_parallel_step(&mut self, index: usize, step: usize, now: u64) {
        let Some(worker_id) = self.parallel_worker(index, step) else {
            self.fail_parallel_step(index, step, "the step has no agent".into(), now);
            return;
        };
        if let Err(reason) = self.observe_worker(&worker_id) {
            self.fail_parallel_step(index, step, format!("agent lost: {reason}"), now);
            return;
        }
        let started = self.orbit_notifications.tasks[index].steps[step].started_at;
        if started.is_some_and(|at| now.saturating_sub(at) > LAUNCH_TIMEOUT_MS + READY_TIMEOUT_MS) {
            self.fail_parallel_step(
                index,
                step,
                "the step's agent was not ready for its prompt in time".into(),
                now,
            );
            return;
        }
        let Some(target) = self.idle_orbit_target(&worker_id, now) else {
            return;
        };
        let Some(text) = self.parallel_prompt(index, step, &worker_id) else {
            self.fail_parallel_step(index, step, "the step has no checkout".into(), now);
            return;
        };
        let task_id = self.orbit_notifications.tasks[index].task_id.clone();
        let prompt = AgentPromptParams {
            target,
            text,
            wait: None,
        };
        match self.queue_orbit_prompt(format!("parallel:{task_id}:{step}"), &prompt) {
            Ok(_ack) => {
                let task = self.orbit_notifications.task_mut(index);
                // Each step gets the assignment's full timeout from its own start.
                let timeout = task.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
                task.deadline = task.deadline.max(Some(now + timeout));
                task.updated_at = now;
                self.edit_parallel_step(index, step, |s| s.prompted(now));
            }
            Err(error) if error.is_retryable() => {
                let note = format!("waiting to prompt the agent: {error}");
                self.edit_parallel_step(index, step, |s| s.note(note));
            }
            Err(error) => {
                self.fail_parallel_step(
                    index,
                    step,
                    format!("could not prompt the agent: {error}"),
                    now,
                );
            }
        }
    }

    fn parallel_worker(&self, index: usize, step: usize) -> Option<String> {
        let task = &self.orbit_notifications.tasks[index];
        task.steps[step].parallel.as_ref()?.worker_id.clone()
    }

    fn watch_parallel_worker(&mut self, index: usize, step: usize, now: u64) {
        let Some(worker_id) = self.parallel_worker(index, step) else {
            return;
        };
        if let Err(reason) = self.observe_worker(&worker_id) {
            self.fail_parallel_step(index, step, format!("agent lost: {reason}"), now);
        }
    }
}
