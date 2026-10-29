//! Lead binding: the Polaris terminal id is persisted in the ledger and resolved
//! before the `polaris` name, so renaming the lead never breaks Orbit.
use std::time::Instant;

use serde_json::{json, Value};

use super::state::Delivery;
use super::App;
use crate::app::terminal_targets::TerminalTarget;
use crate::detect::AgentState;

const LEAD_NAME: &str = crate::app::polaris::NAME;

pub(super) fn state_name(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Unknown => "unknown",
    }
}

impl App {
    /// Records the lead terminal, for example when Polaris is started.
    pub(crate) fn bind_orbit_lead(&mut self, terminal_id: String) {
        let now = self.orbit_notifications.now_ms(Instant::now());
        self.ensure_orbit_ledger(now);
        self.record_orbit_lead(terminal_id);
        self.persist_orbit_ledger();
    }

    fn record_orbit_lead(&mut self, terminal_id: String) {
        let notifications = &mut self.orbit_notifications;
        if notifications.lead_terminal_id.as_deref() == Some(terminal_id.as_str()) {
            return;
        }
        tracing::info!(%terminal_id, "orbit lead bound");
        notifications.lead_terminal_id = Some(terminal_id);
        notifications.dirty = true;
    }

    fn bound_lead_target(&self) -> Option<TerminalTarget> {
        let bound = self.orbit_notifications.lead_terminal_id.as_deref()?;
        self.resolve_terminal_target(bound)
            .ok()
            .filter(|target| target.terminal_id == bound)
    }

    /// The one answer to "which terminal is the Orbit lead", shared by
    /// assignment, delivery, notices, approvals, and stop/layout guards: the
    /// bound lead while its terminal exists, otherwise the unique agent named
    /// `polaris`. Read-only; `resolve_orbit_lead` also records a name match as
    /// the new binding.
    pub(crate) fn orbit_lead_terminal(&self) -> Option<TerminalTarget> {
        self.bound_lead_target()
            .or_else(|| self.resolve_agent_target(LEAD_NAME).ok())
    }

    /// Whether `terminal_id` is the Orbit lead. Without a unique lead (no
    /// binding and two agents named `polaris`) every `polaris` counts, so none
    /// is ever managed, stopped, or approved as a worker.
    pub(crate) fn orbit_is_lead_terminal(&self, terminal_id: &str) -> bool {
        match self.orbit_lead_terminal() {
            Some(lead) => lead.terminal_id == terminal_id,
            None => self
                .state
                .terminals
                .get(terminal_id)
                .is_some_and(|terminal| terminal.agent_name.as_deref() == Some(LEAD_NAME)),
        }
    }

    /// `orbit_lead_terminal`, recording a name match as the new binding.
    /// Requires a loaded ledger.
    pub(super) fn resolve_orbit_lead(&mut self) -> Option<TerminalTarget> {
        let target = self.orbit_lead_terminal()?;
        self.record_orbit_lead(target.terminal_id.clone());
        Some(target)
    }

    /// The lead a server notice goes to: the resolved lead, else the last
    /// binding (delivery then waits for that terminal to host its agent).
    pub(super) fn orbit_notice_lead_id(&mut self) -> Option<String> {
        self.resolve_orbit_lead()
            .map(|target| target.terminal_id)
            .or_else(|| self.orbit_notifications.lead_terminal_id.clone())
    }

    /// `{terminal_id, present, state, visible_idle, visible_working,
    /// queued_reports}` for activity.
    pub(super) fn orbit_lead_activity(&mut self) -> Value {
        let terminal_id = self.orbit_notice_lead_id();
        let terminal = terminal_id
            .as_deref()
            .and_then(|id| self.state.terminals.get(id));
        let queued = self
            .orbit_notifications
            .tasks
            .iter()
            .filter(|task| task.delivery == Delivery::Queued)
            .count();
        json!({
            "terminal_id": terminal_id,
            "present": terminal.is_some(),
            "state": terminal.map(|terminal| state_name(terminal.state)),
            "visible_idle": terminal.is_some_and(|terminal| terminal.visible_idle()),
            "visible_working": terminal.is_some_and(|terminal| terminal.visible_working()),
            "queued_reports": queued,
        })
    }
}
