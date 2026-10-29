//! Seams for tests of what follows the parallel steps.
use std::path::PathBuf;

use super::{Phase, StepState};
use crate::api::schema::OrbitOutcome;
use crate::parallel_checkout::{Checkout, PatchSummary};

/// A parallel step as the tick leaves it once it has reported and its patch is
/// saved, for tests of what follows.
pub(crate) struct FinishedStep {
    pub(crate) summary: String,
    /// `None`: the step failed and left no patch.
    pub(crate) patch: Option<PatchSummary>,
    pub(crate) repo_root: PathBuf,
}

impl crate::app::App {
    /// Marks the parallel steps of `task_id` finished, with patches in `patch_dir`.
    pub(crate) fn finish_parallel_steps_for_test(
        &mut self,
        task_id: &str,
        patch_dir: PathBuf,
        finished: Vec<FinishedStep>,
    ) {
        self.orbit_notifications.parallel.patch_dir = Some(patch_dir);
        let Some(index) = self.orbit_notifications.find(task_id) else {
            return;
        };
        let task = self.orbit_notifications.task_mut(index);
        for (step, done) in task.steps.iter_mut().zip(finished) {
            let Some(parallel) = step.parallel.as_mut() else {
                continue;
            };
            step.state = if done.patch.is_some() {
                StepState::Done
            } else {
                StepState::Failed
            };
            parallel.phase = Phase::Capturing;
            parallel.outcome = Some(OrbitOutcome::Completed);
            parallel.summary = Some(done.summary);
            parallel.patch = done.patch;
            // Creation records the repository by its real path.
            let repo_root = std::fs::canonicalize(&done.repo_root).unwrap_or(done.repo_root);
            parallel.checkout = Some(Checkout {
                path: repo_root.join("released-checkout"),
                repo_root,
                subdir: PathBuf::new(),
                snapshot: String::new(),
                baseline: String::new(),
                method: crate::parallel_checkout::CheckoutMethod::Worktree,
            });
            parallel.released = true;
            parallel.removed = true;
        }
    }

    /// Starts the integrate step of `task_id` without driving it.
    pub(crate) fn begin_integration_for_test(&mut self, task_id: &str) -> bool {
        let Some(index) = self.orbit_notifications.find(task_id) else {
            return false;
        };
        let now = self.orbit_notifications.now_ms(std::time::Instant::now());
        self.begin_integration(index, now)
    }

    /// The ledger as a restart leaves it mid-integration: `applied` recorded,
    /// `current` the patch that was being applied, nothing in flight.
    pub(crate) fn simulate_integration_restart_for_test(
        &mut self,
        task_id: &str,
        applied: Vec<usize>,
        current: Option<usize>,
    ) {
        let Some(index) = self.orbit_notifications.find(task_id) else {
            return;
        };
        let task = self.orbit_notifications.task_mut(index);
        let Some(step) = task.integrate_step() else {
            return;
        };
        if let Some(integrate) = task.steps[step].integrate.as_mut() {
            integrate.applied = applied;
            integrate.current = current;
            integrate.stopped = None;
            integrate.phase = super::IntegratePhase::Applying;
        }
        let key = (task_id.to_owned(), step);
        self.orbit_notifications.parallel.live.remove(&key);
    }

    /// Names the repository of `task_id`'s integrate step as an older build
    /// might have recorded it, and has the ledger resolve it again as on load.
    pub(crate) fn alias_integration_root_for_test(&mut self, task_id: &str, alias: PathBuf) {
        let Some(index) = self.orbit_notifications.find(task_id) else {
            return;
        };
        for step in self.orbit_notifications.task_mut(index).steps.iter_mut() {
            if let Some(integrate) = step.integrate.as_mut() {
                integrate.repo_root = alias.clone();
            }
        }
        self.normalize_integration_roots();
    }

    /// Drops a task from the ledger, as eviction of a finished task does.
    pub(crate) fn evict_task_for_test(&mut self, task_id: &str) {
        self.orbit_notifications
            .tasks
            .retain(|task| task.task_id != task_id);
    }

    /// One ledger tick now, ignoring the tick spacing.
    pub(crate) fn pump_orbit_for_test(&mut self) {
        self.orbit_notifications.next_tick = 0;
        self.pump_orbit_notifications_at(std::time::Instant::now());
    }
}
