//! `orbit.agent_approve` and `orbit.agent_deny`: Polaris answers a blocked Orbit
//! worker's prompt, only when `[polaris.approvals] lead_may_approve` allows it.
//! Every refusal is checked before any key reaches the worker.
use std::sync::mpsc::Sender;
use std::time::Instant;

use crate::api::schema::{Method, OrbitAgentApprovalParams, ReadFormat, ReadSource, Request};
use crate::app::orbit_notifications::{
    answer_keys, permission_prompt_rules, Decision, Waiter, ANSWER_WAIT,
};
use crate::app::App;
use crate::detect::AgentState;

use super::responses::{encode_error, encode_error_body};

/// Detection lines read for a decision about a worker's prompt.
const DECISION_SCREEN_LINES: u32 = 40;
/// Request id used in refusals of an approval the decision layer sends.
const DECISION_REQUEST_ID: &str = "orbit-decision";

impl App {
    /// Sends the answer, then replies `{answered, state}` once the worker leaves
    /// `Blocked` or `ANSWER_WAIT` passes. Returns false for other methods.
    pub(crate) fn handle_deferred_orbit_approval(
        &mut self,
        request: Request,
        respond_to: Sender<String>,
    ) -> bool {
        let (params, decision) = match request.method {
            Method::OrbitAgentApprove(params) => (params, Decision::Approve),
            Method::OrbitAgentDeny(params) => (params, Decision::Deny),
            _ => return false,
        };
        match self.answer_orbit_worker(&request.id, &params, decision) {
            Ok(terminal_id) => self.await_orbit_answer(Waiter {
                id: request.id,
                respond_to,
                terminal_id,
                deadline: Instant::now() + ANSWER_WAIT,
            }),
            Err(response) => {
                let _ = respond_to.send(response);
            }
        }
        true
    }

    /// Whether `name` resolves to the bound lead or the agent named `polaris`.
    fn is_orbit_lead_target(&self, name: &str) -> bool {
        let Ok(target) = self
            .resolve_agent_target(name)
            .or_else(|_| self.resolve_terminal_target(name))
        else {
            return false;
        };
        self.orbit_is_lead_terminal(&target.terminal_id)
    }

    /// The blocked-screen rule of the pane's agent, from its detection screen.
    fn blocked_rule(
        &self,
        agent: crate::detect::Agent,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<String> {
        let (runtime, _) = self.lookup_runtime(ws_idx, pane_id)?;
        let (screen, osc_title, osc_progress) = (
            runtime.detection_text(),
            runtime.agent_osc_title(),
            runtime.agent_osc_progress(),
        );
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        explain.blocked_screen_rule().map(str::to_owned)
    }

    /// The worker is still `Blocked` on a permission prompt that the approval
    /// keys answer, judged from its current detection screen.
    fn check_permission_prompt(
        &self,
        id: &str,
        pane: &str,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Result<(), String> {
        let terminal = self.state.workspaces[ws_idx]
            .terminal_id(pane_id)
            .and_then(|terminal_id| self.state.terminals.get(terminal_id));
        let agent = terminal.and_then(|terminal| terminal.effective_known_agent());
        let (Some(terminal), Some(agent)) = (terminal, agent) else {
            return Err(encode_error(
                id.into(),
                "orbit_missing",
                "Worker terminal is unavailable",
            ));
        };
        if terminal.state != AgentState::Blocked {
            return Err(encode_error(
                id.into(),
                "orbit_no_pending_approval",
                format!("no pending approval: the worker in pane {pane} is not blocked"),
            ));
        }
        match self.blocked_rule(agent, ws_idx, pane_id) {
            Some(rule) if permission_prompt_rules(agent).contains(&rule.as_str()) => Ok(()),
            rule => Err(encode_error(
                id.into(),
                "orbit_approval_unsupported_prompt",
                format!(
                    "The worker in pane {pane} is blocked on a {} dialog, not a permission prompt; ask the user to answer in its pane",
                    rule.as_deref().unwrap_or("unrecognised")
                ),
            )),
        }
    }

    /// The worker's agent while it is `Blocked` on a permission prompt the
    /// approval keys answer; `None` otherwise.
    pub(crate) fn orbit_permission_prompt_agent(
        &self,
        terminal_id: &str,
    ) -> Option<crate::detect::Agent> {
        let target = self
            .resolve_terminal_target(terminal_id)
            .ok()
            .filter(|target| target.terminal_id == terminal_id)?;
        let terminal = self.state.terminals.get(terminal_id)?;
        let agent = terminal.effective_known_agent()?;
        if terminal.state != AgentState::Blocked || answer_keys(agent, Decision::Approve).is_none()
        {
            return None;
        }
        let rule = self.blocked_rule(agent, target.ws_idx, target.pane_id)?;
        permission_prompt_rules(agent)
            .contains(&rule.as_str())
            .then_some(agent)
    }

    /// The bottom of the worker's detection screen, as plain text.
    pub(crate) fn orbit_worker_detection_screen(&self, terminal_id: &str) -> Option<String> {
        let target = self.resolve_terminal_target(terminal_id).ok()?;
        let (runtime, _) = self.lookup_runtime(target.ws_idx, target.pane_id)?;
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            runtime,
            ReadSource::Detection,
            ReadFormat::Text,
            Some(DECISION_SCREEN_LINES),
        );
        Some(snapshot.text)
    }

    /// Approves for the decision layer, only while the worker still shows the
    /// prompt `command` was read from, through the same checks as
    /// `orbit_agent_approve`. `Err` is a short reason without screen text.
    pub(crate) fn auto_approve_orbit_worker(
        &mut self,
        terminal_id: &str,
        command: Option<&str>,
    ) -> Result<(), String> {
        let current = self
            .orbit_worker_detection_screen(terminal_id)
            .and_then(|screen| crate::app::orbit_decisions::rules::command_text(&screen));
        if current.as_deref() != command {
            return Err("the prompt changed".into());
        }
        let params = OrbitAgentApprovalParams {
            name: terminal_id.to_owned(),
        };
        self.answer_orbit_worker(DECISION_REQUEST_ID, &params, Decision::Approve)
            .map(|_| ())
            .map_err(|response| {
                serde_json::from_str::<serde_json::Value>(&response)
                    .ok()
                    .and_then(|value| value["error"]["code"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| "refused".into())
            })
    }

    /// The worker's terminal id after its answer keys were sent.
    fn answer_orbit_worker(
        &mut self,
        id: &str,
        params: &OrbitAgentApprovalParams,
        decision: Decision,
    ) -> Result<String, String> {
        if self.is_orbit_lead_target(&params.name) {
            return Err(encode_error(
                id.into(),
                "orbit_lead_refused",
                "Polaris cannot answer its own prompts; the user answers in the Polaris pane",
            ));
        }
        let pane = self.orbit_worker_target(id, &params.name)?;
        let resolved = self
            .resolve_agent_target(&pane)
            .map_err(|error| encode_error_body(id.into(), self.agent_target_error_body(error)))?;
        let missing = || encode_error(id.into(), "orbit_missing", "Worker terminal is unavailable");
        let terminal_id = self.state.workspaces[resolved.ws_idx]
            .terminal_id(resolved.pane_id)
            .map(ToString::to_string)
            .ok_or_else(missing)?;
        let terminal = self
            .state
            .terminals
            .get(terminal_id.as_str())
            .ok_or_else(missing)?;
        if terminal.state != AgentState::Blocked {
            return Err(encode_error(
                id.into(),
                "orbit_no_pending_approval",
                format!("no pending approval: the worker in pane {pane} is not blocked"),
            ));
        }
        if !self.orbit_lead_may_approve() {
            return Err(encode_error(
                id.into(),
                "orbit_approval_not_allowed",
                format!(
                    "Polaris may not answer worker prompts ([polaris.approvals] lead_may_approve = false). Ask the user to answer in the worker's pane {pane}."
                ),
            ));
        }
        let agent = terminal.effective_known_agent();
        let keys = agent
            .and_then(|agent| answer_keys(agent, decision))
            .ok_or_else(|| {
                let kind = agent.map_or("unknown", crate::detect::agent_label);
                encode_error(
                    id.into(),
                    "orbit_approval_unsupported",
                    format!("No known approval keys for {kind} workers; ask the user to answer in pane {pane}"),
                )
            })?;
        let keys: Vec<String> = keys.iter().map(|key| (*key).to_owned()).collect();
        // Checked last, on the event loop, right before the keys go out.
        self.check_permission_prompt(id, &pane, resolved.ws_idx, resolved.pane_id)?;
        self.send_agent_keys(id, &pane, &keys)?;
        let task_id = self.orbit_open_task_id(&terminal_id);
        tracing::info!(%terminal_id, task_id = ?task_id, ?decision, "orbit worker prompt answered for Polaris");
        Ok(terminal_id)
    }
}
