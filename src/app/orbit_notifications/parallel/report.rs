//! A parallel step's report, its patch, and the one report Polaris gets when
//! every step has finished.
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};

use serde_json::{json, Value};

use super::{Live, Phase};
use crate::api::schema::{OrbitAgentReportParams, OrbitOutcome};
use crate::app::orbit_notifications::facts::check_path;
use crate::app::orbit_notifications::ledger::CONFLICTING_REPORT;
use crate::app::orbit_notifications::notify::bounded_text;
use crate::app::orbit_notifications::state::{Delivery, Task};
use crate::app::orbit_notifications::steps::StepState;
use crate::app::orbit_notifications::{token, Notifications};
use crate::app::App;
use crate::parallel_checkout::{capture_patch, MAX_LISTED_FILES};

const STEP_SUMMARY_CHARS: usize = 600;
const MAX_REPORT_BYTES: usize = 3900;
const BLOCKED_ON_CHARS: usize = 400;

impl Notifications {
    /// Where step patches are saved: next to the ledger, in the session data
    /// directory; a temporary directory for a session that saves nothing.
    pub(in crate::app::orbit_notifications) fn patch_dir(&self) -> PathBuf {
        if let Some(dir) = &self.parallel.patch_dir {
            return dir.clone();
        }
        let beside = self.store_path.as_ref().and_then(|path| path.parent());
        beside.map_or_else(
            || std::env::temp_dir().join(format!("aster-orbit-patches-{}", std::process::id())),
            |dir| dir.join("orbit-patches"),
        )
    }
}

impl App {
    /// Handles `params` when it reports a parallel step; `None` for any other
    /// report, which is admitted as usual (and rejected if it is not genuine).
    pub(in crate::app::orbit_notifications) fn parallel_step_report(
        &mut self,
        params: &OrbitAgentReportParams,
        now: u64,
    ) -> Option<Result<Value, String>> {
        let step = params.step?;
        let index = self.orbit_notifications.find(&params.task_id)?;
        let task = &self.orbit_notifications.tasks[index];
        let parallel = task.steps.get(step)?.parallel.as_ref()?;
        let valid_token = token::accepts(task.token_hash.as_ref(), params.token.as_deref());
        if !valid_token || parallel.worker_id.as_deref() != Some(params.worker_id.as_str()) {
            return None;
        }
        if params.handoff.is_some() {
            return Some(Err(
                "A parallel step reports its result directly; report again without handoff".into(),
            ));
        }
        let same = parallel.outcome.as_ref() == Some(&params.outcome)
            && parallel.summary.as_deref() == Some(params.summary.as_str());
        if parallel.outcome.is_some() {
            return Some(if same {
                Ok(json!(task))
            } else {
                Err(CONFLICTING_REPORT.into())
            });
        }
        if task.delivery != Delivery::AwaitingReport {
            return Some(Err(format!(
                "Assignment already resolved as {}; do not retry",
                task.delivery.as_str()
            )));
        }
        if !matches!(parallel.phase, Phase::Starting | Phase::Running) {
            return Some(Err("This step is not running; do not report it".into()));
        }
        let (outcome, summary) = (params.outcome.clone(), params.summary.clone());
        self.edit_parallel_step(index, step, |s| s.reported(outcome, summary));
        self.start_parallel_capture(index, step, now);
        Some(Ok(json!(&self.orbit_notifications.tasks[index])))
    }

    /// Saves the step's patch on a background thread.
    pub(super) fn start_parallel_capture(&mut self, index: usize, step: usize, now: u64) {
        let task = &self.orbit_notifications.tasks[index];
        let checkout = task.steps[step]
            .parallel
            .as_ref()
            .and_then(|parallel| parallel.checkout.clone());
        let Some(checkout) = checkout else {
            self.settle_reported_step(
                index,
                step,
                None,
                "the step has no checkout to take a patch from",
                now,
            );
            return;
        };
        let path = self.orbit_notifications.patch_dir().join(format!(
            "{}-step{}.patch",
            task.task_id,
            step + 1
        ));
        let key = (task.task_id.clone(), step);
        let (send, patch) = mpsc::channel();
        let spawned = crate::thread_spawn::spawn_named("aster-orbit-patch", move || {
            let _ = send.send(capture_patch(&checkout, &path));
        });
        match spawned {
            Ok(_) => {
                let live = &mut self.orbit_notifications.parallel.live;
                live.insert(key, Live::Capturing(patch));
            }
            Err(error) => {
                let reason = format!("could not start saving the patch: {error}");
                self.settle_reported_step(index, step, None, &reason, now);
            }
        }
    }

    pub(super) fn poll_parallel_capture(&mut self, index: usize, step: usize, now: u64) {
        let key = (self.orbit_notifications.tasks[index].task_id.clone(), step);
        let live = &mut self.orbit_notifications.parallel.live;
        let Some(Live::Capturing(patch)) = live.remove(&key) else {
            // A restart lost the capture; the checkout is still there to take it from.
            self.start_parallel_capture(index, step, now);
            return;
        };
        match patch.try_recv() {
            Ok(Ok(summary)) => self.settle_reported_step(index, step, Some(summary), "", now),
            Ok(Err(error)) => {
                let reason = format!("could not save the patch: {error}");
                self.settle_reported_step(index, step, None, &reason, now);
            }
            Err(TryRecvError::Empty) => {
                let live = &mut self.orbit_notifications.parallel.live;
                live.insert(key, Live::Capturing(patch));
            }
            Err(TryRecvError::Disconnected) => {
                self.settle_reported_step(index, step, None, "saving the patch stopped early", now);
            }
        }
    }

    /// Ends a reported step: done when it completed and its patch is saved.
    fn settle_reported_step(
        &mut self,
        index: usize,
        step: usize,
        patch: Option<crate::parallel_checkout::PatchSummary>,
        problem: &str,
        now: u64,
    ) {
        let problem = problem.to_owned();
        self.edit_parallel_step(index, step, |s| s.finished(patch, problem, now));
    }

    /// The final report of a fanned-out task: the worst outcome of its steps,
    /// each step's summary and patch, and every changed path.
    pub(in crate::app::orbit_notifications) fn aggregate_report(
        &self,
        index: usize,
    ) -> OrbitAgentReportParams {
        aggregate(&self.orbit_notifications.tasks[index])
    }
}

/// The final report for `task`'s parallel steps.
pub(super) fn aggregate(task: &Task) -> OrbitAgentReportParams {
    let steps: Vec<_> = task
        .steps
        .iter()
        .enumerate()
        .filter_map(|(at, step)| Some((at, step, step.parallel.as_ref()?)))
        .collect();
    let outcome = if steps.iter().any(|(_, step, p)| {
        step.state == StepState::Failed && p.outcome != Some(OrbitOutcome::Blocked)
    }) {
        OrbitOutcome::Failed
    } else if steps
        .iter()
        .any(|(_, _, p)| p.outcome == Some(OrbitOutcome::Blocked))
    {
        OrbitOutcome::Blocked
    } else {
        OrbitOutcome::Completed
    };
    let mut lines = vec![format!("Parallel task with {} steps.", steps.len())];
    let mut files: Vec<String> = Vec::new();
    for (at, step, parallel) in &steps {
        let result = match (&parallel.outcome, &parallel.summary) {
            (Some(outcome), Some(summary)) => {
                format!("{outcome:?}: {}", bounded_text(summary, STEP_SUMMARY_CHARS))
            }
            _ => format!(
                "did not finish: {}",
                parallel
                    .log
                    .last()
                    .map_or("no reason recorded", String::as_str)
            ),
        };
        let patch = parallel.patch.as_ref().map_or(String::new(), |patch| {
            files.extend(patch.files.iter().cloned());
            format!(
                " Patch {} ({} files; apply with git apply).",
                patch.path.display(),
                patch.file_count
            )
        });
        let order = parallel
            .log
            .first()
            .map_or(String::new(), |note| format!(" ({note})"));
        lines.push(format!(
            "Step {} ({}){order} {result}.{patch}",
            at + 1,
            step.member
        ));
    }
    let mut summary = lines.join("\n");
    while summary.len() > MAX_REPORT_BYTES {
        summary.pop();
    }
    let last = steps.iter().rev().find_map(|(_, _, p)| p.worker_id.clone());
    let mut report = OrbitAgentReportParams::new(
        task.task_id.clone(),
        last.unwrap_or_else(|| task.worker_id.clone()),
        outcome.clone(),
        summary,
    );
    files.sort();
    files.dedup();
    files.retain(|path| check_path("changed_files", path).is_ok());
    files.truncate(MAX_LISTED_FILES);
    report.changed_files = (!files.is_empty()).then_some(files);
    if outcome == OrbitOutcome::Blocked {
        let blocked = steps
            .iter()
            .find(|(_, _, p)| p.outcome == Some(OrbitOutcome::Blocked));
        let reason = blocked
            .and_then(|(at, _, p)| Some(format!("step {}: {}", at + 1, p.summary.as_deref()?)));
        report.blocked_on = reason.map(|reason| bounded_text(&reason, BLOCKED_ON_CHARS));
    }
    report
}
