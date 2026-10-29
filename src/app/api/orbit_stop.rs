//! Stopping Orbit workers and closing Orbit panes. Both refuse the lead and
//! anything outside the bound Orbit tab, never close Orbit's last pane, and
//! leave the user's focus where it was.
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::responses::{encode_error, encode_error_body};
use crate::api::schema::{OrbitAgentStopParams, OrbitPaneCloseParams, PaneTarget};
use crate::app::terminal_targets::TerminalTarget;
use crate::app::App;

/// How long the agent's process group has to exit after SIGTERM before SIGKILL.
const AGENT_TERMINATE_GRACE: Duration = Duration::from_millis(1_500);
const AGENT_EXIT_POLL: Duration = Duration::from_millis(50);

const LEAD_PROTECTED: &str = "The Polaris lead cannot be stopped or closed through Orbit";
const MEMBER_KEPT: &str =
    "Team members keep their pane; the team supervisor relaunches the member there";

/// Ends the agent job in the pane's foreground with SIGTERM, then SIGKILL after
/// a grace period, and leaves the pane's shell running. Returns whether a
/// foreground job other than the shell was signalled.
fn terminate_foreground_job(shell_pid: u32) -> bool {
    let Some(job) = crate::detect::foreground_job(shell_pid) else {
        return false;
    };
    if job.processes.iter().any(|process| process.pid == shell_pid) {
        return false;
    }
    let process_group = job.process_group_id;
    let pids: Vec<u32> = job.processes.iter().map(|process| process.pid).collect();
    crate::platform::signal_processes(&pids, crate::platform::Signal::Terminate);
    let escalate = move || {
        let deadline = Instant::now() + AGENT_TERMINATE_GRACE;
        while Instant::now() < deadline {
            if !pids.iter().any(|pid| crate::platform::process_exists(*pid)) {
                return;
            }
            std::thread::sleep(AGENT_EXIT_POLL);
        }
        let survivors = still_in_group(&pids, process_group, crate::platform::process_group_id);
        if survivors.is_empty() {
            return;
        }
        tracing::warn!(?survivors, "orbit worker ignored SIGTERM; killing it");
        crate::platform::signal_processes(&survivors, crate::platform::Signal::Kill);
    };
    if let Err(error) = crate::thread_spawn::spawn_named("aster-orbit-stop", escalate) {
        tracing::warn!(%error, "could not watch the stopped Orbit worker");
    }
    true
}

/// The pids still in the stopped job's process group. An exited worker's pid
/// may be reused by an unrelated process, which must never receive SIGKILL.
fn still_in_group(
    pids: &[u32],
    process_group: u32,
    group_of: impl Fn(u32) -> Option<u32>,
) -> Vec<u32> {
    pids.iter()
        .copied()
        .filter(|pid| group_of(*pid) == Some(process_group))
        .collect()
}

/// Whether the pane's foreground job is the worker: the recorded kind, or any
/// known agent for a worker without a record.
fn foreground_is_worker(
    runtime: &crate::terminal::TerminalRuntime,
    recorded: Option<crate::detect::Agent>,
) -> bool {
    match recorded {
        Some(kind) => crate::app::agents::runtime_hosts_agent(runtime, kind),
        None => crate::app::agents::live_runtime_agent(runtime).is_some(),
    }
}

impl App {
    /// A worker by agent name, terminal id, or pane id, falling back to a
    /// recorded worker name whose agent already exited. Team members are
    /// refused unless `force`; the second value is the member's role.
    fn orbit_stop_target(
        &mut self,
        id: &str,
        name: &str,
        force: bool,
    ) -> Result<(TerminalTarget, Option<String>), String> {
        if name == crate::app::polaris::NAME {
            return Err(encode_error(
                id.into(),
                "orbit_lead_protected",
                LEAD_PROTECTED,
            ));
        }
        let (orbit_ws, orbit_tab) = self
            .resolve_orbit()
            .map_err(|(code, message)| encode_error(id.into(), code, message))?;
        let recorded = self
            .orbit_tab_terminals()
            .into_iter()
            .find(|(terminal_id, _, _)| {
                self.orbit_workers
                    .get(terminal_id)
                    .is_some_and(|record| record.name == name)
            });
        let resolved = self.resolve_agent_target(name).or_else(|original| {
            self.resolve_terminal_target(name)
                .ok()
                .filter(|target| target.terminal_id == name)
                .ok_or(original)
        });
        let target = match resolved {
            Ok(target) => target,
            Err(error) => match recorded {
                Some((terminal_id, ws_idx, pane_id)) => TerminalTarget {
                    ws_idx,
                    tab_idx: orbit_tab,
                    pane_id,
                    terminal_id,
                },
                None => {
                    return Err(encode_error_body(
                        id.into(),
                        self.agent_target_error_body(error),
                    ))
                }
            },
        };
        let member = self
            .orbit_team_member_of_terminal(&target.terminal_id)
            .filter(|(role, _)| {
                self.orbit_role_of_tab(target.ws_idx, target.tab_idx)
                    .as_ref()
                    == Some(role)
            });
        if let Some((role, member)) = &member {
            if !force {
                return Err(encode_error(
                    id.into(),
                    "orbit_team_member_protected",
                    format!(
                        "{member} is a member of team role {role} and is kept alive; retire the role with orbit_team_remove, or stop with force to restart it in its pane"
                    ),
                ));
            }
        } else if target.ws_idx != orbit_ws || target.tab_idx != orbit_tab {
            return Err(encode_error(
                id.into(),
                "orbit_scope_violation",
                "Worker is outside the bound Orbit tab",
            ));
        }
        if self.orbit_is_lead_terminal(&target.terminal_id) {
            return Err(encode_error(
                id.into(),
                "orbit_lead_protected",
                LEAD_PROTECTED,
            ));
        }
        if !self.orbit_workers.contains_key(&target.terminal_id)
            && !self.orbit_terminal_hosts_agent(&target.terminal_id)
        {
            return Err(encode_error(
                id.into(),
                "orbit_not_a_worker",
                "Pane hosts neither an Orbit worker nor a live agent; close it with orbit_pane_close",
            ));
        }
        Ok((target, member.map(|(role, _)| role)))
    }

    /// Assignments awaiting the worker's report block a stop unless `force`
    /// cancels them. Reports already made are never cancelled.
    fn orbit_release_worker_tasks(
        &mut self,
        id: &str,
        worker_id: &str,
        force: bool,
    ) -> Result<Vec<String>, String> {
        let open = self.orbit_open_tasks_for_worker(worker_id);
        if open.is_empty() {
            return Ok(open);
        }
        if !force {
            return Err(encode_error(
                id.into(),
                "orbit_worker_assigned",
                format!(
                    "Worker has assignments awaiting its report: {}; wait for their reports, cancel them, or stop with force",
                    open.join(", ")
                ),
            ));
        }
        Ok(self.orbit_cancel_worker_tasks(worker_id))
    }

    fn orbit_tab_has_one_pane(&self, ws: usize, tab: usize) -> bool {
        self.state.workspaces[ws].tabs[tab].panes.len() <= 1
    }

    /// Ends the worker in `target`: closes its pane, or signals its agent and
    /// clears the label when the pane stays. Returns whether the pane closed and
    /// whether a kept pane's agent was signalled. A foreground job that is not
    /// the worker is never signalled.
    fn stop_orbit_worker(
        &mut self,
        id: &str,
        target: &TerminalTarget,
        pane_id: &str,
        close: bool,
    ) -> Result<(bool, bool), String> {
        let record = self.orbit_workers.remove(&target.terminal_id);
        if close && !self.orbit_tab_has_one_pane(target.ws_idx, target.tab_idx) {
            self.close_pane(
                id.into(),
                &PaneTarget {
                    pane_id: pane_id.into(),
                },
            )?;
            return Ok((true, false));
        }
        let recorded = record
            .as_ref()
            .and_then(|record| crate::detect::parse_agent_label(&record.kind));
        let signalled = self.state.workspaces[target.ws_idx]
            .terminal_id(target.pane_id)
            .and_then(|terminal_id| self.terminal_runtimes.get(terminal_id))
            .filter(|runtime| foreground_is_worker(runtime, recorded))
            .and_then(|runtime| runtime.child_pid())
            .is_some_and(terminate_foreground_job);
        self.set_orbit_pane_label(target.ws_idx, target.pane_id, None);
        Ok((false, signalled))
    }

    /// `orbit.agent_stop`. The socket server finishes `pending` work off the
    /// event loop: waiting for a kept pane's agent to exit, and removing the
    /// worktree. Direct callers see `pending` as returned.
    pub(super) fn handle_orbit_agent_stop(
        &mut self,
        id: String,
        params: OrbitAgentStopParams,
    ) -> String {
        match self.orbit_agent_stop(&id, params) {
            Ok(result) => json!({"id": id, "result": result}).to_string(),
            Err(response) => response,
        }
    }

    fn orbit_agent_stop(
        &mut self,
        id: &str,
        params: OrbitAgentStopParams,
    ) -> Result<Value, String> {
        let mut close = params.close_pane.unwrap_or(true);
        if params.remove_worktree && !close {
            return Err(encode_error(
                id.into(),
                "invalid_params",
                "remove_worktree requires close_pane, so no shell is left inside a removed checkout",
            ));
        }
        // A worker restored after a restart is recorded with its worktree first,
        // so `remove_worktree` still finds it.
        self.recover_orbit_worktree_workers();
        let (target, member_role) = self.orbit_stop_target(id, &params.name, params.force)?;
        // A member is stopped, not removed: its pane and terminal stay for the relaunch.
        if member_role.is_some() {
            close = false;
        }
        let worktree = self
            .orbit_workers
            .get(&target.terminal_id)
            .and_then(|record| record.worktree.clone());
        if params.remove_worktree && worktree.is_none() {
            return Err(encode_error(
                id.into(),
                "orbit_no_worktree",
                "Worker was not started in an Orbit worktree",
            ));
        }
        if params.remove_worktree
            && close
            && self.orbit_tab_has_one_pane(target.ws_idx, target.tab_idx)
        {
            return Err(encode_error(
                id.into(),
                "orbit_last_pane",
                "Orbit's last pane cannot be closed, so its worktree cannot be removed",
            ));
        }
        let pane_id = self
            .public_pane_id(target.ws_idx, target.pane_id)
            .ok_or_else(|| {
                encode_error(id.into(), "orbit_missing", "Worker pane is unavailable")
            })?;
        let cancelled = self.orbit_release_worker_tasks(id, &target.terminal_id, params.force)?;
        let pending_deliveries = self.orbit_pending_deliveries_for_worker(&target.terminal_id);
        let (closed, signalled) = self.stop_orbit_worker(id, &target, &pane_id, close)?;
        tracing::info!(terminal_id = %target.terminal_id, closed, signalled, ?cancelled, ?pending_deliveries, "orbit worker stopped");
        // Tell the lead why a pane it asked to close is still there.
        let kept_reason = if closed {
            None
        } else if member_role.is_some() {
            Some(MEMBER_KEPT)
        } else if !close {
            Some("close_pane was false; the shell was kept")
        } else {
            Some("Orbit's last pane is never closed; the agent was ended and the shell kept")
        };
        let removal = worktree
            .as_ref()
            .filter(|_| params.remove_worktree)
            .map(|worktree| {
                json!({
                    "repo_root": worktree.repo_root,
                    "path": worktree.path,
                    "force": params.discard_changes,
                    "delete_branch": (params.discard_changes && worktree.created_branch)
                        .then(|| worktree.branch.clone())
                        .flatten(),
                })
            });
        Ok(json!({
            "stopped": true,
            "role": member_role,
            "pane_id": pane_id,
            "closed": closed,
            "kept_reason": kept_reason,
            "cancelled_tasks": cancelled,
            "pending_deliveries": pending_deliveries,
            "worktree": worktree,
            "worktree_removed": false,
            "pending": {"await_exit": signalled, "remove_worktree": removal},
        }))
    }

    /// `orbit.pane_close`: closes an Orbit pane; a live worker needs `force`,
    /// which then stops it like `orbit.agent_stop` and cancels its assignments.
    pub(super) fn handle_orbit_pane_close(
        &mut self,
        id: String,
        params: OrbitPaneCloseParams,
    ) -> String {
        match self.orbit_pane_close(&id, params) {
            Ok(result) => json!({"id": id, "result": result}).to_string(),
            Err(response) => response,
        }
    }

    fn orbit_pane_close(
        &mut self,
        id: &str,
        params: OrbitPaneCloseParams,
    ) -> Result<Value, String> {
        let (ws, pane, pane_id) = self
            .resolve_orbit_pane(Some(&params.pane_id))
            .map_err(|(code, message)| encode_error(id.into(), code, message))?;
        let (_, tab) = self
            .resolve_orbit()
            .map_err(|(code, message)| encode_error(id.into(), code, message))?;
        let terminal_id = self.state.workspaces[ws]
            .terminal_id(pane)
            .map(ToString::to_string)
            .ok_or_else(|| {
                encode_error(id.into(), "orbit_missing", "Pane terminal is unavailable")
            })?;
        if self.orbit_is_lead_terminal(&terminal_id) {
            return Err(encode_error(
                id.into(),
                "orbit_lead_protected",
                LEAD_PROTECTED,
            ));
        }
        if self.orbit_tab_has_one_pane(ws, tab) {
            return Err(encode_error(
                id.into(),
                "orbit_last_pane",
                "Orbit's last pane cannot be closed",
            ));
        }
        let live = self.agent_info(ws, pane);
        let cancelled = match &live {
            Some(agent) if !params.force => {
                return Err(encode_error(
                    id.into(),
                    "orbit_worker_live",
                    format!(
                    "Pane hosts live worker {}; stop it with orbit_agent_stop or close with force",
                    agent.name.as_deref().unwrap_or(&agent.terminal_id)
                ),
                ))
            }
            Some(_) => self.orbit_release_worker_tasks(id, &terminal_id, true)?,
            None => Vec::new(),
        };
        let pending_deliveries = self.orbit_pending_deliveries_for_worker(&terminal_id);
        self.orbit_workers.remove(&terminal_id);
        self.close_pane(
            id.into(),
            &PaneTarget {
                pane_id: pane_id.clone(),
            },
        )?;
        Ok(json!({
            "closed": true,
            "pane_id": pane_id,
            "cancelled_tasks": cancelled,
            "pending_deliveries": pending_deliveries,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::super::orbit_lifecycle::tests::{add_worker, exit_worker, orbit_app, orbit_panes};
    use super::*;

    fn call(app: &mut App, method: &str, params: Value) -> Value {
        let request: crate::api::schema::Request =
            serde_json::from_value(json!({"id": "test", "method": method, "params": params}))
                .unwrap();
        serde_json::from_str(&app.handle_api_request(request)).unwrap()
    }

    fn user_focus(app: &App) -> (Option<usize>, Vec<usize>) {
        (
            app.state.active,
            app.state
                .workspaces
                .iter()
                .map(|ws| ws.active_tab)
                .collect(),
        )
    }

    /// Gives the worker a runtime and an open assignment; returns its task id.
    fn assigned(app: &mut App, terminal_id: &str) -> String {
        let (runtime, _queue) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 8);
        let id = app.state.terminals[terminal_id].id.clone();
        app.terminal_runtimes.insert(id, runtime);
        let response = call(
            app,
            "orbit.agent_assign",
            json!({"name": terminal_id, "prompt": "Review only"}),
        );
        response["result"]["task_id"]
            .as_str()
            .unwrap_or_else(|| panic!("{response}"))
            .to_owned()
    }

    /// Reports `task_id` as the worker, leaving the report queued for the lead.
    fn report_as_worker(app: &mut App, terminal_id: &str, task_id: &str) {
        let response = call(
            app,
            "orbit.agent_report",
            json!({
                "task_id": task_id,
                "worker_id": terminal_id,
                "token": app.orbit_last_issued_token(),
                "outcome": "completed",
                "summary": "done",
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
    }

    fn delivery(app: &mut App, task_id: &str) -> Value {
        call(app, "orbit.task_status", json!({"task_id": task_id}))["result"]["task"]["delivery"]
            .clone()
    }

    #[tokio::test]
    async fn stop_after_the_worker_reported_keeps_the_report_delivering() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", None);
        let task_id = assigned(&mut app, &worker);
        report_as_worker(&mut app, &worker, &task_id);
        assert_eq!(delivery(&mut app, &task_id), "queued");
        // No force needed: a report already made is not open work.
        let response = call(
            &mut app,
            "orbit.agent_stop",
            json!({"name": "reviewer", "close_pane": false}),
        );
        let result = &response["result"];
        assert_eq!(result["stopped"], true, "{response}");
        assert_eq!(result["cancelled_tasks"], json!([]));
        assert_eq!(result["pending_deliveries"], json!([task_id]));
        assert_eq!(delivery(&mut app, &task_id), "queued");

        // force cancels only assignments still awaiting a report.
        let worker = add_worker(&mut app, 1, "reviewer", None);
        let second = assigned(&mut app, &worker);
        report_as_worker(&mut app, &worker, &second);
        let third = {
            let response = call(
                &mut app,
                "orbit.agent_assign",
                json!({"name": "reviewer", "prompt": "Another"}),
            );
            response["result"]["task_id"].as_str().unwrap().to_owned()
        };
        let response = call(
            &mut app,
            "orbit.agent_stop",
            json!({"name": "reviewer", "force": true}),
        );
        let result = &response["result"];
        assert_eq!(result["cancelled_tasks"], json!([third]), "{response}");
        let pending = result["pending_deliveries"].as_array().unwrap();
        assert!(pending.contains(&json!(task_id)) && pending.contains(&json!(second)));
        assert_eq!(delivery(&mut app, &second), "queued");
        assert_eq!(delivery(&mut app, &third), "cancelled");
    }

    #[test]
    fn worktree_removal_discards_changes_only_with_discard_changes() {
        for (params, discards) in [
            (json!({"remove_worktree": true, "force": true}), false),
            (
                json!({"remove_worktree": true, "discard_changes": true}),
                true,
            ),
        ] {
            let mut app = orbit_app();
            let worker = add_worker(&mut app, 1, "editor", None);
            app.orbit_workers.get_mut(&worker).unwrap().worktree =
                Some(crate::app::api::orbit_lifecycle::OrbitWorktree {
                    path: "/tmp/wt/editor".into(),
                    branch: Some("b".into()),
                    repo_root: "/tmp/repo".into(),
                    recovered: false,
                    created_branch: false,
                });
            let mut params = params;
            params["name"] = json!("editor");
            let response = call(&mut app, "orbit.agent_stop", params);
            let removal = &response["result"]["pending"]["remove_worktree"];
            assert_eq!(removal["force"], discards, "{response}");
        }
    }

    #[test]
    fn stop_refuses_a_pane_without_a_worker_or_live_agent() {
        let mut app = orbit_app();
        let (_, _, shell) = orbit_panes(&mut app)[1].clone();
        let response = call(&mut app, "orbit.agent_stop", json!({"name": shell}));
        assert_eq!(
            response["error"]["code"], "orbit_not_a_worker",
            "{response}"
        );
        assert_eq!(orbit_panes(&mut app).len(), 2);
    }

    #[test]
    fn sigkill_escalation_skips_pids_that_left_the_process_group() {
        let groups = |pid: u32| match pid {
            10 | 11 => Some(10),
            12 => Some(99),
            _ => None,
        };
        assert_eq!(still_in_group(&[10, 11, 12, 13], 10, groups), vec![10, 11]);
        assert!(still_in_group(&[12, 13], 10, groups).is_empty());
    }

    #[test]
    fn stop_refuses_the_lead_and_workers_outside_orbit() {
        let mut app = orbit_app();
        let lead = app
            .orbit_notifications
            .lead_terminal_id()
            .unwrap()
            .to_owned();
        for name in ["polaris", lead.as_str()] {
            let response = call(&mut app, "orbit.agent_stop", json!({"name": name}));
            let code = &response["error"]["code"];
            assert!(
                code == "orbit_lead_protected" || code == "orbit_scope_violation",
                "{response}"
            );
        }
        let outside = app.state.workspaces[1]
            .terminal_id(app.state.workspaces[1].tabs[0].root_pane)
            .unwrap()
            .clone();
        let terminal = app.state.terminals.get_mut(&outside).unwrap();
        terminal.restore_managed_agent("outsider".into(), crate::detect::Agent::Claude);
        let response = call(&mut app, "orbit.agent_stop", json!({"name": "outsider"}));
        assert_eq!(
            response["error"]["code"], "orbit_scope_violation",
            "{response}"
        );
        // A lead moved into Orbit is still refused.
        let worker = add_worker(&mut app, 1, "reviewer", None);
        app.bind_orbit_lead(worker);
        let response = call(&mut app, "orbit.agent_stop", json!({"name": "reviewer"}));
        assert_eq!(
            response["error"]["code"], "orbit_lead_protected",
            "{response}"
        );
    }

    #[tokio::test]
    async fn stop_refuses_open_assignment_then_force_cancels_and_closes() {
        let mut app = orbit_app();
        let before = user_focus(&app);
        let worker = add_worker(&mut app, 1, "reviewer", None);
        let task_id = assigned(&mut app, &worker);
        let response = call(&mut app, "orbit.agent_stop", json!({"name": "reviewer"}));
        assert_eq!(
            response["error"]["code"], "orbit_worker_assigned",
            "{response}"
        );
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&task_id));
        assert_eq!(orbit_panes(&mut app).len(), 2);
        let response = call(
            &mut app,
            "orbit.agent_stop",
            json!({"name": "reviewer", "force": true}),
        );
        let result = &response["result"];
        assert_eq!(result["stopped"], true, "{response}");
        assert_eq!(result["closed"], true);
        assert_eq!(result["cancelled_tasks"], json!([task_id]));
        assert_eq!(result["pending"]["await_exit"], false);
        assert_eq!(orbit_panes(&mut app).len(), 1);
        let status = call(&mut app, "orbit.task_status", json!({"task_id": task_id}));
        assert_eq!(
            status["result"]["task"]["delivery"], "cancelled",
            "{status}"
        );
        assert!(!app.orbit_workers.contains_key(&worker));
        assert_eq!(user_focus(&app), before);
    }

    #[test]
    fn stop_keeping_the_pane_clears_its_label() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", None);
        let response = call(
            &mut app,
            "orbit.agent_stop",
            json!({"name": "reviewer", "close_pane": false}),
        );
        assert_eq!(response["result"]["closed"], false, "{response}");
        assert!(response["result"]["kept_reason"]
            .as_str()
            .unwrap()
            .contains("close_pane was false"));
        // The test pane has no foreground job, so nothing was signalled to wait for.
        assert_eq!(response["result"]["pending"]["await_exit"], false);
        assert_eq!(orbit_panes(&mut app).len(), 2);
        assert_eq!(app.state.terminals[worker.as_str()].manual_label, None);
        assert!(!app.orbit_workers.contains_key(&worker));
    }

    #[test]
    fn stop_never_closes_orbits_last_pane_and_checks_worktree_options() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", None);
        let response = call(
            &mut app,
            "orbit.agent_stop",
            json!({"name": "reviewer", "close_pane": false, "remove_worktree": true}),
        );
        assert_eq!(response["error"]["code"], "invalid_params", "{response}");
        let response = call(
            &mut app,
            "orbit.agent_stop",
            json!({"name": "reviewer", "remove_worktree": true}),
        );
        assert_eq!(response["error"]["code"], "orbit_no_worktree", "{response}");
        let (_, root, _) = orbit_panes(&mut app)[0].clone();
        assert_eq!(
            call(&mut app, "orbit.pane_close", json!({"pane_id": root}))["result"]["closed"],
            true
        );
        let response = call(&mut app, "orbit.agent_stop", json!({"name": "reviewer"}));
        assert_eq!(response["result"]["stopped"], true, "{response}");
        assert_eq!(response["result"]["closed"], false);
        assert!(response["result"]["kept_reason"]
            .as_str()
            .unwrap()
            .contains("last pane"));
        assert_eq!(orbit_panes(&mut app).len(), 1);
        assert_eq!(app.state.terminals[worker.as_str()].manual_label, None);
    }

    #[tokio::test]
    async fn pane_close_refuses_live_worker_unless_forced() {
        let mut app = orbit_app();
        let before = user_focus(&app);
        let worker = add_worker(&mut app, 1, "reviewer", None);
        let task_id = assigned(&mut app, &worker);
        let (_, pane_id, _) = orbit_panes(&mut app)[1].clone();
        let response = call(&mut app, "orbit.pane_close", json!({"pane_id": pane_id}));
        assert_eq!(response["error"]["code"], "orbit_worker_live", "{response}");
        let response = call(
            &mut app,
            "orbit.pane_close",
            json!({"pane_id": pane_id, "force": true}),
        );
        assert_eq!(response["result"]["closed"], true, "{response}");
        assert_eq!(response["result"]["cancelled_tasks"], json!([task_id]));
        assert_eq!(orbit_panes(&mut app).len(), 1);
        let (_, last, _) = orbit_panes(&mut app)[0].clone();
        let response = call(&mut app, "orbit.pane_close", json!({"pane_id": last}));
        assert_eq!(response["error"]["code"], "orbit_last_pane", "{response}");
        assert_eq!(user_focus(&app), before);
    }

    #[test]
    fn stale_pane_closes_without_force() {
        let mut app = orbit_app();
        let worker = add_worker(&mut app, 1, "reviewer", None);
        exit_worker(&mut app, &worker);
        let stale = app.orbit_stale_panes();
        let pane_id = stale[0]["pane_id"].as_str().unwrap().to_owned();
        let response = call(&mut app, "orbit.pane_close", json!({"pane_id": pane_id}));
        assert_eq!(response["result"]["closed"], true, "{response}");
        assert!(app.orbit_stale_panes().is_empty());
    }
}
