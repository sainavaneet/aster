//! Ending parallel steps: wait for a step's background work, stop its agent,
//! remove its checkout, and send the task's one report when none is left.
use std::sync::mpsc::{self, TryRecvError};

use serde_json::{json, Value};

use super::drive::error_text;
use super::Live;
use crate::api::schema::{OrbitOutcome, Request};
use crate::app::orbit_notifications::state::Delivery;
use crate::app::App;
use crate::events::ParallelLaunch;
use crate::parallel_checkout::RemoveError;

/// Removals tried before a step stops (a restart tries again). The wait between
/// tries starts at `REMOVAL_RETRY_MS` and doubles up to `MAX_REMOVAL_RETRY_MS`,
/// so a cause that clears in minutes or hours (a busy volume, a held file) is
/// outlasted instead of leaving gigabytes behind.
const MAX_REMOVAL_TRIES: u32 = 12;
const REMOVAL_RETRY_MS: u64 = 5_000;
const MAX_REMOVAL_RETRY_MS: u64 = 30 * 60 * 1_000;

/// The wait before the removal that follows `tries` failed ones.
fn removal_backoff_ms(tries: u32) -> u64 {
    let doublings = tries.saturating_sub(1).min(16);
    (REMOVAL_RETRY_MS << doublings).min(MAX_REMOVAL_RETRY_MS)
}

impl App {
    /// A terminal step: wait for its background work, stop its agent, then
    /// remove its checkout, trying again until the removal is confirmed.
    pub(super) fn settle_parallel_step(&mut self, index: usize, step: usize, now: u64) {
        let task = &self.orbit_notifications.tasks[index];
        if task.steps[step]
            .parallel
            .as_ref()
            .is_none_or(|parallel| parallel.released)
        {
            return;
        }
        let key = (task.task_id.clone(), step);
        let live = &mut self.orbit_notifications.parallel.live;
        match live.remove(&key) {
            Some(Live::Launching(response)) => match response.try_recv() {
                Ok(text) => {
                    let value: Value = serde_json::from_str(&text).unwrap_or_default();
                    if let Some(id) = value["result"]["agent"]["terminal_id"].as_str() {
                        let id = id.to_owned();
                        self.edit_parallel_step(index, step, |s| s.launched(id));
                    }
                }
                Err(TryRecvError::Empty) => {
                    let live = &mut self.orbit_notifications.parallel.live;
                    live.insert(key, Live::Launching(response));
                    return;
                }
                Err(TryRecvError::Disconnected) => {}
            },
            Some(Live::Capturing(patch)) => {
                if let Err(TryRecvError::Empty) = patch.try_recv() {
                    let live = &mut self.orbit_notifications.parallel.live;
                    live.insert(key, Live::Capturing(patch));
                    return;
                }
            }
            Some(applying @ Live::Integrating(_)) => {
                let live = &mut self.orbit_notifications.parallel.live;
                live.insert(key, applying);
                return;
            }
            Some(Live::Removing(removal)) => {
                match removal.try_recv() {
                    Ok(Ok(())) => {
                        self.edit_parallel_step(index, step, |s| s.released());
                    }
                    Err(TryRecvError::Empty) => {
                        let live = &mut self.orbit_notifications.parallel.live;
                        live.insert(key, Live::Removing(removal));
                    }
                    Ok(Err(RemoveError::Refused(error))) => {
                        tracing::warn!(%error, step, "orbit parallel checkout removal refused");
                        self.edit_parallel_step(index, step, |s| s.removal_refused(&error));
                    }
                    Ok(Err(RemoveError::Failed(error))) => {
                        self.removal_failed(index, step, &error, now);
                    }
                    Err(TryRecvError::Disconnected) => {
                        self.removal_failed(index, step, "the removal stopped early", now);
                    }
                }
                return;
            }
            None => {}
        }
        // Read after the launch result above, which may have named the agent.
        let parallel = self.orbit_notifications.tasks[index].steps[step]
            .parallel
            .as_ref();
        let (worker, checkout, tries) = parallel.map_or((None, None, 0), |p| {
            (p.worker_id.clone(), p.checkout.clone(), p.removals)
        });
        if tries == 0 {
            if let Some(worker) = worker {
                self.stop_parallel_worker(&worker);
            }
        }
        let Some(checkout) = checkout else {
            self.edit_parallel_step(index, step, |s| s.released());
            return;
        };
        let retry = self.orbit_notifications.parallel.removal_after.get(&key);
        if retry.is_some_and(|after| now < *after) {
            return;
        }
        let directory = self.state.worktree_directory.clone();
        let (send, removal) = mpsc::channel();
        let spawned = crate::thread_spawn::spawn_named("aster-orbit-checkout-remove", move || {
            let _ = send.send(crate::parallel_checkout::remove(&checkout, &directory));
        });
        self.edit_parallel_step(index, step, |s| s.removal_started());
        match spawned {
            Ok(_) => {
                let live = &mut self.orbit_notifications.parallel.live;
                live.insert(key, Live::Removing(removal));
            }
            Err(error) => self.removal_failed(index, step, &error.to_string(), now),
        }
    }

    /// A removal failed: log it and try again shortly, or give up after a few tries.
    fn removal_failed(&mut self, index: usize, step: usize, error: &str, now: u64) {
        let key = (self.orbit_notifications.tasks[index].task_id.clone(), step);
        tracing::warn!(%error, step, "could not remove a parallel step checkout");
        let gave_up =
            self.edit_parallel_step(index, step, |s| s.removal_failed(error, MAX_REMOVAL_TRIES));
        if gave_up == Some(true) {
            self.orbit_notifications.parallel.removal_after.remove(&key);
        } else {
            let tries = self.orbit_notifications.tasks[index].steps[step]
                .parallel
                .as_ref()
                .map_or(1, |parallel| parallel.removals);
            let removal_after = &mut self.orbit_notifications.parallel.removal_after;
            removal_after.insert(key, now + removal_backoff_ms(tries));
        }
    }

    fn stop_parallel_worker(&mut self, worker_id: &str) {
        let request = serde_json::from_value::<Request>(json!({
            "id": "parallel-stop",
            "method": "orbit.agent_stop",
            "params": {"name": worker_id, "close_pane": true, "force": true},
        }));
        let Ok(request) = request else { return };
        let response: Value =
            serde_json::from_str(&self.handle_api_request_after_internal_events_drained(request))
                .unwrap_or_default();
        if let Some(message) = error_text(&response) {
            // Usually the agent is already gone.
            tracing::info!(worker_id, %message, "orbit parallel agent not stopped");
        }
    }

    /// Keeps a step's checkout once its agent exists, even if the step ends
    /// before the agent starts.
    pub(crate) fn record_parallel_checkout(&mut self, plan: &ParallelLaunch) {
        let Some(checkout) = plan.checkout.clone() else {
            return;
        };
        let Some(index) = self.orbit_notifications.find(&plan.task_id) else {
            // The ledger no longer has the task (evicted, reset or lost): no
            // step will ever release this checkout, so release it here.
            self.remove_unowned_checkout(checkout);
            return;
        };
        self.edit_parallel_step(index, plan.step, |step| step.recorded(checkout));
    }

    /// Removes a checkout no ledger entry owns, off the event loop.
    fn remove_unowned_checkout(&self, checkout: crate::parallel_checkout::Checkout) {
        let directory = self.state.worktree_directory.clone();
        let spawned = crate::thread_spawn::spawn_named("aster-orbit-checkout-remove", move || {
            if let Err(error) = crate::parallel_checkout::remove(&checkout, &directory) {
                tracing::warn!(%error, path = %checkout.path.display(), "could not remove an unowned parallel checkout");
            }
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start removing an unowned parallel checkout");
        }
    }

    /// The session recorded in the markers of the checkouts this server makes:
    /// the directory of its ledger and sockets.
    pub(crate) fn orbit_checkout_owner(&self) -> String {
        crate::session::data_dir().display().to_string()
    }

    /// Removes marked checkouts no ledger entry owns and that are at least
    /// `min_age` old and made by this session, off the event loop (see `parallel_checkout::reclaim_orphans`).
    pub(crate) fn reclaim_orphaned_checkouts(&mut self, min_age: std::time::Duration) {
        let owned: std::collections::HashSet<std::path::PathBuf> = self
            .orbit_notifications
            .tasks
            .iter()
            .flat_map(|task| task.steps.iter())
            .filter_map(|step| step.parallel.as_ref()?.checkout.as_ref())
            .map(|checkout| checkout.path.clone())
            .collect();
        let directory = self.state.worktree_directory.clone();
        let owner = self.orbit_checkout_owner();
        let spawned = crate::thread_spawn::spawn_named("aster-orbit-checkout-reclaim", move || {
            let found =
                crate::parallel_checkout::reclaim_orphans(&directory, &owner, &owned, min_age);
            for item in found {
                match item.result {
                    Ok(()) => {
                        tracing::info!(path = %item.path.display(), "reclaimed an orphaned parallel checkout")
                    }
                    Err(error) => {
                        tracing::warn!(%error, path = %item.path.display(), "could not reclaim an orphaned parallel checkout")
                    }
                }
            }
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start reclaiming orphaned parallel checkouts");
        }
    }

    /// One final report once no step is left to run.
    pub(super) fn finish_fanout_if_done(&mut self, index: usize, now: u64) {
        let task = &self.orbit_notifications.tasks[index];
        if task.delivery != Delivery::AwaitingReport || task.has_unfinished_steps() {
            return;
        }
        // The role's integrator takes it from here; without one (or with
        // nothing to integrate) the task reports its steps directly.
        if self.begin_integration(index, now) {
            return;
        }
        let report = self.aggregate_report(index);
        let task = self.orbit_notifications.task_mut(index);
        task.worker_id = report.worker_id.clone();
        let outcome = report.outcome.clone();
        task.report = Some(report);
        task.origin = Some(crate::app::orbit_notifications::state::ReportOrigin::Worker);
        task.error =
            (outcome != OrbitOutcome::Completed).then(|| "a parallel step did not complete".into());
        task.live = Default::default();
        task.set_delivery(Delivery::Queued, now, "parallel steps finished");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_backoff_doubles_from_five_seconds_and_is_capped() {
        let waits: Vec<u64> = (1..=MAX_REMOVAL_TRIES).map(removal_backoff_ms).collect();
        assert_eq!(waits[..4], [5_000, 10_000, 20_000, 40_000]);
        assert!(waits.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(waits[MAX_REMOVAL_TRIES as usize - 1], 1_800_000);
        assert_eq!(removal_backoff_ms(u32::MAX), 1_800_000);
        assert_eq!(MAX_REMOVAL_TRIES, 12);
    }
}
