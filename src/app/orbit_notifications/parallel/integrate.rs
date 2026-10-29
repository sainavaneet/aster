//! The integrate step: when every parallel step has finished, the role's
//! integrator gets one more step. Aster first applies the saved patches to the
//! main tree itself, in step order and once each, on a background thread; the
//! integrator is then told how it went, finishes any conflicts, runs the full
//! tests, and reports. That report, without a handoff, is the task's only one.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::Parallel;
use crate::api::schema::AgentPromptParams;
use crate::app::orbit_notifications::assign::DEFAULT_TIMEOUT_MS;
use crate::app::orbit_notifications::notify::bounded_text;
use crate::app::orbit_notifications::state::{Ack, LiveState, Submission, Task};
use crate::app::orbit_notifications::steps::{Step, StepState};
use crate::app::App;
use crate::parallel_checkout::integrate::Stopped;

/// Characters of a step's report kept as its note in the integrator's prompt.
const NOTE_CHARS: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IntegratePhase {
    /// Waiting for another task's integration of the same repository to end.
    Waiting,
    /// The patches are being applied to the main tree, one at a time.
    Applying,
    /// Applied (or stopped); waiting for the integrator to be free for its prompt.
    Ready,
    /// The integrator has its prompt.
    Running,
}

/// A parallel step's patch, as listed for the integrator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct IntegratedPatch {
    pub(in crate::app::orbit_notifications) step: usize,
    pub(in crate::app::orbit_notifications) member: String,
    pub(in crate::app::orbit_notifications) path: PathBuf,
    pub(in crate::app::orbit_notifications) files: Vec<String>,
    /// What the step reported.
    pub(in crate::app::orbit_notifications) note: String,
    /// The steps whose results this patch is built on (overlapping files).
    #[serde(default)]
    pub(in crate::app::orbit_notifications) after: Vec<usize>,
}

/// The integrate part of a `Step`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Integrate {
    pub(in crate::app::orbit_notifications) phase: IntegratePhase,
    /// The patches in the order they are applied.
    pub(in crate::app::orbit_notifications) patches: Vec<IntegratedPatch>,
    /// Parallel steps that left no patch, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(in crate::app::orbit_notifications) skipped: Vec<String>,
    /// The repository the patches are applied to; one integration at a time.
    pub(in crate::app::orbit_notifications) repo_root: PathBuf,
    /// Steps whose patches were applied cleanly, recorded as each is applied.
    #[serde(default)]
    pub(in crate::app::orbit_notifications) applied: Vec<usize>,
    /// Steps whose patches were already in the tree: Aster neither applied nor
    /// staged them, and an undo must not reverse them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(in crate::app::orbit_notifications) present: Vec<usize>,
    /// Steps found in the tree after a restart without a journal, assumed applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(in crate::app::orbit_notifications) recovered: Vec<usize>,
    /// The patch being applied, recorded before it starts: after a restart its
    /// result is unknown and is read from the tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::app::orbit_notifications) current: Option<usize>,
    /// Why the integration waited.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(in crate::app::orbit_notifications) log: Vec<String>,
    /// The patch that stopped the integration, with its conflicted files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::app::orbit_notifications) stopped: Option<Stopped>,
    /// The integrator's terminal, once found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::app::orbit_notifications) worker_id: Option<String>,
}

impl Integrate {
    /// Whether the patch of `step` needs no further applying.
    pub(in crate::app::orbit_notifications) fn done(&self, step: usize) -> bool {
        [&self.applied, &self.present, &self.recovered]
            .iter()
            .any(|steps| steps.contains(&step))
    }
}

impl Task {
    /// The step that integrates the parallel steps' patches, if there is one.
    pub(in crate::app::orbit_notifications) fn integrate_step(&self) -> Option<usize> {
        self.steps.iter().position(|step| step.integrate.is_some())
    }

    /// The patches are being applied, or wait for the integrator's prompt.
    pub(in crate::app::orbit_notifications) fn integration_in_progress(&self) -> bool {
        self.steps.iter().any(|step| {
            step.state == StepState::Active
                && step
                    .integrate
                    .as_ref()
                    .is_some_and(|i| i.phase != IntegratePhase::Running)
        })
    }

    /// The integrator has its prompt: the single-worker lifecycle takes over.
    pub(in crate::app::orbit_notifications) fn integration_running(&self) -> bool {
        self.steps.iter().any(|step| {
            step.integrate
                .as_ref()
                .is_some_and(|i| i.phase == IntegratePhase::Running)
        })
    }

    /// The integrator is prompted: it owns the task from here on.
    fn start_integration(&mut self, step: usize, worker_id: String, ack: Ack, now: u64) {
        self.worker_id = worker_id;
        self.submission = Submission::Pending;
        self.error = None;
        self.nudged_at = None;
        self.deadline = Some(now + self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        self.updated_at = now;
        self.live = LiveState {
            submission_ack: Some(ack),
            ..LiveState::default()
        };
        if let Some(integrate) = self.steps[step].integrate.as_mut() {
            integrate.phase = IntegratePhase::Running;
        }
    }
}

impl App {
    /// Starts integrating a task whose parallel steps have all finished.
    /// `false` when there is nothing to integrate or nobody to do it, and the
    /// task then reports its steps directly.
    pub(in crate::app::orbit_notifications) fn begin_integration(
        &mut self,
        index: usize,
        now: u64,
    ) -> bool {
        let task = &self.orbit_notifications.tasks[index];
        if task.integrate_step().is_some() {
            return false;
        }
        let patch_dir = self.orbit_notifications.patch_dir();
        let (mut patches, mut skipped, mut repo_root) = (Vec::new(), Vec::new(), None);
        for (at, step) in task.steps.iter().enumerate() {
            let Some(parallel) = step.parallel.as_ref() else {
                continue;
            };
            match (step.state, parallel.patch.as_ref()) {
                (StepState::Done, Some(patch)) if patch.path.starts_with(&patch_dir) => {
                    repo_root =
                        repo_root.or(parallel.checkout.as_ref().map(|c| c.repo_root.clone()));
                    patches.push(IntegratedPatch {
                        step: at,
                        member: step.member.clone(),
                        path: patch.path.clone(),
                        files: patch.files.clone(),
                        note: bounded_text(parallel.summary.as_deref().unwrap_or(""), NOTE_CHARS),
                        after: super::drive::chain_patches(task, at)
                            .into_iter()
                            .map(|(earlier, _)| earlier)
                            .collect(),
                    });
                }
                _ => skipped.push(skipped_note(at, step, parallel)),
            }
        }
        let role = task.role.clone();
        let Some(repo_root) = repo_root.filter(|_| !patches.is_empty()) else {
            return false;
        };
        let Some((member, kind)) = self.orbit_integrator(role.as_deref()) else {
            return false;
        };
        let worker_id = self.member_terminal(&member);
        let task = self.orbit_notifications.task_mut(index);
        let step = task.steps.len();
        let mut integrating = Step::active(step, &member, &kind, now);
        integrating.integrate = Some(Integrate {
            phase: IntegratePhase::Waiting,
            patches,
            skipped,
            repo_root,
            applied: Vec::new(),
            present: Vec::new(),
            recovered: Vec::new(),
            current: None,
            log: Vec::new(),
            stopped: None,
            worker_id,
        });
        task.steps.push(integrating);
        task.deadline = Some(now + task.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        task.updated_at = now;
        true
    }

    /// The terminal where team member `name` runs, if it does.
    pub(in crate::app::orbit_notifications) fn member_terminal(
        &self,
        name: &str,
    ) -> Option<String> {
        let (role, _) = self.orbit_team.roster.member(name)?;
        let (ws, tab) = self.resolve_role_tab(role).ok()?;
        self.orbit_member_pane(ws, tab, name)
            .map(|(_, terminal)| terminal)
    }

    /// Advances the integrate step of a task: collect the result of applying
    /// the patches, then prompt the integrator once it is free.
    pub(in crate::app::orbit_notifications) fn drive_integration(
        &mut self,
        index: usize,
        now: u64,
    ) {
        let Some(step) = self.orbit_notifications.tasks[index].integrate_step() else {
            return;
        };
        let task = &self.orbit_notifications.tasks[index];
        let Some(integrate) = task.steps[step].integrate.as_ref() else {
            return;
        };
        if task.steps[step].state != StepState::Active {
            return;
        }
        match integrate.phase {
            IntegratePhase::Waiting => self.start_when_repository_is_free(index, step),
            IntegratePhase::Applying => self.advance_applying(index, step),
            IntegratePhase::Ready => self.prompt_integrator(index, step, now),
            IntegratePhase::Running => {}
        }
    }

    fn prompt_integrator(&mut self, index: usize, step: usize, now: u64) {
        let task = &self.orbit_notifications.tasks[index];
        let member = task.steps[step].member.clone();
        let known = task.steps[step]
            .integrate
            .as_ref()
            .and_then(|i| i.worker_id.clone());
        let Some(terminal) = known.or_else(|| self.member_terminal(&member)) else {
            return;
        };
        self.edit_parallel_step(index, step, |s| {
            if let Some(integrate) = s.integrate.as_mut() {
                integrate.worker_id = Some(terminal.clone());
            }
        });
        if self.orbit_worker_has_open_task(&terminal) {
            return;
        }
        let Some(target) = self.idle_orbit_target(&terminal, now) else {
            return;
        };
        let Some(text) = self.integrator_prompt(index, step, &terminal) else {
            return;
        };
        let task_id = self.orbit_notifications.tasks[index].task_id.clone();
        let prompt = AgentPromptParams {
            target,
            text,
            wait: None,
        };
        match self.queue_orbit_prompt(format!("integrate:{task_id}:{step}"), &prompt) {
            Ok(ack) => {
                let task = self.orbit_notifications.task_mut(index);
                task.start_integration(step, terminal, ack, now);
            }
            Err(error) if error.is_retryable() => {}
            Err(error) => {
                tracing::warn!(%error, "cannot prompt the integrator");
                self.edit_parallel_step(index, step, |s| s.fail(error.to_string(), now));
            }
        }
    }
}

fn skipped_note(at: usize, step: &Step, parallel: &Parallel) -> String {
    let why = parallel
        .log
        .last()
        .map_or("it did not complete", String::as_str);
    format!("step {} ({}) left no patch: {why}", at + 1, step.member)
}
