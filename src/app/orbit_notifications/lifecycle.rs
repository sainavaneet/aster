//! Worker lifecycle for unreported assignments: restart reconciliation, lost workers,
//! timeouts, a single idle reminder before the lead is told the task went unreported,
//! and a relay to the lead when a worker stays blocked on a prompt, with or without
//! an assignment.
use std::time::Duration;

use serde_json::{json, Value};

use super::assign::{report_command, REPORT_FIELDS_HINT, REPORT_HOW};
use super::notify::bounded_text;
use super::redact::redact_secrets;
use super::state::{Delivery, LiveState, ReportOrigin, Submission, Task};
use super::App;
use crate::api::schema::{
    AgentPromptParams, OrbitAgentReportParams, OrbitOutcome, ReadFormat, ReadSource,
};
use crate::config::OrbitNotifyEvent;
use crate::detect::AgentState;

/// Detection and runtimes may lag a restored session; lifecycle checks wait this long.
pub(super) const STARTUP_GRACE_MS: u64 = 15_000;
/// Continuous worker idleness without a report before one reminder, and again before
/// the assignment is declared unreported.
pub(super) const NUDGE_AFTER: Duration = Duration::from_secs(5 * 60);
const NUDGE_AFTER_MS: u64 = NUDGE_AFTER.as_millis() as u64;
const NUDGE_RETRY_MS: u64 = 5_000;
/// Continuous `Blocked` before Polaris receives one notice for the episode and the
/// user is notified. Leaving `Blocked` ends the episode.
pub(crate) const BLOCKED_NOTICE_MS: u64 = 30_000;
/// Blocked-episode tracking runs on the ledger's 250 ms cadence, also while idle.
const BLOCKED_TICK_MS: u64 = 250;
/// Detection lines read once per relayed episode, and the bottom lines searched.
const PROMPT_READ_LINES: u32 = 40;
const PROMPT_SCAN_LINES: usize = 15;
/// Option label lines kept after the prompt's question line.
const PROMPT_OPTION_LINES: usize = 3;
const PROMPT_EXCERPT_CHARS: usize = 200;
const BORDER_CHARS: &[char] = &[
    '│', '┃', '|', '╭', '╮', '╰', '╯', '─', '━', '┌', '┐', '└', '┘',
];

/// One continuous `Blocked` stretch of an agent terminal.
#[derive(Debug, Clone, Copy)]
pub(super) struct BlockedEpisode {
    pub(super) since: u64,
    relayed: bool,
}

enum WorkerFate {
    Resolve {
        to: Delivery,
        summary: String,
        reason: String,
    },
    Nudge,
}

fn awaits_worker(task: &Task) -> bool {
    task.awaits_worker()
}

fn server_notice(
    task: &Task,
    task_id: String,
    outcome: OrbitOutcome,
    summary: String,
    now: u64,
) -> Task {
    Task {
        task_id,
        worker_id: task.worker_id.clone(),
        lead_id: task.lead_id.clone(),
        submission: task.submission,
        delivery: Delivery::Queued,
        report: Some(OrbitAgentReportParams::new(
            task.task_id.clone(),
            task.worker_id.clone(),
            outcome,
            summary,
        )),
        origin: Some(ReportOrigin::Server),
        notice_for: Some(task.task_id.clone()),
        error: None,
        created_at: now,
        updated_at: now,
        title: None,
        attempts: 0,
        recoveries: 0,
        redelivered: false,
        nudged_at: None,
        deadline: None,
        late: false,
        late_from: None,
        extra: Default::default(),
        token_hash: None,
        live: LiveState::default(),
    }
}

/// A standalone server notice about a worker with no assignment: `notice_for`
/// is `None` and the report names the notice itself.
pub(super) fn worker_notice(
    notice_id: String,
    worker_id: &str,
    lead_id: String,
    summary: String,
    now: u64,
) -> Task {
    Task {
        task_id: notice_id.clone(),
        worker_id: worker_id.into(),
        lead_id,
        // Nothing is written to the worker, so there is nothing to confirm.
        submission: Submission::Submitted,
        delivery: Delivery::Queued,
        report: Some(OrbitAgentReportParams::new(
            notice_id,
            worker_id.into(),
            OrbitOutcome::Blocked,
            summary,
        )),
        origin: Some(ReportOrigin::Server),
        notice_for: None,
        error: None,
        created_at: now,
        updated_at: now,
        title: None,
        attempts: 0,
        recoveries: 0,
        redelivered: false,
        nudged_at: None,
        deadline: None,
        late: false,
        late_from: None,
        extra: Default::default(),
        token_hash: None,
        live: LiveState::default(),
    }
}

fn nudge_text(task_id: &str, command: &serde_json::Value) -> String {
    format!("[Aster assignment {task_id} reminder]\nYou appear idle and have not reported this assignment. If it is finished, failed, or blocked, report now using your shell tool, replacing outcome and summary. {REPORT_HOW} Do not merely print the command. {REPORT_FIELDS_HINT}\n{command}\n")
}

/// The short reason a human notification gives for a server notice.
fn notice_detail(to: Delivery) -> &'static str {
    match to {
        Delivery::WorkerLost => "worker lost",
        Delivery::Unreported => "worker did not report",
        Delivery::TimedOut => "assignment timed out",
        _ => "assignment resolved",
    }
}

fn clean_line(line: &str) -> String {
    let text: String = line.chars().filter(|ch| !ch.is_control()).collect();
    text.trim_matches(|ch: char| ch.is_whitespace() || BORDER_CHARS.contains(&ch))
        .to_owned()
}

/// An option of a numbered choice list, optionally behind a cursor marker:
/// `❯ 1. Yes`, `2. No, and tell Codex what to do differently (esc)`.
fn is_option_line(line: &str) -> bool {
    let line = line
        .trim_start_matches(['❯', '›', '>', '▶', '→'])
        .trim_start();
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && line[digits..].starts_with(['.', ')'])
}

/// The prompt's question line and the option labels under it, from the bottom of
/// a detection snapshot: command bodies and other lines are dropped, control
/// characters stripped, secrets redacted (before lines are chosen, so a wrapped
/// token is still recognised), and the result bounded.
pub(super) fn prompt_excerpt(screen: &str) -> Option<String> {
    let lines: Vec<String> = screen
        .lines()
        .map(clean_line)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = redact_secrets(&lines[lines.len().saturating_sub(PROMPT_SCAN_LINES)..].join("\n"));
    let tail: Vec<&str> = tail.lines().collect();
    let question = tail.iter().rposition(|line| line.ends_with('?'))?;
    let options = tail[question + 1..]
        .iter()
        .copied()
        .filter(|line| is_option_line(line))
        .take(PROMPT_OPTION_LINES);
    let excerpt = std::iter::once(tail[question])
        .chain(options)
        .collect::<Vec<_>>()
        .join(" / ");
    Some(bounded_text(&excerpt, PROMPT_EXCERPT_CHARS))
}

fn lost(worker_id: &str, reason: &str) -> WorkerFate {
    WorkerFate::Resolve {
        to: Delivery::WorkerLost,
        summary: format!("Worker {worker_id} was lost before reporting"),
        reason: reason.into(),
    }
}

impl App {
    /// ACK receivers do not survive a restart: unacknowledged writes become uncertain.
    pub(super) fn reconcile_loaded_orbit_ledger(&mut self, now: u64) {
        let mut missing = Vec::new();
        for index in 0..self.orbit_notifications.tasks.len() {
            let worker_exists = self
                .state
                .terminals
                .contains_key(self.orbit_notifications.tasks[index].worker_id.as_str());
            let task = self.orbit_notifications.task_mut(index);
            if task.submission == Submission::Pending {
                task.set_submission(Submission::Uncertain, now, "server restarted before ACK");
            }
            match task.delivery {
                Delivery::Submitting => {
                    task.live.uncertain_since = Some(now);
                    task.set_delivery(Delivery::Uncertain, now, "server restarted before ACK");
                }
                Delivery::Uncertain => task.live.uncertain_since = Some(now),
                Delivery::AwaitingReport if awaits_worker(task) && !worker_exists => {
                    missing.push((task.task_id.clone(), task.worker_id.clone()));
                }
                _ => {}
            }
        }
        for (task_id, worker_id) in missing {
            let fate = lost(&worker_id, "worker terminal missing after restart");
            self.apply_worker_fate(&task_id, fate, now);
        }
    }

    /// `Ok(idle)` while the worker can still report; `Err(reason)` once it cannot.
    fn observe_worker(&self, worker_id: &str) -> Result<bool, &'static str> {
        let Some((key, terminal)) = self.state.terminals.get_key_value(worker_id) else {
            return Err("worker terminal no longer exists");
        };
        if self.terminal_runtimes.get(key).is_none() {
            return Err("worker terminal runtime is gone");
        }
        if !terminal.is_agent_terminal() && terminal.managed_agent_kind().is_none() {
            // A roster member is relaunched in the same terminal; until the
            // supervisor gives up, its assignment is still open.
            if self.orbit_team_member_recovering(worker_id) {
                return Ok(false);
            }
            return Err("worker terminal no longer hosts an agent");
        }
        Ok(terminal.state == AgentState::Idle)
    }

    fn worker_fate(&mut self, index: usize, now: u64) -> Option<WorkerFate> {
        let observed = self.observe_worker(&self.orbit_notifications.tasks[index].worker_id);
        let task = &mut self.orbit_notifications.tasks[index];
        let idle = match observed {
            Err(reason) => return Some(lost(&task.worker_id, reason)),
            Ok(idle) => idle,
        };
        if task.deadline.is_some_and(|deadline| now >= deadline) {
            return Some(WorkerFate::Resolve {
                to: Delivery::TimedOut,
                summary: format!(
                    "Worker {} did not report before the assignment timeout",
                    task.worker_id
                ),
                reason: "assignment deadline passed".into(),
            });
        }
        if !idle {
            task.live.idle_since = None;
            return None;
        }
        let since = *task.live.idle_since.get_or_insert(now);
        if now.saturating_sub(since) < NUDGE_AFTER_MS {
            return None;
        }
        match task.nudged_at {
            None if now >= task.live.retry_at => Some(WorkerFate::Nudge),
            None => None,
            Some(_) => Some(WorkerFate::Resolve {
                to: Delivery::Unreported,
                summary: format!(
                    "Worker {} stayed idle without reporting after a reminder",
                    task.worker_id
                ),
                reason: "idle without report after reminder".into(),
            }),
        }
    }

    pub(super) fn check_orbit_workers(&mut self, now: u64) {
        if now < self.orbit_notifications.lifecycle_after {
            return;
        }
        let mut fates = Vec::new();
        for index in 0..self.orbit_notifications.tasks.len() {
            if !awaits_worker(&self.orbit_notifications.tasks[index]) {
                continue;
            }
            if let Some(fate) = self.worker_fate(index, now) {
                fates.push((self.orbit_notifications.tasks[index].task_id.clone(), fate));
            }
        }
        for (task_id, fate) in fates {
            self.apply_worker_fate(&task_id, fate, now);
        }
    }

    fn apply_worker_fate(&mut self, task_id: &str, fate: WorkerFate, now: u64) {
        match fate {
            WorkerFate::Nudge => self.nudge_orbit_worker(task_id, now),
            WorkerFate::Resolve {
                to,
                summary,
                reason,
            } => self.resolve_unreported_assignment(task_id, to, summary, &reason, now),
        }
    }

    /// Terminal state for the assignment plus a server-originated notice for the lead,
    /// delivered through the same queue and states as worker reports.
    fn resolve_unreported_assignment(
        &mut self,
        task_id: &str,
        to: Delivery,
        summary: String,
        reason: &str,
        now: u64,
    ) {
        let Some(index) = self.orbit_notifications.find(task_id) else {
            return;
        };
        let task = self.orbit_notifications.task_mut(index);
        if !task.set_delivery(to, now, reason) {
            return;
        }
        task.error = Some(reason.into());
        task.live = LiveState::default();
        let notice_id = format!("{}/server-notice", task.task_id);
        let notice = server_notice(task, notice_id, OrbitOutcome::Failed, summary, now);
        let worker_id = task.worker_id.clone();
        let notifications = &mut self.orbit_notifications;
        notifications.insert_notice(notice);
        notifications.notifier.record(
            OrbitNotifyEvent::Notice,
            task_id,
            &worker_id,
            notice_detail(to),
        );
    }

    /// Tracks continuous `Blocked` for workers with an open assignment and for
    /// workers Orbit started (its worker records, assigned or not), and relays
    /// each episode that reaches `BLOCKED_NOTICE_MS` once. Runs every ledger tick,
    /// so it returns before any work while Orbit is unbound or nothing is watched,
    /// and allocates only when an episode starts or is relayed.
    pub(super) fn observe_orbit_blocked(&mut self, now: u64) {
        let unbound = self.orbit_binding.is_none();
        let records = &self.orbit_workers;
        let notifications = &mut self.orbit_notifications;
        if now < notifications.blocked_next_tick {
            return;
        }
        notifications.blocked_next_tick = now + BLOCKED_TICK_MS;
        if unbound || (records.is_empty() && !notifications.tasks.iter().any(awaits_worker)) {
            notifications.blocked.clear();
            return;
        }
        let terminals = &self.state.terminals;
        let blocked = |id: &str| {
            terminals.get(id).is_some_and(|terminal| {
                terminal.state == AgentState::Blocked && terminal.is_agent_terminal()
            })
        };
        let tasks = &notifications.tasks;
        let watched = |id: &str| {
            records.get(id).is_some_and(|record| !record.exited)
                || tasks
                    .iter()
                    .any(|task| awaits_worker(task) && task.worker_id == id)
        };
        notifications
            .blocked
            .retain(|id, _| blocked(id) && watched(id));
        let assigned = tasks
            .iter()
            .filter(|task| awaits_worker(task))
            .map(|task| &task.worker_id);
        let started = records
            .iter()
            .filter(|(_, record)| !record.exited)
            .map(|(id, _)| id);
        for id in assigned.chain(started) {
            if blocked(id) && !notifications.blocked.contains_key(id) {
                notifications.blocked.insert(
                    id.clone(),
                    BlockedEpisode {
                        since: now,
                        relayed: false,
                    },
                );
            }
        }
        if now < notifications.lifecycle_after {
            return;
        }
        let due: Vec<(String, u64)> = notifications
            .blocked
            .iter_mut()
            .filter(|(_, episode)| {
                !episode.relayed && now.saturating_sub(episode.since) >= BLOCKED_NOTICE_MS
            })
            .map(|(id, episode)| {
                episode.relayed = true;
                (id.clone(), episode.since)
            })
            .collect();
        for (worker_id, since) in due {
            self.relay_blocked_worker(&worker_id, since, now);
        }
    }

    /// The worker's most recent open assignment, if any.
    fn open_assignment(&self, worker_id: &str) -> Option<usize> {
        self.orbit_notifications
            .tasks
            .iter()
            .rposition(|task| awaits_worker(task) && task.worker_id == worker_id)
    }

    /// The task id of the worker's most recent open assignment.
    pub(crate) fn orbit_open_task_id(&self, worker_id: &str) -> Option<String> {
        self.open_assignment(worker_id)
            .map(|index| self.orbit_notifications.tasks[index].task_id.clone())
    }

    fn visible_prompt_excerpt(&self, worker_id: &str) -> Option<String> {
        let target = self.resolve_terminal_target(worker_id).ok()?;
        let (runtime, _) = self.lookup_runtime(target.ws_idx, target.pane_id)?;
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            runtime,
            ReadSource::Detection,
            ReadFormat::Text,
            Some(PROMPT_READ_LINES),
        );
        prompt_excerpt(&snapshot.text)
    }

    fn relay_blocked_worker(&mut self, worker_id: &str, since: u64, now: u64) {
        let Some(index) = self.open_assignment(worker_id) else {
            self.relay_unassigned_blocked_worker(worker_id, since, now);
            return;
        };
        let label = self.orbit_worker_label(worker_id);
        let excerpt = self.visible_prompt_excerpt(worker_id);
        let task = &self.orbit_notifications.tasks[index];
        let task_id = task.task_id.clone();
        let pane = label.pane_id.as_deref().unwrap_or("unknown pane");
        let mut summary = format!(
            "Worker {} ({pane}) is blocked on an approval prompt for task {task_id}. The user can answer in its pane; use orbit_agent_approve/deny only if the lead is allowed to.",
            label.name
        );
        if let Some(excerpt) = excerpt {
            summary.push_str("\nVisible prompt: ");
            summary.push_str(&excerpt);
        }
        let notice_id = format!("{task_id}/blocked-{now}");
        let notice = server_notice(task, notice_id, OrbitOutcome::Blocked, summary, now);
        tracing::info!(
            task_id,
            worker_id,
            "orbit worker blocked; notifying Polaris"
        );
        let notifications = &mut self.orbit_notifications;
        notifications.insert_notice(notice);
        notifications
            .notifier
            .record_blocked(&task_id, worker_id, since);
    }

    /// A worker Orbit started, blocked with no assignment (a startup dialog such
    /// as folder trust, or a prompt after its work): one standalone notice to the
    /// lead and one human notification for the episode.
    fn relay_unassigned_blocked_worker(&mut self, worker_id: &str, since: u64, now: u64) {
        if !self.orbit_workers.contains_key(worker_id)
            || self.orbit_is_lead_terminal(worker_id)
            || !self.orbit_terminal_in_scope(worker_id)
        {
            return;
        }
        let notice_id = format!("worker-{worker_id}-{since}/server-notice");
        let lead_id = self.orbit_notice_lead_id();
        tracing::info!(
            notice_id,
            worker_id,
            "orbit worker blocked without an assignment; notifying Polaris"
        );
        if let Some(lead_id) = lead_id {
            let label = self.orbit_worker_label(worker_id);
            let excerpt = self
                .visible_prompt_excerpt(worker_id)
                .unwrap_or_else(|| "no prompt text recognised".into());
            let pane = label.pane_id.as_deref().unwrap_or("unknown pane");
            let summary = format!(
                "Worker {} ({pane}) is blocked on a dialog and has no assignment: {excerpt}. The user can answer in its pane; use orbit_agent_approve/deny only if the lead is allowed to and it is a permission prompt.",
                label.name
            );
            let notice = worker_notice(notice_id.clone(), worker_id, lead_id, summary, now);
            self.orbit_notifications.insert_notice(notice);
        }
        self.orbit_notifications
            .notifier
            .record_blocked_unassigned(&notice_id, worker_id, since);
    }

    /// Blocked agents in the bound Orbit tab other than the lead, oldest episode
    /// first. `since_ms` is known for workers the ledger tick tracks: those with
    /// an open assignment and those Orbit started; others sort last.
    pub(super) fn orbit_blocked_workers(&mut self) -> Vec<Value> {
        let mut rows: Vec<(Option<u64>, String)> = self
            .orbit_tab_terminals()
            .into_iter()
            .map(|(id, _, _)| id)
            .filter(|id| {
                !self.orbit_is_lead_terminal(id)
                    && self
                        .state
                        .terminals
                        .get(id.as_str())
                        .is_some_and(|terminal| {
                            terminal.state == AgentState::Blocked && terminal.is_agent_terminal()
                        })
            })
            .map(|id| {
                let since = self.orbit_notifications.blocked.get(&id).map(|e| e.since);
                (since, id)
            })
            .collect();
        rows.sort_by_key(|(since, _)| since.unwrap_or(u64::MAX));
        rows.into_iter()
            .map(|(since, worker_id)| {
                let label = self.orbit_worker_label(&worker_id);
                json!({
                    "name": label.name,
                    "pane_id": label.pane_id,
                    "task_id": self.orbit_open_task_id(&worker_id),
                    "since_ms": since,
                })
            })
            .collect()
    }

    fn nudge_orbit_worker(&mut self, task_id: &str, now: u64) {
        let Some(index) = self.orbit_notifications.find(task_id) else {
            return;
        };
        let worker_id = self.orbit_notifications.tasks[index].worker_id.clone();
        let Some(target) = self.idle_orbit_target(&worker_id, now) else {
            return;
        };
        let command = match report_command(task_id, &worker_id) {
            Ok(command) => command,
            Err(error) => {
                let task = self.orbit_notifications.task_mut(index);
                task.nudged_at = Some(now);
                task.error = Some(error);
                return;
            }
        };
        let prompt = AgentPromptParams {
            target,
            text: nudge_text(task_id, &command),
            wait: None,
        };
        let result = self.queue_orbit_prompt(task_id.into(), &prompt);
        let task = self.orbit_notifications.task_mut(index);
        match result {
            Ok(_ack) => {
                tracing::info!(task_id, "orbit worker reminded to report");
                task.nudged_at = Some(now);
                task.live.idle_since = None;
            }
            Err(error) if error.is_retryable() => {
                task.live.retry_at = now + NUDGE_RETRY_MS;
                task.error = Some(error.to_string());
            }
            Err(error) => {
                task.nudged_at = Some(now);
                task.error = Some(error.to_string());
            }
        }
    }
}
