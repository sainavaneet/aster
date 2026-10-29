//! Human-facing Orbit notifications. Ledger transitions record an event; the
//! 250 ms ledger tick names the worker, publishes an `orbit.notice` API event,
//! and queues a short notice that the server forwards to attached clients, which
//! apply `ui.toast.delivery`. Text names the worker and task only: never a
//! report summary, prompt text, or token.
use std::collections::{HashSet, VecDeque};

use super::state::{Delivery, ReportOrigin};
use super::{App, Notifications};
use crate::api::schema::{EventData, EventEnvelope, EventKind};
use crate::config::{OrbitNotifyEvent, ToastDelivery};

/// Events waiting for the next tick; more are dropped rather than queued.
const MAX_PENDING: usize = 64;
/// Notices not yet taken by the server; the oldest is dropped first.
const MAX_OUTBOX: usize = 16;
/// Coalescing memory of (event, task) pairs already notified.
const MAX_SEEN: usize = 512;
const MAX_TITLE_CHARS: usize = 80;
const MAX_BODY_CHARS: usize = 240;
const MAX_NAME_CHARS: usize = 32;
/// Detail of a blocked event for a worker with no assignment.
const UNASSIGNED: &str = "no assignment";

/// One notification ready for clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HumanNotice {
    pub(crate) event: OrbitNotifyEvent,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) workspace_id: Option<String>,
    pub(crate) tab_id: Option<String>,
    pub(crate) pane_id: Option<String>,
}

/// Recorded by ledger code; names are resolved on the tick.
#[derive(Debug, Clone)]
struct Event {
    kind: OrbitNotifyEvent,
    task_id: String,
    worker_id: String,
    detail: &'static str,
}

pub(super) struct Notifier {
    enabled: HashSet<OrbitNotifyEvent>,
    pending: Vec<Event>,
    seen: HashSet<(OrbitNotifyEvent, String)>,
    seen_order: VecDeque<(OrbitNotifyEvent, String)>,
    outbox: VecDeque<HumanNotice>,
}

impl Default for Notifier {
    fn default() -> Self {
        Self {
            enabled: OrbitNotifyEvent::ALL.into_iter().collect(),
            pending: Vec::new(),
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            outbox: VecDeque::new(),
        }
    }
}

impl Notifier {
    pub(super) fn set_events(&mut self, events: &[OrbitNotifyEvent]) {
        self.enabled = events.iter().copied().collect();
    }

    /// Records `kind` for `task_id` once; later events of that kind for the task
    /// are coalesced into the first.
    pub(super) fn record(
        &mut self,
        kind: OrbitNotifyEvent,
        task_id: &str,
        worker_id: &str,
        detail: &'static str,
    ) {
        self.record_keyed(kind, task_id.to_owned(), task_id, worker_id, detail);
    }

    /// Records a blocked episode of `task_id` that began at `since_ms` once, so
    /// each separate blocked stretch notifies the human once.
    pub(super) fn record_blocked(&mut self, task_id: &str, worker_id: &str, since_ms: u64) {
        let key = format!("{task_id}@{since_ms}");
        self.record_keyed(OrbitNotifyEvent::Blocked, key, task_id, worker_id, "");
    }

    /// Records a blocked episode of a worker with no assignment once; `notice_id`
    /// names the episode's standalone notice.
    pub(super) fn record_blocked_unassigned(
        &mut self,
        notice_id: &str,
        worker_id: &str,
        since_ms: u64,
    ) {
        let key = format!("{notice_id}@{since_ms}");
        self.record_keyed(
            OrbitNotifyEvent::Blocked,
            key,
            notice_id,
            worker_id,
            UNASSIGNED,
        );
    }

    fn record_keyed(
        &mut self,
        kind: OrbitNotifyEvent,
        coalesce: String,
        task_id: &str,
        worker_id: &str,
        detail: &'static str,
    ) {
        if !self.enabled.contains(&kind) || self.pending.len() >= MAX_PENDING {
            return;
        }
        let key = (kind, coalesce);
        if !self.seen.insert(key.clone()) {
            return;
        }
        self.seen_order.push_back(key);
        if self.seen_order.len() > MAX_SEEN {
            if let Some(oldest) = self.seen_order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.pending.push(Event {
            kind,
            task_id: task_id.to_owned(),
            worker_id: worker_id.to_owned(),
            detail,
        });
    }

    fn push(&mut self, notice: HumanNotice) {
        if self.outbox.len() == MAX_OUTBOX {
            self.outbox.pop_front();
        }
        self.outbox.push_back(notice);
    }
}

impl Notifications {
    /// Notes a finished delivery attempt: a worker report reaching the lead, or
    /// any delivery that failed.
    pub(super) fn note_delivery(&mut self, index: usize) {
        let task = &self.tasks[index];
        let kind = match task.delivery {
            Delivery::Submitted if task.origin == Some(ReportOrigin::Worker) => {
                OrbitNotifyEvent::Report
            }
            Delivery::Failed => OrbitNotifyEvent::Failed,
            _ => return,
        };
        let (task_id, worker_id) = (task.task_id.clone(), task.worker_id.clone());
        self.notifier.record(kind, &task_id, &worker_id, "");
    }
}

/// Control characters removed and length bounded with an ellipsis.
pub(super) fn bounded_text(text: &str, max_chars: usize) -> String {
    let mut chars = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch });
    let mut bounded: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() && max_chars > 0 {
        bounded.pop();
        bounded.push('…');
    }
    bounded.trim().to_owned()
}

fn notice_text(event: &Event, name: &str, pane: Option<&str>) -> (String, String) {
    let task = &event.task_id;
    match event.kind {
        OrbitNotifyEvent::Report => (
            format!("Orbit report from {name}"),
            format!("Task {task} was delivered to Polaris."),
        ),
        OrbitNotifyEvent::Notice if task.starts_with(super::team::TEAM_KEY_PREFIX) => {
            (format!("Orbit team: {name}"), format!("{}.", event.detail))
        }
        OrbitNotifyEvent::Notice => (
            format!("Orbit notice for {name}"),
            format!("Task {task}: {}. Polaris was told.", event.detail),
        ),
        OrbitNotifyEvent::Blocked if event.detail == UNASSIGNED => (
            format!("Orbit worker {name} is blocked"),
            format!(
                "Waits on a dialog in pane {} with no assignment. Polaris was told.",
                pane.unwrap_or("unknown")
            ),
        ),
        OrbitNotifyEvent::Blocked => (
            format!("Orbit worker {name} is blocked"),
            format!(
                "Task {task} waits on a prompt in pane {}.",
                pane.unwrap_or("unknown")
            ),
        ),
        OrbitNotifyEvent::Failed => (
            "Orbit delivery failed".to_owned(),
            format!("Task {task} from {name} could not be delivered to Polaris."),
        ),
    }
}

/// The worker's display name and pane, for notices and activity.
pub(super) struct WorkerLabel {
    pub(super) name: String,
    pub(super) pane_id: Option<String>,
    pub(super) workspace_id: Option<String>,
    pub(super) tab_id: Option<String>,
}

impl App {
    pub(super) fn orbit_worker_label(&self, terminal_id: &str) -> WorkerLabel {
        let info = self
            .resolve_terminal_target(terminal_id)
            .ok()
            .filter(|target| target.terminal_id == terminal_id)
            .and_then(|target| self.agent_info(target.ws_idx, target.pane_id));
        let name = info
            .as_ref()
            .and_then(|info| info.name.clone())
            .unwrap_or_else(|| terminal_id.to_owned());
        WorkerLabel {
            name: bounded_text(&name, MAX_NAME_CHARS),
            pane_id: info.as_ref().map(|info| info.pane_id.clone()),
            workspace_id: info.as_ref().map(|info| info.workspace_id.clone()),
            tab_id: info.map(|info| info.tab_id),
        }
    }

    /// Turns recorded events into `orbit.notice` API events and client notices;
    /// no client notice is raised while `ui.toast.delivery` is `off`.
    pub(super) fn flush_orbit_notices(&mut self) {
        if self.orbit_notifications.notifier.pending.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.orbit_notifications.notifier.pending);
        let toasts = self.state.toast_config.delivery != ToastDelivery::Off;
        for event in pending {
            let label = self.orbit_worker_label(&event.worker_id);
            let (title, body) = notice_text(&event, &label.name, label.pane_id.as_deref());
            let body = bounded_text(&body, MAX_BODY_CHARS);
            tracing::info!(task_id = %event.task_id, kind = ?event.kind, "orbit notification raised");
            self.emit_event(EventEnvelope {
                event: EventKind::OrbitNotice,
                data: EventData::OrbitNotice {
                    kind: event.kind.as_str().to_owned(),
                    worker: label.name.clone(),
                    task_id: event.task_id.clone(),
                    pane_id: label.pane_id.clone(),
                    message: body.clone(),
                },
            });
            if !toasts {
                continue;
            }
            self.orbit_notifications.notifier.push(HumanNotice {
                event: event.kind,
                title: bounded_text(&title, MAX_TITLE_CHARS),
                body,
                workspace_id: label.workspace_id,
                tab_id: label.tab_id,
                pane_id: label.pane_id,
            });
        }
    }

    /// Notices for the server to forward to attached clients.
    pub(crate) fn take_orbit_human_notices(&mut self) -> Vec<HumanNotice> {
        self.orbit_notifications.notifier.outbox.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_coalesces_per_task_and_kind_and_respects_enabled_events() {
        let mut notifier = Notifier::default();
        notifier.record(OrbitNotifyEvent::Report, "t1", "w", "");
        notifier.record(OrbitNotifyEvent::Report, "t1", "w", "");
        notifier.record(OrbitNotifyEvent::Failed, "t1", "w", "");
        notifier.record(OrbitNotifyEvent::Report, "t2", "w", "");
        assert_eq!(notifier.pending.len(), 3);
        notifier.set_events(&[OrbitNotifyEvent::Blocked]);
        notifier.record(OrbitNotifyEvent::Report, "t3", "w", "");
        notifier.record(OrbitNotifyEvent::Blocked, "t3", "w", "");
        let kinds: Vec<_> = notifier.pending.iter().map(|event| event.kind).collect();
        assert_eq!(kinds.last(), Some(&OrbitNotifyEvent::Blocked));
        assert_eq!(kinds.len(), 4);
    }

    #[test]
    fn blocked_notifications_coalesce_per_episode_not_per_task() {
        let mut notifier = Notifier::default();
        notifier.record_blocked("t1", "w", 1_000);
        notifier.record_blocked("t1", "w", 1_000);
        notifier.record_blocked("t1", "w", 90_000);
        notifier.record_blocked("t2", "w", 1_000);
        assert_eq!(notifier.pending.len(), 3);
        assert!(notifier
            .pending
            .iter()
            .all(|event| event.kind == OrbitNotifyEvent::Blocked));
        assert_eq!(notifier.pending[1].task_id, "t1");
    }

    #[test]
    fn notice_text_names_worker_and_task_only() {
        let event = Event {
            kind: OrbitNotifyEvent::Notice,
            task_id: "task-9".into(),
            worker_id: "term".into(),
            detail: "worker lost",
        };
        let (title, body) = notice_text(&event, "reviewer", Some("w1:p2"));
        assert_eq!(title, "Orbit notice for reviewer");
        assert_eq!(body, "Task task-9: worker lost. Polaris was told.");
        assert_eq!(bounded_text("a\u{1b}[2Jb", 80), "a [2Jb");
        assert_eq!(bounded_text("abcdef", 4), "abc…");
    }
}
