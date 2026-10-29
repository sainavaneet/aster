//! Bounded ledger operations: admission, eviction to tombstones, reports, cancel, status.
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::state::{Delivery, ReportOrigin, Submission, Task, Tombstone};
use super::{token, Notifications, CAPACITY, TOMBSTONE_CAPACITY};
use crate::api::schema::OrbitAgentReportParams;

const UNKNOWN_TASK: &str = "Unknown assignment; use the ID supplied by orbit_agent_assign";
/// One message for an unknown task, another worker's task, a server notice, or a
/// wrong or missing token, so a reporter learns nothing about which was wrong.
pub(super) const REPORT_REJECTED: &str = "Assignment not found or token invalid; report with the exact task_id, worker_id, and token from your assignment";

pub(super) const CONFLICTING_REPORT: &str = "Assignment already has a different report";
const CANCELLED: &str =
    "Assignment was cancelled by Polaris; stop this task and do not retry the report";

/// How a repeated report relates to the accepted one.
#[derive(Debug, PartialEq, Eq)]
enum Retry {
    /// Identical, or the original report retried after an enrichment.
    Same,
    /// Same outcome and summary, adding structured fields to a report with none.
    /// Stored, but never delivered again.
    Enrichment,
    Conflict,
}

impl Retry {
    fn classify(previous: &OrbitAgentReportParams, report: &OrbitAgentReportParams) -> Self {
        if previous == report {
            return Self::Same;
        }
        if previous.outcome != report.outcome || previous.summary != report.summary {
            return Self::Conflict;
        }
        match (previous.has_facts(), report.has_facts()) {
            (false, true) => Self::Enrichment,
            (true, false) => Self::Same,
            _ => Self::Conflict,
        }
    }
}

impl Notifications {
    pub(super) fn find(&self, task_id: &str) -> Option<usize> {
        self.tasks.iter().position(|task| task.task_id == task_id)
    }

    /// Mutable access that marks the ledger for persistence.
    pub(super) fn task_mut(&mut self, index: usize) -> &mut Task {
        self.dirty = true;
        &mut self.tasks[index]
    }

    fn tombstone(&self, task_id: &str) -> Option<&Tombstone> {
        self.tombstones
            .iter()
            .find(|entry| entry.task_id == task_id)
    }

    pub(super) fn next_task_id(&mut self) -> String {
        self.sequence += 1;
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("task-{}-{epoch}-{}", std::process::id(), self.sequence)
    }

    /// Checked before the worker is prompted, so a full ledger never orphans an assignment.
    pub(super) fn check_room(&self) -> Result<(), String> {
        if self.tasks.len() < CAPACITY || self.tasks.iter().any(|t| t.delivery.is_terminal()) {
            return Ok(());
        }
        let active = self
            .tasks
            .iter()
            .filter(|task| !task.delivery.is_terminal())
            .count();
        Err(format!(
            "Orbit assignment ledger is full ({CAPACITY}); {active} assignments are still active. Cancel one with orbit_task_cancel or wait for results"
        ))
    }

    /// Evicts the oldest resolved entries into the bounded tombstone ring.
    fn make_room(&mut self) {
        while self.tasks.len() >= CAPACITY {
            let Some(index) = self.tasks.iter().position(|t| t.delivery.is_terminal()) else {
                return;
            };
            let evicted = self.tasks.remove(index);
            tracing::info!(task_id = %evicted.task_id, "orbit task evicted to tombstone");
            if self.tombstones.len() >= TOMBSTONE_CAPACITY {
                self.tombstones.pop_front();
            }
            self.tombstones.push_back(Tombstone::from_task(&evicted));
        }
    }

    /// Server notices count toward `CAPACITY` like assignments. When the ledger
    /// is full: the oldest resolved entry is evicted first; with none, the
    /// oldest server notice still waiting in the queue (not being delivered) is
    /// dropped; with neither, the new notice is refused. Drops and refusals are
    /// logged and counted in `ledger_error`. Returns whether it was stored.
    pub(super) fn insert_notice(&mut self, notice: Task) -> bool {
        self.make_room();
        if self.tasks.len() >= CAPACITY {
            let waiting = self.tasks.iter().position(Task::is_waiting_notice);
            self.dropped_notices += 1;
            let Some(index) = waiting else {
                tracing::warn!(task_id = %notice.task_id, "orbit ledger full of active assignments; server notice refused");
                return false;
            };
            let dropped = self.tasks.remove(index);
            tracing::warn!(task_id = %dropped.task_id, "orbit ledger full; oldest queued server notice dropped");
        }
        self.tasks.push(notice);
        self.dirty = true;
        true
    }

    pub(super) fn insert(&mut self, task: Task) -> Value {
        self.make_room();
        let value = json!(&task);
        self.tasks.push(task);
        self.dirty = true;
        value
    }

    /// Verifies identity and token, then stores the report without its token.
    pub(super) fn accept_report(
        &mut self,
        mut report: OrbitAgentReportParams,
        now_ms: u64,
    ) -> Result<Value, String> {
        let presented = report.token.take();
        let Some(index) = self.find(&report.task_id) else {
            return self.accept_tombstone_report(report, presented.as_deref(), now_ms);
        };
        let task = &self.tasks[index];
        let token_valid = token::accepts(task.token_hash.as_ref(), presented.as_deref());
        if task.worker_id != report.worker_id
            || task.origin == Some(ReportOrigin::Server)
            || !token_valid
        {
            return Err(REPORT_REJECTED.into());
        }
        // The integrator's report is the task's one report: it carries every
        // patch's paths. Merging is deterministic, so retries stay identical.
        let report = super::parallel::merge_report(task, report);
        if let Some(previous) = &task.report {
            return match Retry::classify(previous, &report) {
                Retry::Same => Ok(json!(task)),
                Retry::Enrichment => {
                    let task = self.task_mut(index);
                    tracing::info!(task_id = %task.task_id, "orbit report enriched with facts");
                    task.report = Some(report);
                    task.updated_at = now_ms;
                    Ok(json!(task))
                }
                Retry::Conflict => Err(CONFLICTING_REPORT.into()),
            };
        }
        let late = match task.delivery {
            Delivery::AwaitingReport => None,
            Delivery::Cancelled => return Err(CANCELLED.into()),
            other if other.accepts_late_report() => Some(other),
            other => {
                return Err(format!(
                    "Assignment already resolved as {}; Polaris was notified. Do not retry",
                    other.as_str()
                ))
            }
        };
        let task = self.task_mut(index);
        if matches!(task.submission, Submission::Pending | Submission::Uncertain) {
            task.set_submission(
                Submission::ConfirmedByReport,
                now_ms,
                "matching worker report proves the assignment arrived",
            );
        }
        task.report = Some(report);
        task.close_step(now_ms);
        task.origin = Some(ReportOrigin::Worker);
        task.live.idle_since = None;
        let Some(from) = late else {
            task.set_delivery(Delivery::Queued, now_ms, "worker reported");
            return Ok(json!(task));
        };
        task.late = true;
        task.late_from = Some(from);
        task.error = None;
        task.set_delivery(Delivery::Queued, now_ms, "late worker report");
        tracing::info!(task_id = %task.task_id, from = from.as_str(), "orbit late report accepted");
        // Behind the notice Polaris already got for it, so a shared delivery
        // prompt shows the notice first.
        let task = self.tasks.remove(index);
        let value = json!(&task);
        self.tasks.push(task);
        Ok(value)
    }

    fn accept_tombstone_report(
        &mut self,
        report: OrbitAgentReportParams,
        presented: Option<&str>,
        now_ms: u64,
    ) -> Result<Value, String> {
        let position = self
            .tombstones
            .iter()
            .position(|entry| entry.task_id == report.task_id)
            .filter(|at| self.tombstones[*at].worker_id == report.worker_id)
            .filter(|at| token::accepts(self.tombstones[*at].token_hash.as_ref(), presented))
            .ok_or(REPORT_REJECTED)?;
        let tombstone = &mut self.tombstones[position];
        let retry = match &tombstone.report {
            Some(previous) => Retry::classify(previous, &report),
            None if tombstone.final_delivery.accepts_late_report() => {
                return self.accept_late_tombstone_report(position, report, now_ms);
            }
            None if tombstone.final_delivery == Delivery::Cancelled => {
                return Err(CANCELLED.into());
            }
            None => Retry::Conflict,
        };
        match retry {
            Retry::Same => Ok(json!(tombstone)),
            Retry::Enrichment => {
                tombstone.report = Some(report);
                self.dirty = true;
                Ok(json!(tombstone))
            }
            Retry::Conflict => {
                Err("Assignment already resolved; this report was not accepted".into())
            }
        }
    }

    /// A late report for an evicted assignment Aster resolved itself: back into
    /// the ledger when there is room, else delivered as a standalone server
    /// notice carrying the report. Either way it is delivered once: the task (or
    /// tombstone) keeps the report, so identical retries are recognised. A
    /// ledger full of active work refuses it, and the worker retries later.
    fn accept_late_tombstone_report(
        &mut self,
        position: usize,
        report: OrbitAgentReportParams,
        now_ms: u64,
    ) -> Result<Value, String> {
        let from = self.tombstones[position].final_delivery;
        if self.check_room().is_err() && !self.tasks.iter().any(Task::is_waiting_notice) {
            return Err(format!(
                "Orbit assignment ledger is full ({CAPACITY}) of active assignments; retry the identical report in a minute"
            ));
        }
        self.dirty = true;
        let lead_id = self.lead_terminal_id.clone().unwrap_or_default();
        if self.check_room().is_ok() {
            let tombstone = self
                .tombstones
                .remove(position)
                .expect("tombstone position is valid");
            tracing::info!(task_id = %tombstone.task_id, from = from.as_str(), "orbit late report restores an evicted task");
            let task = Task::late_report(&tombstone, lead_id, report, now_ms);
            return Ok(self.insert(task));
        }
        let tombstone = &mut self.tombstones[position];
        tombstone.report = Some(report.clone());
        let mut notice = Task::late_report(tombstone, lead_id, report, now_ms);
        notice.task_id = format!("{}/late-report", tombstone.task_id);
        notice.notice_for = Some(tombstone.task_id.clone());
        notice.origin = Some(ReportOrigin::Server);
        notice.token_hash = None;
        tracing::info!(task_id = %tombstone.task_id, "orbit late report delivered as a server notice; ledger full");
        let value = json!(&*tombstone);
        self.insert_notice(notice);
        Ok(value)
    }

    pub(super) fn cancel(&mut self, task_id: &str, now_ms: u64) -> Result<Value, String> {
        let Some(index) = self.find(task_id) else {
            return match self.tombstone(task_id) {
                Some(entry) => Err(format!(
                    "Assignment already resolved as {}; nothing to cancel",
                    entry.final_delivery.as_str()
                )),
                None => Err(UNKNOWN_TASK.into()),
            };
        };
        let delivery = self.tasks[index].delivery;
        if delivery == Delivery::Cancelled {
            return Ok(json!(&self.tasks[index]));
        }
        if delivery.is_terminal() {
            return Err(format!(
                "Assignment already resolved as {}; nothing to cancel",
                delivery.as_str()
            ));
        }
        self.mark_cancelled(index, now_ms, "cancelled by lead");
        Ok(json!(&self.tasks[index]))
    }

    fn mark_cancelled(&mut self, index: usize, now_ms: u64, reason: &'static str) {
        let task = self.task_mut(index);
        // The submission ACK keeps reaping so a cancelled task never shows a
        // pending assignment write forever; only its delivery is abandoned.
        // A shared delivery ACK stays with its batch; only Submitting members use it.
        task.live.batch = None;
        task.fail_step(now_ms);
        task.set_delivery(Delivery::Cancelled, now_ms, reason);
    }

    /// IDs of the worker's assignments still waiting for its report. Reports the
    /// worker already made are not open work: they deliver whatever happens to it.
    pub(super) fn open_tasks_for_worker(&self, worker_id: &str) -> Vec<String> {
        self.tasks
            .iter()
            .filter(|task| task.worker_id == worker_id && task.awaits_worker())
            .map(|task| task.task_id.clone())
            .collect()
    }

    /// Whether the worker has an assignment waiting for its report.
    fn worker_has_open_task(&self, worker_id: &str) -> bool {
        self.tasks
            .iter()
            .any(|task| task.worker_id == worker_id && task.awaits_worker())
    }

    /// IDs of reports and notices about the worker still on their way to the lead.
    pub(super) fn pending_deliveries_for_worker(&self, worker_id: &str) -> Vec<String> {
        self.tasks
            .iter()
            .filter(|task| {
                task.worker_id == worker_id
                    && !task.delivery.is_terminal()
                    && task.delivery != Delivery::AwaitingReport
            })
            .map(|task| task.task_id.clone())
            .collect()
    }

    /// Cancels the worker's assignments still awaiting a report when the lead
    /// stopped it and returns their IDs; a later report from that worker is
    /// rejected. Reports already made keep delivering.
    pub(crate) fn cancel_open_tasks_for_worker(
        &mut self,
        worker_id: &str,
        now_ms: u64,
    ) -> Vec<String> {
        let open = self.open_tasks_for_worker(worker_id);
        for task_id in &open {
            if let Some(index) = self.find(task_id) {
                self.mark_cancelled(index, now_ms, "worker stopped by lead");
            }
        }
        open
    }

    pub(super) fn status(&self, task_id: &str) -> Result<Value, String> {
        if let Some(index) = self.find(task_id) {
            return Ok(json!({"task": &self.tasks[index], "tombstone": null}));
        }
        let tombstone = self.tombstone(task_id).ok_or(UNKNOWN_TASK)?;
        Ok(json!({"task": null, "tombstone": tombstone}))
    }
}

impl super::App {
    /// Assignment IDs of an Orbit worker terminal still awaiting its report.
    pub(crate) fn orbit_open_tasks_for_worker(&mut self, worker_id: &str) -> Vec<String> {
        let now = self.orbit_notifications.now_ms(std::time::Instant::now());
        self.ensure_orbit_ledger(now);
        self.orbit_notifications.open_tasks_for_worker(worker_id)
    }

    /// Whether an Orbit worker terminal has an assignment awaiting its report.
    pub(crate) fn orbit_worker_has_open_task(&mut self, worker_id: &str) -> bool {
        let now = self.orbit_notifications.now_ms(std::time::Instant::now());
        self.ensure_orbit_ledger(now);
        self.orbit_notifications.worker_has_open_task(worker_id)
    }

    /// Reports and notices about the worker not yet delivered to the lead.
    pub(crate) fn orbit_pending_deliveries_for_worker(&mut self, worker_id: &str) -> Vec<String> {
        let now = self.orbit_notifications.now_ms(std::time::Instant::now());
        self.ensure_orbit_ledger(now);
        self.orbit_notifications
            .pending_deliveries_for_worker(worker_id)
    }

    /// Cancels and persists the worker's unresolved assignments.
    pub(crate) fn orbit_cancel_worker_tasks(&mut self, worker_id: &str) -> Vec<String> {
        let now = self.orbit_notifications.now_ms(std::time::Instant::now());
        self.ensure_orbit_ledger(now);
        let cancelled = self
            .orbit_notifications
            .cancel_open_tasks_for_worker(worker_id, now);
        self.persist_orbit_ledger();
        cancelled
    }
}
