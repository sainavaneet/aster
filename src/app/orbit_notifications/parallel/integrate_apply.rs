//! Applying an integrate step's patches to the main tree, one patch at a time
//! on background threads. Each result is recorded as it arrives, and the patch
//! about to be applied is recorded before it starts, so nothing is applied
//! twice after a restart. Only one task integrates into a repository at a time.
use std::sync::mpsc::{self, TryRecvError};

use super::integrate::IntegratePhase;
use super::Live;
use crate::app::orbit_notifications::steps::StepState;
use crate::app::App;
use crate::parallel_checkout::integrate::{ApplyReport, PatchToApply, Stopped};
use crate::parallel_checkout::integrate_resume::{apply_journaled, reconcile};
use crate::parallel_checkout::journal::Journal;

fn to_apply(patch: &super::integrate::IntegratedPatch) -> PatchToApply {
    PatchToApply {
        step: patch.step,
        path: patch.path.clone(),
        files: patch.files.clone(),
        after: patch.after.clone(),
    }
}

impl App {
    /// The task whose integration into the same repository is under way, if
    /// any: from the first patch being applied until its integrator reports.
    fn integration_holder(&self, index: usize, repo: &std::path::Path) -> Option<String> {
        let tasks = &self.orbit_notifications.tasks;
        let running = tasks
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != index)
            .find(|(_, task)| {
                task.steps.iter().any(|step| {
                    step.state == StepState::Active
                        && step.integrate.as_ref().is_some_and(|i| {
                            i.phase != IntegratePhase::Waiting && i.repo_root == repo
                        })
                })
            })
            .map(|(_, task)| task.task_id.clone());
        // A step that was cancelled or timed out (or even evicted from the
        // ledger) still has its git thread running until it finishes: the
        // repository stays reserved for it, as recorded by the thread itself.
        running.or_else(|| {
            let runtime = &self.orbit_notifications.parallel;
            let own = &tasks[index].task_id;
            runtime
                .live
                .iter()
                .filter(|(_, live)| matches!(live, Live::Integrating(_)))
                .find_map(|((task_id, step), _)| {
                    let root = runtime.apply_repo.get(&(task_id.clone(), *step))?;
                    (task_id != own && root.as_path() == repo).then(|| task_id.clone())
                })
        })
    }

    /// Starts applying once no other task is integrating into the repository.
    pub(super) fn start_when_repository_is_free(&mut self, index: usize, step: usize) {
        // Repository names from an older build are still being resolved.
        if self.orbit_notifications.parallel.normalizing.is_some() {
            return;
        }
        let task = &self.orbit_notifications.tasks[index];
        let Some(integrate) = task.steps[step].integrate.as_ref() else {
            return;
        };
        if let Some(holder) = self.integration_holder(index, &integrate.repo_root) {
            let note = format!("waiting for integration of task {holder}");
            self.edit_parallel_step(index, step, |s| {
                if let Some(integrate) = s.integrate.as_mut() {
                    if !integrate.log.contains(&note) {
                        integrate.log.push(note);
                    }
                }
            });
            return;
        }
        self.edit_parallel_step(index, step, |s| {
            if let Some(integrate) = s.integrate.as_mut() {
                integrate.phase = IntegratePhase::Applying;
            }
        });
        self.advance_applying(index, step);
    }

    /// Collects the result of the patch in flight, or starts the next one.
    pub(super) fn advance_applying(&mut self, index: usize, step: usize) {
        let key = (self.orbit_notifications.tasks[index].task_id.clone(), step);
        let live = &mut self.orbit_notifications.parallel.live;
        match live.remove(&key) {
            Some(Live::Integrating(applied)) => match applied.try_recv() {
                Ok(report) => self.record_patch(index, step, report),
                Err(TryRecvError::Empty) => {
                    let live = &mut self.orbit_notifications.parallel.live;
                    live.insert(key, Live::Integrating(applied));
                }
                Err(TryRecvError::Disconnected) => {
                    self.mark_unclear(index, step, "applying the patch stopped early");
                }
            },
            Some(other) => {
                self.orbit_notifications.parallel.live.insert(key, other);
            }
            None => self.resume_applying(index, step),
        }
    }

    /// Nothing is in flight (a first call, or after a restart): start the
    /// next patch that is not recorded as applied. Applying is safe to repeat,
    /// so one applied just before a crash is found in the tree and skipped.
    fn resume_applying(&mut self, index: usize, step: usize) {
        let task = &self.orbit_notifications.tasks[index];
        let Some(integrate) = task.steps[step].integrate.as_ref() else {
            return;
        };
        let pending: Vec<PatchToApply> = integrate
            .patches
            .iter()
            .filter(|p| !integrate.done(p.step))
            .map(to_apply)
            .collect();
        let repo_root = integrate.repo_root.clone();
        let journal = match self.integration_journal(index) {
            Ok(journal) => journal,
            Err(error) => return self.mark_unclear(index, step, &error),
        };
        let key = (task.task_id.clone(), step);
        let (send, applied) = mpsc::channel();
        let spawned = crate::thread_spawn::spawn_named("aster-orbit-reconcile", move || {
            let _ = send.send(reconcile(&repo_root, &pending, &journal));
        });
        self.watch_apply_thread(index, step, key, spawned, applied);
    }

    /// The durable journal of this task's integration.
    fn integration_journal(&self, index: usize) -> Result<Journal, String> {
        let dir = self.orbit_notifications.patch_dir();
        Journal::in_dir(&dir, &self.orbit_notifications.tasks[index].task_id)
            .map_err(|error| error.to_string())
    }

    /// Tracks a started apply thread, and the repository it holds until it ends.
    fn watch_apply_thread(
        &mut self,
        index: usize,
        step: usize,
        key: (String, usize),
        spawned: std::io::Result<std::thread::JoinHandle<()>>,
        applied: mpsc::Receiver<ApplyReport>,
    ) {
        match spawned {
            Ok(_) => {
                let repo = self.orbit_notifications.tasks[index].steps[step]
                    .integrate
                    .as_ref()
                    .map(|i| i.repo_root.clone());
                let runtime = &mut self.orbit_notifications.parallel;
                if let Some(repo) = repo {
                    runtime.apply_repo.insert(key.clone(), repo);
                }
                runtime.live.insert(key, Live::Integrating(applied));
            }
            Err(error) => self.mark_unclear(index, step, &error.to_string()),
        }
    }

    /// Records the patch as in flight (before it starts), then applies it.
    fn apply_next_patch(&mut self, index: usize, step: usize) {
        let integrate = self.orbit_notifications.tasks[index].steps[step]
            .integrate
            .as_ref();
        let next = integrate.and_then(|i| {
            let patch = i.patches.iter().find(|p| !i.done(p.step))?;
            Some((i.repo_root.clone(), to_apply(patch)))
        });
        let Some((repo_root, patch)) = next else {
            self.edit_parallel_step(index, step, |s| {
                if let Some(integrate) = s.integrate.as_mut() {
                    integrate.current = None;
                    integrate.phase = IntegratePhase::Ready;
                }
            });
            return;
        };
        let at = patch.step;
        self.edit_parallel_step(index, step, |s| {
            if let Some(integrate) = s.integrate.as_mut() {
                integrate.current = Some(at);
            }
        });
        let key = (self.orbit_notifications.tasks[index].task_id.clone(), step);
        let journal = match self.integration_journal(index) {
            Ok(journal) => journal,
            Err(error) => return self.mark_unclear(index, step, &error),
        };
        let (send, applied) = mpsc::channel();
        let spawned = crate::thread_spawn::spawn_named("aster-orbit-integrate", move || {
            let _ = send.send(apply_journaled(&repo_root, &patch, &journal));
        });
        self.watch_apply_thread(index, step, key, spawned, applied);
    }

    fn record_patch(&mut self, index: usize, step: usize, report: ApplyReport) {
        let stopped = report.stopped.is_some();
        self.edit_parallel_step(index, step, |s| {
            if let Some(integrate) = s.integrate.as_mut() {
                integrate.applied.extend(report.applied);
                integrate.present.extend(report.present);
                integrate.recovered.extend(report.recovered);
                integrate.current = None;
                if report.stopped.is_some() {
                    integrate.stopped = report.stopped;
                    integrate.phase = IntegratePhase::Ready;
                }
            }
        });
        if !stopped {
            self.apply_next_patch(index, step);
        }
    }

    /// The patch in flight has an unknown result: stop and say so honestly.
    fn mark_unclear(&mut self, index: usize, step: usize, why: &str) {
        let message = why.to_owned();
        self.edit_parallel_step(index, step, |s| {
            let Some(integrate) = s.integrate.as_mut() else {
                return;
            };
            let at = integrate.current.take();
            let patch = integrate
                .patches
                .iter()
                .find(|p| Some(p.step) == at || at.is_none() && !integrate.done(p.step));
            integrate.stopped = patch.map(|patch| Stopped {
                step: patch.step,
                path: patch.path.clone(),
                files: patch.files.clone(),
                conflict: false,
                unclear: true,
                message,
            });
            integrate.phase = IntegratePhase::Ready;
        });
    }
}

/// Drops the result channel of an integration whose step ended meanwhile (the
/// task was cancelled), once its thread has finished.
pub(super) fn drain_abandoned(app: &mut App) {
    let tasks = &app.orbit_notifications.tasks;
    let abandoned: Vec<(String, usize)> = app
        .orbit_notifications
        .parallel
        .live
        .iter()
        .filter(|(_, live)| matches!(live, Live::Integrating(_)))
        .map(|(key, _)| key.clone())
        .filter(|(task_id, step)| {
            let task = tasks.iter().find(|task| task.task_id == *task_id);
            !task.is_some_and(|t| {
                t.steps
                    .get(*step)
                    .is_some_and(|s| s.state == StepState::Active)
            })
        })
        .collect();
    for key in abandoned {
        let live = &mut app.orbit_notifications.parallel.live;
        match live.remove(&key) {
            Some(Live::Integrating(applied)) if applied.try_recv() == Err(TryRecvError::Empty) => {
                live.insert(key, Live::Integrating(applied));
            }
            _ => {}
        }
    }
    // A thread whose channel is gone has ended: its repository is free again.
    let runtime = &mut app.orbit_notifications.parallel;
    let live = &runtime.live;
    runtime.apply_repo.retain(|key, _| live.contains_key(key));
}
