//! Parallel steps: one task fanned out to several new agents, each working in
//! its own snapshot checkout of the Orbit project (see `parallel_checkout`). The
//! task stays one ledger entry with one final report to Polaris, sent when every
//! step has finished. A step's changes come back as a patch in the session data
//! directory. Steps whose declared files overlap run one after another.
//!
//! The event loop only starts background work and polls it on the ledger tick:
//! `request` builds steps, `drive` advances them, `report` takes a step's report
//! and finishes the task.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use serde::{Deserialize, Serialize};

use super::state::Task;
use super::steps::{Step, StepState};
use crate::api::schema::OrbitOutcome;
use crate::parallel_checkout::integrate::ApplyReport;
use crate::parallel_checkout::{Checkout, PatchSummary, RemoveError};

mod drive;
mod integrate;
mod integrate_apply;
mod integrate_roots;
mod integrate_text;
mod release;
mod report;
mod request;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use test_support::FinishedStep;

pub(super) use integrate::{Integrate, IntegratePhase};
pub(super) use integrate_text::merge_report;
pub(super) use request::check_requests;

/// Parallel steps one request may start.
pub(super) const MAX_PARALLEL: usize = 4;

/// Where a parallel step stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// Not started: waiting for an overlapping step, or for room to launch.
    Waiting,
    /// Its checkout and agent are being created.
    Launching,
    /// The agent runs; its prompt waits for it to be idle.
    Starting,
    /// The agent has its prompt and works.
    Running,
    /// It reported; its patch is being saved.
    Capturing,
}

/// The parallel part of a `Step`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Parallel {
    pub(super) prompt: String,
    /// Paths the step declared it will change.
    pub(super) files: Vec<String>,
    pub(super) phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) permission_mode: Option<String>,
    /// The terminal and name of the step's agent, once launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) worker_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) checkout: Option<Checkout>,
    /// The saved patch of the step's changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) patch: Option<PatchSummary>,
    /// What the step's agent reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) outcome: Option<OrbitOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) summary: Option<String>,
    /// Why the step waited or failed, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) log: Vec<String>,
    /// Its agent was stopped and its checkout removed, or removal was given up.
    #[serde(default)]
    pub(super) released: bool,
    /// The checkout is confirmed gone from disk.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) removed: bool,
    /// Removals tried so far.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(super) removals: u32,
}

fn is_zero(count: &u32) -> bool {
    *count == 0
}

impl Parallel {
    pub(super) fn new(prompt: String, files: Vec<String>) -> Self {
        Self {
            prompt,
            files,
            phase: Phase::Waiting,
            model: None,
            permission_mode: None,
            worker_id: None,
            worker_name: None,
            checkout: None,
            patch: None,
            outcome: None,
            summary: None,
            log: Vec::new(),
            released: false,
            removed: false,
            removals: 0,
        }
    }

    /// Adds `note` to the log unless it is already the last word on it.
    pub(super) fn note(&mut self, note: String) {
        if !self.log.contains(&note) {
            self.log.push(note);
        }
    }
}

impl Task {
    /// A task whose steps run in parallel; the ledger tick drives it instead of
    /// the single-worker lifecycle.
    pub(super) fn is_fanout(&self) -> bool {
        self.steps.iter().any(|step| step.parallel.is_some()) && !self.integration_running()
    }

    /// The tick has work here: a fanned-out task still open, or steps whose
    /// agents or checkouts are to be released.
    pub(super) fn needs_driving(&self) -> bool {
        let open = self.delivery == super::state::Delivery::AwaitingReport && self.is_fanout();
        open || self.holds_parallel_resources() || self.integration_in_progress()
    }

    /// Resolved and nothing of it left to release, so the ledger may evict it.
    pub(super) fn is_evictable(&self) -> bool {
        self.delivery.is_terminal()
            && !self.holds_parallel_resources()
            && !self.integration_in_progress()
    }

    /// A task that ended fails the parallel steps it never finished, so their
    /// agents and checkouts are released whichever path ended it.
    pub(super) fn fail_parallel_steps(&mut self, now: u64) {
        for step in &mut self.steps {
            if step.parallel.is_some()
                && matches!(step.state, StepState::Active | StepState::Pending)
            {
                step.state = StepState::Failed;
                step.finished_at = Some(now);
            }
        }
    }

    /// An agent or checkout of a parallel step is still to be released.
    pub(super) fn holds_parallel_resources(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.parallel.as_ref().is_some_and(|p| !p.released))
    }

    /// Checkouts of finished steps not confirmed removed are tried again: after
    /// a restart, whatever stopped their removal may be gone.
    pub(super) fn resweep_parallel_checkouts(&mut self) {
        for step in &mut self.steps {
            let finished = !matches!(step.state, StepState::Pending | StepState::Active);
            if let Some(parallel) = step.parallel.as_mut() {
                if finished && parallel.checkout.is_some() && !parallel.removed {
                    parallel.released = false;
                    parallel.removals = 0;
                }
            }
        }
    }

    /// A step has not finished.
    pub(super) fn has_unfinished_steps(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step.state, StepState::Pending | StepState::Active))
    }

    /// `parallel 1/3 done`, for a fanned-out task.
    pub(super) fn parallel_line(&self) -> Option<String> {
        if let Some(integrate) = self.integrate_step().map(|step| &self.steps[step]) {
            let phase = match integrate.integrate.as_ref().map(|i| i.phase) {
                Some(IntegratePhase::Applying) => "applying patches",
                Some(IntegratePhase::Ready) => "waiting for",
                _ => "integrating by",
            };
            return Some(format!("{phase} {}", integrate.member));
        }
        let steps: Vec<&Step> = self.steps.iter().filter(|s| s.parallel.is_some()).collect();
        if steps.is_empty() {
            return None;
        }
        let finished = steps
            .iter()
            .filter(|step| !matches!(step.state, StepState::Pending | StepState::Active))
            .count();
        Some(format!("parallel {finished}/{} done", steps.len()))
    }
}

/// The in-flight background work of parallel steps, which does not survive a
/// restart: a step found mid-launch then fails, and a capture starts again.
pub(super) enum Live {
    Launching(Receiver<String>),
    Capturing(Receiver<Result<PatchSummary, String>>),
    /// One saved patch is being applied to the main tree.
    Integrating(Receiver<ApplyReport>),
    /// The checkout is being removed.
    Removing(Receiver<Result<(), RemoveError>>),
}

#[derive(Default)]
pub(super) struct Runtime {
    pub(super) live: HashMap<(String, usize), Live>,
    /// Earliest ledger time of the next removal try of a checkout that failed.
    pub(super) removal_after: HashMap<(String, usize), u64>,
    /// The repository each running apply thread works in, by (task, step),
    /// independent of the ledger: a cancelled or evicted task's thread still
    /// reserves its repository.
    pub(super) apply_repo: HashMap<(String, usize), PathBuf>,
    /// Waiting for the real paths of repositories recorded by older builds.
    pub(super) normalizing: Option<Receiver<Vec<(PathBuf, PathBuf)>>>,
    /// Where patches go; the session data directory's `orbit-patches`.
    pub(super) patch_dir: Option<PathBuf>,
}

fn trimmed(path: &str) -> &str {
    path.trim().trim_start_matches("./").trim_end_matches('/')
}

/// The first path that two declared file sets share: equal, or one inside the
/// other, in which case the deeper one is named. `.` (the whole project)
/// overlaps everything.
pub(super) fn shared_path(a: &[String], b: &[String]) -> Option<String> {
    let whole = |path: &str| matches!(trimmed(path), "" | ".");
    for first in a {
        for second in b {
            let (x, y) = (Path::new(trimmed(first)), Path::new(trimmed(second)));
            if whole(first) || whole(second) || x.starts_with(y) || y.starts_with(x) {
                let deeper = if x.starts_with(y) { first } else { second };
                return Some(trimmed(if whole(second) { first } else { deeper }).to_owned());
            }
        }
    }
    None
}

impl Step {
    /// Adds a note to the step's log.
    pub(super) fn note(&mut self, note: String) {
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.note(note);
        }
    }

    /// The step failed without finishing its work; `reason` goes in its log.
    pub(super) fn fail(&mut self, reason: String, now: u64) {
        self.note(reason);
        self.state = StepState::Failed;
        self.finished_at = Some(now);
    }

    /// The step's checkout and agent are being created.
    pub(super) fn start_launch(&mut self, worker_name: &str, now: u64) {
        self.state = StepState::Active;
        self.started_at = Some(now);
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.phase = Phase::Launching;
            parallel.worker_name = Some(worker_name.to_owned());
        }
    }

    /// The step's agent exists in `terminal_id`.
    pub(super) fn launched(&mut self, terminal_id: String) {
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.worker_id = Some(terminal_id);
            if parallel.phase == Phase::Launching {
                parallel.phase = Phase::Starting;
            }
        }
    }

    /// The agent has its prompt.
    pub(super) fn prompted(&mut self, _now: u64) {
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.phase = Phase::Running;
        }
    }

    /// The step's checkout exists. One that appears after the step was
    /// released (its start finished late) is still to be removed.
    pub(super) fn recorded(&mut self, checkout: Checkout) {
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.checkout = Some(checkout);
            parallel.released = false;
            parallel.removed = false;
        }
    }

    /// The agent's report: its patch is saved next.
    pub(super) fn reported(&mut self, outcome: OrbitOutcome, summary: String) {
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.outcome = Some(outcome);
            parallel.summary = Some(summary);
            parallel.phase = Phase::Capturing;
        }
    }

    /// The patch is saved (or could not be): done if the agent completed, else failed.
    pub(super) fn finished(&mut self, patch: Option<PatchSummary>, problem: String, now: u64) {
        let completed = self
            .parallel
            .as_ref()
            .is_some_and(|p| p.outcome == Some(OrbitOutcome::Completed));
        let saved = patch.is_some();
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.patch = patch;
            if !problem.is_empty() {
                parallel.note(problem);
            }
        }
        self.state = if completed && saved {
            StepState::Done
        } else {
            StepState::Failed
        };
        self.finished_at = Some(now);
    }

    /// Nothing is left of the step to release.
    pub(super) fn released(&mut self) {
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.released = true;
            parallel.removed = true;
        }
    }

    /// A removal of the checkout starts.
    pub(super) fn removal_started(&mut self) {
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.removals += 1;
        }
    }

    /// The checkout is not one Aster may delete: nothing was touched, retrying
    /// cannot help, and the step stops trying (the log names the path).
    pub(super) fn removal_refused(&mut self, error: &str) {
        if let Some(parallel) = self.parallel.as_mut() {
            parallel.note(error.to_owned());
            parallel.released = true;
        }
    }

    /// A removal failed: the reason goes in the log; after `give_up_after`
    /// tries the step stops retrying (a restart tries again).
    pub(super) fn removal_failed(&mut self, error: &str, give_up_after: u32) -> bool {
        let Some(parallel) = self.parallel.as_mut() else {
            return true;
        };
        let (tries, path) = (
            parallel.removals,
            parallel
                .checkout
                .as_ref()
                .map(|c| c.path.display().to_string()),
        );
        parallel.note(format!(
            "could not remove the checkout (try {tries}): {error}"
        ));
        let give_up = tries >= give_up_after;
        if give_up {
            let path = path.unwrap_or_default();
            parallel.note(format!(
                "gave up removing the checkout; delete {path} by hand"
            ));
            parallel.released = true;
        }
        give_up
    }
}
